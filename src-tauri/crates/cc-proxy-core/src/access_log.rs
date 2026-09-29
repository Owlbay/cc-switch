//! 访问日志（设计文档 §8）。
//!
//! 每个请求恰好一行，target 固定为 [`ACCESS_TARGET`]，级别统一为 info，用 `outcome` 区分结果：
//!
//! | outcome | 含义 |
//! |---|---|
//! | `complete` | 响应完整发出（含中转自身的错误响应、HEAD / 204 / 304 等无 body 响应） |
//! | `error` | 响应开始后上游出错或空闲超时，客户端连接被中断 |
//! | `aborted` | 响应开始后客户端断开 |
//! | `cancelled` | 响应开始前客户端断开（请求 future 被 drop） |
//!
//! **不记录** 请求 / 响应 body、任何 Key 或 token；path 不含 query。

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use http::{Method, Response};
use http_body::{Body, Frame, SizeHint};

use crate::error::{BoxError, RelayBody};
use crate::interface::Interface;

/// 访问日志的 tracing target
pub const ACCESS_TARGET: &str = "cc_proxy_core::access";

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// 一个请求的访问日志记录。未写出就被 drop 时记为 `cancelled`。
pub struct AccessLog {
    request_id: String,
    method: Method,
    path: String,
    started: Instant,
    interface: Option<Interface>,
    upstream: Option<String>,
    attempts: u32,
    head_elapsed: Option<Duration>,
    request_bytes: u64,
    status: Option<u16>,
    convert: Option<(Interface, Interface)>,
    streaming: bool,
    emitted: bool,
}

impl AccessLog {
    /// `path` 必须是不含 query 的 path。
    pub fn new(method: &Method, path: &str) -> Self {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            request_id: format!("r-{id:x}"),
            method: method.clone(),
            path: path.to_string(),
            started: Instant::now(),
            interface: None,
            upstream: None,
            attempts: 0,
            head_elapsed: None,
            request_bytes: 0,
            status: None,
            convert: None,
            streaming: false,
            emitted: false,
        }
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn set_interface(&mut self, interface: Interface) {
        self.interface = Some(interface);
    }

    pub fn set_request_bytes(&mut self, bytes: usize) {
        self.request_bytes = bytes as u64;
    }

    /// 开始一次上游尝试；`convert` 为该次尝试的转换方向（客户端协议 → 上游协议），透传为 None
    pub fn start_attempt(&mut self, upstream: &str, convert: Option<(Interface, Interface)>) {
        self.attempts += 1;
        self.upstream = Some(upstream.to_string());
        self.convert = convert;
    }

    /// 收到上游响应头
    pub fn head_received(&mut self) {
        self.head_elapsed = Some(self.started.elapsed());
    }

    /// 本次响应是流式透传的上游 body
    pub fn set_streaming(&mut self) {
        self.streaming = true;
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// 对固定 body 的响应（中转自身响应、回放的缓冲响应、HEAD / 204 / 304 等）：立即写出一行。
    pub fn finish(mut self, response: Response<RelayBody>) -> Response<RelayBody> {
        self.status = Some(response.status().as_u16());
        // HEAD 响应不发送 body
        let bytes = if self.method == Method::HEAD {
            0
        } else {
            response.body().size_hint().exact().unwrap_or(0)
        };
        self.emit("complete", bytes, None);
        response
    }

    /// 对流式响应：用 [`LoggedBody`] 包装，在流结束、出错或被 drop 时写出一行。
    pub fn stream(mut self, response: Response<RelayBody>) -> Response<RelayBody> {
        self.status = Some(response.status().as_u16());
        if response.body().is_end_stream() {
            return self.finish(response);
        }
        response.map(|body| http_body_util::BodyExt::boxed(LoggedBody::new(body, self)))
    }

    fn emit(&mut self, outcome: &str, response_bytes: u64, error: Option<&str>) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        let head_ms = self.head_elapsed.map(|d| d.as_millis() as u64);
        let convert = self
            .convert
            .map(|(from, to)| format!("{}->{}", from.as_str(), to.as_str()));
        tracing::info!(
            target: ACCESS_TARGET,
            request_id = %self.request_id,
            method = %self.method,
            path = %self.path,
            interface = self.interface.map(|i| i.as_str()).unwrap_or("-"),
            upstream = self.upstream.as_deref().unwrap_or("-"),
            convert = convert.as_deref().unwrap_or("-"),
            attempts = self.attempts,
            status = self.status.unwrap_or(0),
            head_ms = head_ms.unwrap_or(0),
            total_ms = self.started.elapsed().as_millis() as u64,
            request_bytes = self.request_bytes,
            response_bytes,
            outcome,
            error = error.unwrap_or("-"),
            "relay request"
        );
    }
}

impl Drop for AccessLog {
    fn drop(&mut self) {
        // 请求 future 在写出响应前被 drop：客户端在收到响应头之前断开
        self.emit("cancelled", 0, None);
    }
}

/// 统计响应字节数，并在流结束 / 出错 / 被 drop 时写出访问日志
pub struct LoggedBody<B> {
    inner: B,
    log: Option<AccessLog>,
    bytes: u64,
}

impl<B> LoggedBody<B> {
    pub fn new(inner: B, log: AccessLog) -> Self {
        Self {
            inner,
            log: Some(log),
            bytes: 0,
        }
    }
}

impl<B> Body for LoggedBody<B>
where
    B: Body<Data = Bytes, Error = BoxError> + Unpin,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes += data.remaining() as u64;
                }
                // hyper 在两种情况下读完最后一帧后直接 drop body、不再 poll 出 None：
                // 已知长度的 body 读到最后一帧（end_stream），以及读到 trailers 帧。
                if frame.is_trailers() || this.inner.is_end_stream() {
                    if let Some(mut log) = this.log.take() {
                        log.emit("complete", this.bytes, None);
                    }
                }
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(mut log) = this.log.take() {
                    log.emit("error", this.bytes, Some(&error.to_string()));
                }
            }
            Poll::Ready(None) => {
                if let Some(mut log) = this.log.take() {
                    log.emit("complete", this.bytes, None);
                }
            }
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for LoggedBody<B> {
    fn drop(&mut self) {
        if let Some(mut log) = self.log.take() {
            // 无 body 的响应（HEAD / 204 / 304）已在 `AccessLog::stream` 中按 is_end_stream 分流，
            // 读完最后一帧的情况已在 poll_frame 中记为 complete；这里剩下的都是中途断开。
            log.emit("aborted", self.bytes, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_unique_and_prefixed() {
        let a = AccessLog::new(&Method::GET, "/a");
        let b = AccessLog::new(&Method::GET, "/b");
        assert!(a.request_id().starts_with("r-"));
        assert_ne!(a.request_id(), b.request_id());
    }
}
