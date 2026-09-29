//! 请求转发（设计文档 §5）。
//!
//! 流程：入站鉴权 → 识别接口 → 读取完整 body → 按顺序尝试上游（熔断、故障转移，§5.2）→
//! 按透传边界重建请求 → 发送 → 把上游响应原样流回。整个流程在同一个 future 内完成，
//! **不 `tokio::spawn`**：客户端断开时 hyper 会 drop 这个 future，上游请求随之取消。
//!
//! 提交点是 `handle` 返回 `Response` 的那一刻：之前的失败都可以换上游重发同一请求，
//! 之后上游中断只会中断客户端连接，不再重试。

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, Uri, Version};
use http_body::Body;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};

use crate::auth::{AuthDecision, InboundAuth};
use crate::breaker::{CircuitBreaker, Permit};
use crate::config::RelayConfig;
use crate::error::{relay_error, BoxError, RelayBody, RelayErrorKind};
use crate::headers::{client_response_headers, upstream_auth_header, upstream_request_headers};
use crate::idle::IdleTimeoutBody;
use crate::interface::{identify, Interface};
use crate::upstream::{BuildError, ResolvedUpstream, UpstreamSet};
use crate::upstream_url::{upstream_host, upstream_uri};

/// 透传中转：持有鉴权器与已解析的上游，可在多个连接间共享。
pub struct Relay {
    auth: InboundAuth,
    upstreams: UpstreamSet,
    max_body_bytes: usize,
    preserve_header_case: bool,
    response_head_timeout: Duration,
    idle_timeout: Duration,
    retry_on_status: Vec<u16>,
    retry_body_bytes: usize,
    max_attempts: Option<usize>,
}

impl Relay {
    /// 校验配置并构造中转（包括 TLS 与各出站代理的客户端）。
    pub fn new(config: &RelayConfig) -> Result<Self, BuildError> {
        let upstreams = UpstreamSet::build(config)?;
        Ok(Self {
            auth: InboundAuth::from_config(&config.server),
            upstreams,
            max_body_bytes: usize::try_from(config.server.max_body_bytes).unwrap_or(usize::MAX),
            preserve_header_case: config.server.preserve_header_case,
            response_head_timeout: Duration::from_secs(config.timeouts.response_head_secs),
            idle_timeout: Duration::from_secs(config.timeouts.idle_secs),
            retry_on_status: config.failover.retry_on_status.clone(),
            retry_body_bytes: usize::try_from(config.failover.retry_body_bytes)
                .unwrap_or(usize::MAX),
            max_attempts: config.failover.max_attempts,
        })
    }

    pub fn preserve_header_case(&self) -> bool {
        self.preserve_header_case
    }

    /// 处理一个入站请求。
    pub async fn handle<B>(&self, request: Request<B>) -> Response<RelayBody>
    where
        B: Body<Data = Bytes>,
        B::Error: Into<BoxError>,
    {
        let (mut parts, body) = request.into_parts();
        let path = parts.uri.path().to_string();
        let query = parts.uri.query().map(str::to_string);

        if path.starts_with("/_relay/") {
            return relay_error(
                RelayErrorKind::NotFound,
                format!("unknown relay path {path}"),
            );
        }

        match self.auth.check(&parts.headers, query.as_deref()) {
            AuthDecision::Denied => {
                return relay_error(
                    RelayErrorKind::Unauthorized,
                    "missing or invalid relay token",
                );
            }
            AuthDecision::Anonymous if parts.headers.contains_key(http::header::ORIGIN) => {
                tracing::warn!("匿名模式下收到带 Origin 头的请求，可能来自浏览器页面");
            }
            AuthDecision::Allowed | AuthDecision::Anonymous => {}
        }

        let Some(interface) = identify(&parts.method, &path, &parts.headers) else {
            return relay_error(
                RelayErrorKind::NotFound,
                format!("{} {path} is not a relayed interface", parts.method),
            );
        };

        let body = match Limited::new(body, self.max_body_bytes).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) if error.is::<LengthLimitError>() => {
                return relay_error(
                    RelayErrorKind::PayloadTooLarge,
                    format!("request body exceeds {} bytes", self.max_body_bytes),
                );
            }
            Err(_) => {
                return relay_error(RelayErrorKind::BadRequest, "failed to read request body");
            }
        };

        let candidates = self.upstreams.for_interface(interface);
        if candidates.is_empty() {
            return relay_error(
                RelayErrorKind::NoUpstream,
                format!("no upstream configured for {}", interface.as_str()),
            );
        }

        // Responses 的按 id 读取 / 删除引用上游私有状态，换上游只会得到别家的 404
        let max_attempts = if is_stateful_responses_call(interface, &parts.method, &path) {
            1
        } else {
            self.max_attempts
                .unwrap_or(candidates.len())
                .min(candidates.len())
        };
        let extensions = std::mem::take(&mut parts.extensions);
        let mut attempts = 0;
        let mut last = LastFailure::default();

        for (index, upstream) in candidates.iter().enumerate() {
            if attempts >= max_attempts {
                break;
            }
            let Some(permit) = upstream.breaker.try_acquire(Instant::now()) else {
                continue;
            };
            let attempt = AttemptGuard::new(&upstream.breaker, permit);
            attempts += 1;

            let request = match build_upstream_request(
                upstream,
                &parts,
                &path,
                query.as_deref(),
                body.clone(),
                self.preserve_header_case.then(|| extensions.clone()),
            ) {
                Ok(request) => request,
                Err(message) => return relay_error(RelayErrorKind::Internal, message),
            };

            let response = match tokio::time::timeout(
                self.response_head_timeout,
                upstream.client.request(request),
            )
            .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    attempt.failure();
                    tracing::warn!(
                        upstream = %upstream.id,
                        interface = interface.as_str(),
                        attempt = attempts,
                        error = %error,
                        "上游请求失败"
                    );
                    last.error = Some(format!("upstream {} request failed", upstream.id));
                    continue;
                }
                Err(_elapsed) => {
                    attempt.failure();
                    tracing::warn!(
                        upstream = %upstream.id,
                        interface = interface.as_str(),
                        attempt = attempts,
                        "等待上游响应头超时"
                    );
                    last.error = Some(format!(
                        "upstream {} did not respond within {}s",
                        upstream.id,
                        self.response_head_timeout.as_secs()
                    ));
                    continue;
                }
            };

            if !self.retry_on_status.contains(&response.status().as_u16()) {
                attempt.success();
                return relay_response(response, self.preserve_header_case, self.idle_timeout);
            }

            attempt.failure();
            tracing::warn!(
                upstream = %upstream.id,
                interface = interface.as_str(),
                attempt = attempts,
                status = response.status().as_u16(),
                "上游返回可重试状态码"
            );
            // is_available 只是快照：若到下一个上游时探测名额已被并发请求占用，循环会结束并
            // 回放这里缓冲的响应，结果与直接流式转发等价（body 不超过 retry_body_bytes 时）。
            let has_next = attempts < max_attempts
                && candidates[index + 1..]
                    .iter()
                    .any(|next| next.breaker.is_available(Instant::now()));
            if !has_next {
                // 已是最后一次可能的尝试：原样流式转发，不缓冲、不截断
                return relay_response(response, self.preserve_header_case, self.idle_timeout);
            }
            match buffer_response(
                response,
                self.retry_body_bytes,
                self.preserve_header_case,
                self.idle_timeout,
                &upstream.id,
            )
            .await
            {
                Ok(buffered) => last.response = Some(buffered),
                Err(message) => last.error = Some(message),
            }
        }

        // 全部失败：优先回放最近一次完整缓冲的上游响应（保留其状态码、retry-after 等），
        // 没有可回放的响应时才返回中转自身的 502；一次都没尝试（全部熔断）返回 503。
        match last {
            LastFailure {
                response: Some(response),
                ..
            } => response,
            LastFailure {
                error: Some(message),
                ..
            } => relay_error(RelayErrorKind::BadGateway, message),
            LastFailure { .. } => relay_error(
                RelayErrorKind::NoUpstream,
                format!(
                    "all upstreams for {} are unavailable (circuit open)",
                    interface.as_str()
                ),
            ),
        }
    }

    /// 某接口已配置的上游
    pub fn upstreams(&self, interface: Interface) -> &[Arc<ResolvedUpstream>] {
        self.upstreams.for_interface(interface)
    }
}

/// 按透传边界构造发往上游的请求。`extensions` 为 `Some` 时搬运入站请求的扩展
/// （其中包含 hyper 记录的头名原始大小写）。
fn build_upstream_request(
    upstream: &ResolvedUpstream,
    parts: &http::request::Parts,
    path: &str,
    query: Option<&str>,
    body: Bytes,
    extensions: Option<http::Extensions>,
) -> Result<Request<Full<Bytes>>, String> {
    let uri: Uri = upstream_uri(
        &upstream.base_url,
        upstream.strip_prefix.as_deref(),
        path,
        query,
    )
    .parse()
    .map_err(|e| format!("invalid upstream uri: {e}"))?;
    let host = HeaderValue::from_str(&upstream_host(&upstream.base_url))
        .map_err(|e| format!("invalid upstream host: {e}"))?;
    let auth = upstream_auth_header(upstream.auth, &upstream.api_key).map_err(|_| {
        format!(
            "upstream {} api_key is not a valid header value",
            upstream.id
        )
    })?;

    let mut request = Request::new(Full::new(body));
    *request.method_mut() = parts.method.clone();
    *request.uri_mut() = uri;
    *request.version_mut() = Version::HTTP_11;
    *request.headers_mut() = upstream_request_headers(&parts.headers, host, auth);
    if let Some(extensions) = extensions {
        *request.extensions_mut() = extensions;
    }
    Ok(request)
}

/// 上游响应原样流回：状态码与 body 不变，响应头只去掉 hop-by-hop。
/// 超过 `idle_timeout` 没有新数据时中断客户端连接（§5.4）。
fn relay_response(
    response: Response<hyper::body::Incoming>,
    preserve_header_case: bool,
    idle_timeout: Duration,
) -> Response<RelayBody> {
    let (mut parts, body) = response.into_parts();
    let headers = client_response_headers(&parts.headers);
    let mut out = Response::new(IdleTimeoutBody::new(body, idle_timeout).boxed());
    *out.status_mut() = parts.status;
    *out.headers_mut() = headers;
    if preserve_header_case {
        *out.extensions_mut() = std::mem::take(&mut parts.extensions);
    }
    out
}

/// `GET` / `DELETE /v1/responses/{id}` 读取或删除上游保存的响应，只能发给同一个上游。
fn is_stateful_responses_call(interface: Interface, method: &Method, path: &str) -> bool {
    interface == Interface::OpenaiResponses
        && (method == Method::GET || method == Method::DELETE)
        && path.starts_with("/v1/responses/")
}

/// 尝试循环中记录的失败
#[derive(Default)]
struct LastFailure {
    /// 最近一次完整缓冲的可重试响应，全部失败时原样回放
    response: Option<Response<RelayBody>>,
    /// 最近一次网络错误、超时，或可重试响应的 body 超过缓冲上限
    error: Option<String>,
}

/// 缓冲一个可重试响应（最多 `limit` 字节），用于全部上游失败时回放。
async fn buffer_response(
    response: Response<hyper::body::Incoming>,
    limit: usize,
    preserve_header_case: bool,
    idle_timeout: Duration,
    upstream_id: &str,
) -> Result<Response<RelayBody>, String> {
    let (mut parts, body) = response.into_parts();
    let body = IdleTimeoutBody::new(body, idle_timeout);
    let bytes = match Limited::new(body, limit).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) if error.is::<LengthLimitError>() => {
            return Err(format!(
                "upstream {upstream_id} failed and its error body exceeds {limit} bytes"
            ));
        }
        Err(_) => {
            return Err(format!(
                "upstream {upstream_id} failed while sending its error body"
            ));
        }
    };
    let mut out = Response::new(crate::error::full(bytes));
    *out.status_mut() = parts.status;
    *out.headers_mut() = client_response_headers(&parts.headers);
    if preserve_header_case {
        *out.extensions_mut() = std::mem::take(&mut parts.extensions);
    }
    Ok(out)
}

/// 一次尝试占用的熔断许可。结果确定时调用 `success` / `failure`；
/// 未确定就被 drop（例如客户端断开导致 future 被取消）时归还许可，不计成功也不计失败。
struct AttemptGuard<'a> {
    breaker: &'a CircuitBreaker,
    permit: Option<Permit>,
}

impl<'a> AttemptGuard<'a> {
    fn new(breaker: &'a CircuitBreaker, permit: Permit) -> Self {
        Self {
            breaker,
            permit: Some(permit),
        }
    }

    fn success(mut self) {
        if let Some(permit) = self.permit.take() {
            self.breaker.record_success(permit);
        }
    }

    fn failure(mut self) {
        if let Some(permit) = self.permit.take() {
            self.breaker.record_failure(permit, Instant::now());
        }
    }
}

impl Drop for AttemptGuard<'_> {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            self.breaker.release(permit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::BreakerState;

    #[test]
    fn stateful_responses_calls() {
        let r = Interface::OpenaiResponses;
        assert!(is_stateful_responses_call(
            r,
            &Method::GET,
            "/v1/responses/resp_1"
        ));
        assert!(is_stateful_responses_call(
            r,
            &Method::DELETE,
            "/v1/responses/resp_1"
        ));
        assert!(!is_stateful_responses_call(
            r,
            &Method::POST,
            "/v1/responses"
        ));
        assert!(!is_stateful_responses_call(
            r,
            &Method::POST,
            "/v1/responses/compact"
        ));
        assert!(!is_stateful_responses_call(
            Interface::Claude,
            &Method::GET,
            "/v1/responses/x"
        ));
    }

    #[test]
    fn dropped_guard_returns_probe_slot() {
        let breaker = CircuitBreaker::new(1, Duration::from_secs(60));
        let t0 = Instant::now();
        let permit = breaker.try_acquire(t0).unwrap();
        AttemptGuard::new(&breaker, permit).failure();
        assert_eq!(breaker.state(t0), BreakerState::Open);

        // failure() 用真实时钟记录打开时间，这里以当前时间为基准越过打开期
        let t1 = Instant::now() + Duration::from_secs(61);
        let probe = breaker.try_acquire(t1).unwrap();
        drop(AttemptGuard::new(&breaker, probe));
        assert!(
            breaker.is_available(t1),
            "probe slot must be released on drop"
        );

        let probe = breaker.try_acquire(t1).unwrap();
        AttemptGuard::new(&breaker, probe).success();
        assert_eq!(breaker.state(t1), BreakerState::Closed);
    }
}
