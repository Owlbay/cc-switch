//! 请求转发（设计文档 §5）。
//!
//! 流程：入站鉴权 → 识别接口 → 读取完整 body → 选上游 → 按透传边界重建请求 → 发送 →
//! 把上游响应原样流回。整个流程在同一个 future 内完成，**不 `tokio::spawn`**：客户端断开时
//! hyper 会 drop 这个 future，上游请求随之取消。

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, Uri, Version};
use http_body::Body;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};

use crate::auth::{AuthDecision, InboundAuth};
use crate::config::RelayConfig;
use crate::error::{relay_error, BoxError, RelayBody, RelayErrorKind};
use crate::headers::{client_response_headers, upstream_auth_header, upstream_request_headers};
use crate::interface::{identify, Interface};
use crate::upstream::{BuildError, ResolvedUpstream, UpstreamSet};
use crate::upstream_url::{upstream_host, upstream_uri};

/// 透传中转：持有鉴权器与已解析的上游，可在多个连接间共享。
pub struct Relay {
    auth: InboundAuth,
    upstreams: UpstreamSet,
    max_body_bytes: usize,
    preserve_header_case: bool,
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

        let Some(upstream) = self.upstreams.for_interface(interface).first() else {
            return relay_error(
                RelayErrorKind::NoUpstream,
                format!("no upstream configured for {}", interface.as_str()),
            );
        };

        let extensions = std::mem::take(&mut parts.extensions);
        let request = match build_upstream_request(
            upstream,
            &parts,
            &path,
            query.as_deref(),
            body,
            self.preserve_header_case.then_some(extensions),
        ) {
            Ok(request) => request,
            Err(message) => return relay_error(RelayErrorKind::Internal, message),
        };

        match upstream.client.request(request).await {
            Ok(response) => relay_response(response, self.preserve_header_case),
            Err(error) => {
                tracing::warn!(
                    upstream = %upstream.id,
                    interface = interface.as_str(),
                    error = %error,
                    "上游请求失败"
                );
                relay_error(
                    RelayErrorKind::BadGateway,
                    format!("upstream {} request failed", upstream.id),
                )
            }
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
fn relay_response(
    response: Response<hyper::body::Incoming>,
    preserve_header_case: bool,
) -> Response<RelayBody> {
    let (mut parts, body) = response.into_parts();
    let headers = client_response_headers(&parts.headers);
    let mut out = Response::new(body.map_err(BoxError::from).boxed());
    *out.status_mut() = parts.status;
    *out.headers_mut() = headers;
    if preserve_header_case {
        *out.extensions_mut() = std::mem::take(&mut parts.extensions);
    }
    out
}
