//! 协议转换钩子（`docs/protocol-conversion-design-zh.md` §8）。
//!
//! core 只定义 trait 与管道，不实现任何具体协议：没有配置转换的上游仍逐字节透传。
//! 具体转换器由独立的 crate 实现，通过 [`crate::Relay::with_converter`] 注入。

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use http_body::{Body, Frame, SizeHint};

use crate::error::BoxError;
use crate::interface::Interface;

/// 模型名映射：客户端请求的模型 → 上游模型；`*` 为兜底。
pub type ModelMap = BTreeMap<String, String>;

/// 按 `model_map` 取上游模型名：精确匹配 → `*` → 原名。
pub fn map_model<'a>(model_map: &'a ModelMap, model: &'a str) -> &'a str {
    model_map
        .get(model)
        .or_else(|| model_map.get("*"))
        .map(String::as_str)
        .unwrap_or(model)
}

/// 协议转换器。实现必须是无状态或内部同步的，可在多个请求间共享。
pub trait Converter: Send + Sync {
    /// 构造期检查：客户端协议 `client` 到上游协议 `upstream` 是否可转换（含同协议恒等转换）。
    fn supports(&self, client: Interface, upstream: Interface) -> bool;

    /// 把客户端协议的请求转为上游协议。每次尝试调用一次（不同上游可能协议不同）。
    fn convert_request(
        &self,
        ctx: &ConversionContext<'_>,
        request: &InboundRequest<'_>,
    ) -> Result<OutboundRequest, ConvertError>;

    /// 为最终要交给客户端的上游响应创建响应转换器（每次上游响应一个）。
    fn response_converter(
        &self,
        ctx: &ConversionContext<'_>,
        meta: &OutboundMeta,
    ) -> Box<dyn ResponseConverter>;
}

/// 一次转换的方向与参数
#[derive(Debug, Clone, Copy)]
pub struct ConversionContext<'a> {
    /// 客户端协议（即入站接口）
    pub client: Interface,
    /// 上游协议
    pub upstream: Interface,
    pub model_map: &'a ModelMap,
    /// 客户端没有指定输出上限时使用
    pub default_max_output_tokens: u64,
}

/// 转换器看到的入站请求（入站凭据已由 core 在构造上游请求时移除，不需要转换器处理）
#[derive(Debug, Clone, Copy)]
pub struct InboundRequest<'a> {
    pub method: &'a Method,
    /// 不含 query 的 path
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub headers: &'a HeaderMap,
    pub body: &'a Bytes,
}

/// 转换后的上游请求。core 会在此基础上按透传边界处理头（去 hop-by-hop 与入站凭据、
/// 写 host 与上游鉴权），并按上游的 `base_url` / `strip_prefix` 拼接 path。
#[derive(Debug, Clone)]
pub struct OutboundRequest {
    pub method: Method,
    /// 目标协议的 path（不含 query），如 `/v1/chat/completions`
    pub path: String,
    pub query: Option<String>,
    /// 转换器应删除 `accept-encoding` 与 `content-length`，并调整协议专有头
    pub headers: HeaderMap,
    pub body: Bytes,
    pub meta: OutboundMeta,
}

/// 响应转换需要的请求侧信息
#[derive(Debug, Clone, Default)]
pub struct OutboundMeta {
    /// 上游响应是否为流式
    pub stream: bool,
    /// 回填到客户端响应中的模型名
    pub client_model: String,
    pub upstream_model: String,
    /// 请求中的工具名（部分协议需要按名字回填调用 id）
    pub tool_names: Vec<String>,
    /// 客户端以自定义（非 function）形式声明的工具名，响应方向需还原为自定义工具调用
    pub custom_tool_names: Vec<String>,
    /// 被映射为上游内置工具的客户端 server tool 名，响应方向需映射回去
    pub hosted_web_search: Option<String>,
}

/// 响应转换器：每次上游响应一个实例，可以有状态（流式状态机）。
/// 需要 `Sync` 是因为响应 body 统一为可跨线程共享的 `BoxBody`；状态只被单个 body 独占访问。
pub trait ResponseConverter: Send + Sync {
    /// 生成发给客户端的响应头（core 之后仍会去掉 hop-by-hop）
    fn convert_head(&mut self, status: StatusCode, headers: &HeaderMap) -> HeaderMap;

    /// 流式：喂入上游 body 字节，返回要发给客户端的字节（可为空）
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ConvertError>;

    /// 上游 body 结束；`error` 为 Some 表示异常结束，转换器应补发目标协议的错误帧
    fn finish(&mut self, error: Option<&(dyn std::error::Error + 'static)>) -> Vec<Bytes>;

    /// 非流式：整段上游 body（含非 2xx 错误 body）→ 整段客户端 body
    fn convert_full(&mut self, status: StatusCode, body: &[u8]) -> Result<Bytes, ConvertError>;
}

/// 转换失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    /// 目标协议无法表达该请求：跳过该上游（不计熔断、不计尝试次数）
    #[error("conversion unsupported: {0}")]
    Unsupported(String),
    /// 转换器自身错误：立即返回 500，不再尝试
    #[error("conversion error: {0}")]
    Internal(String),
    /// 响应阶段：上游 body 无法解析等
    #[error("response conversion error: {0}")]
    Response(String),
}

/// 流式转换 body：把上游 body 逐块交给 [`ResponseConverter::feed`]，结束时调用 `finish`。
///
/// - `size_hint` 恒为未知：输出长度与上游不同，不能沿用上游的 `content-length`。
/// - `is_end_stream` 在输出缓冲排空且上游结束之前恒为 false，保证 `finish` 的输出不丢。
/// - 上游 trailers 丢弃；上游或转换出错时，先发出 `finish(Some(err))` 的帧，再返回错误，
///   使客户端连接被中断（与透传的提交点语义一致）。
pub struct ConvertingBody<B> {
    inner: B,
    converter: Box<dyn ResponseConverter>,
    pending: VecDeque<Bytes>,
    inner_done: bool,
    error: Option<BoxError>,
    /// 返回错误前是否已让出过一次（见 `poll_frame`）
    yielded_before_error: bool,
}

impl<B> ConvertingBody<B> {
    pub fn new(inner: B, converter: Box<dyn ResponseConverter>) -> Self {
        Self {
            inner,
            converter,
            pending: VecDeque::new(),
            inner_done: false,
            error: None,
            yielded_before_error: false,
        }
    }
}

impl<B> Body for ConvertingBody<B>
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
        loop {
            if let Some(bytes) = this.pending.pop_front() {
                if bytes.is_empty() {
                    continue;
                }
                return Poll::Ready(Some(Ok(Frame::data(bytes))));
            }
            if this.inner_done {
                if this.error.is_some() && !this.yielded_before_error {
                    // 错误帧刚交给 hyper、还在写缓冲里：若同一轮就返回 Err，hyper 会直接断开
                    // 连接而丢掉它们。先让出一次，hyper 的连接循环会在 body Pending 时 flush，
                    // 下一轮再返回 Err 中断客户端连接。尽力而为：若这次 flush 遇到 socket
                    // 背压未写完，剩余的错误帧仍可能丢失（错误帧很小，实际影响可忽略）。
                    this.yielded_before_error = true;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                return Poll::Ready(this.error.take().map(Err));
            }
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(frame))) => {
                    // trailers 等非数据帧丢弃
                    if let Ok(data) = frame.into_data() {
                        match this.converter.feed(&data) {
                            Ok(out) => this.pending.extend(out),
                            Err(error) => {
                                let error: BoxError = Box::new(error);
                                this.pending
                                    .extend(this.converter.finish(Some(error.as_ref())));
                                this.error = Some(error);
                                this.inner_done = true;
                            }
                        }
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    this.pending
                        .extend(this.converter.finish(Some(error.as_ref())));
                    this.error = Some(error);
                    this.inner_done = true;
                }
                Poll::Ready(None) => {
                    this.pending.extend(this.converter.finish(None));
                    this.inner_done = true;
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner_done && self.pending.is_empty() && self.error.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn model_map_lookup() {
        let mut map = ModelMap::new();
        assert_eq!(map_model(&map, "a"), "a");
        map.insert("a".into(), "x".into());
        assert_eq!(map_model(&map, "a"), "x");
        assert_eq!(map_model(&map, "b"), "b");
        map.insert("*".into(), "y".into());
        assert_eq!(map_model(&map, "b"), "y");
        assert_eq!(map_model(&map, "a"), "x");
    }

    /// 把每块转成大写，结束时追加 `END`；出错时追加 `ERR`
    struct Upper;
    impl ResponseConverter for Upper {
        fn convert_head(&mut self, _: StatusCode, headers: &HeaderMap) -> HeaderMap {
            headers.clone()
        }
        fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ConvertError> {
            if chunk == b"bad" {
                return Err(ConvertError::Response("bad chunk".into()));
            }
            Ok(vec![Bytes::from(chunk.to_ascii_uppercase())])
        }
        fn finish(&mut self, error: Option<&(dyn std::error::Error + 'static)>) -> Vec<Bytes> {
            vec![Bytes::from_static(if error.is_some() {
                b"ERR"
            } else {
                b"END"
            })]
        }
        fn convert_full(&mut self, _: StatusCode, body: &[u8]) -> Result<Bytes, ConvertError> {
            Ok(Bytes::from(body.to_ascii_uppercase()))
        }
    }

    type TestFrame = Result<Frame<Bytes>, BoxError>;

    /// 按顺序吐出预设帧的 body
    struct Frames(VecDeque<TestFrame>);

    impl Body for Frames {
        type Data = Bytes;
        type Error = BoxError;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    fn body_of(frames: Vec<TestFrame>) -> Frames {
        Frames(frames.into())
    }

    #[tokio::test]
    async fn converts_every_chunk_and_flushes_finish() {
        let inner = body_of(vec![
            Ok(Frame::data(Bytes::from("ab"))),
            Ok(Frame::trailers(HeaderMap::new())),
            Ok(Frame::data(Bytes::from("c"))),
        ]);
        let body = ConvertingBody::new(inner, Box::new(Upper));
        assert!(!body.is_end_stream());
        assert_eq!(body.size_hint().exact(), None);
        let out = body.collect().await.unwrap().to_bytes();
        assert_eq!(out, "ABCEND");
    }

    #[tokio::test]
    async fn inner_error_emits_error_frames_then_fails() {
        let inner = body_of(vec![
            Ok(Frame::data(Bytes::from("ab"))),
            Err("upstream broke".into()),
        ]);
        let mut body = ConvertingBody::new(inner, Box::new(Upper));
        let mut data = Vec::new();
        let error = loop {
            match body.frame().await {
                Some(Ok(frame)) => data.extend_from_slice(&frame.into_data().unwrap()),
                Some(Err(error)) => break error,
                None => panic!("must end with an error"),
            }
        };
        assert_eq!(data, b"ABERR");
        assert_eq!(error.to_string(), "upstream broke");
    }

    #[tokio::test]
    async fn feed_error_emits_error_frames_then_fails() {
        let inner = body_of(vec![
            Ok(Frame::data(Bytes::from("ok"))),
            Ok(Frame::data(Bytes::from("bad"))),
            Ok(Frame::data(Bytes::from("never"))),
        ]);
        let mut body = ConvertingBody::new(inner, Box::new(Upper));
        let mut data = Vec::new();
        let error = loop {
            match body.frame().await {
                Some(Ok(frame)) => data.extend_from_slice(&frame.into_data().unwrap()),
                Some(Err(error)) => break error,
                None => panic!("must end with an error"),
            }
        };
        assert_eq!(data, b"OKERR");
        assert!(error.to_string().contains("bad chunk"));
    }

    #[tokio::test]
    async fn empty_upstream_still_flushes_finish() {
        let inner = body_of(Vec::new());
        let body = ConvertingBody::new(inner, Box::new(Upper));
        assert_eq!(body.collect().await.unwrap().to_bytes(), "END");
    }
}
