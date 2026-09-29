//! 中转自身生成的响应（设计文档 §3、§5）。
//!
//! 这些响应不属于透传内容：鉴权失败、路径不匹配、body 过大、没有上游、上游不可达等。
//! body 统一为 `{"error":{"type":"...","message":"..."}}`。

use bytes::Bytes;
use http::{header, HeaderValue, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 返回给客户端的响应 body：透传时是上游 body 流，中转自身响应时是固定 JSON。
pub type RelayBody = BoxBody<Bytes, BoxError>;

/// 中转自身错误的分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayErrorKind {
    Unauthorized,
    NotFound,
    PayloadTooLarge,
    BadRequest,
    NoUpstream,
    BadGateway,
    Internal,
    /// 所有候选上游都无法用目标协议表达该请求
    ConversionUnsupported,
    /// 转换器内部错误
    ConversionInternal,
    /// 响应阶段转换失败（上游 body 无法解析、被压缩等）
    ConversionFailed,
}

impl RelayErrorKind {
    pub fn status(self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::NoUpstream => StatusCode::SERVICE_UNAVAILABLE,
            Self::BadGateway => StatusCode::BAD_GATEWAY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ConversionUnsupported => StatusCode::BAD_REQUEST,
            Self::ConversionInternal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ConversionFailed => StatusCode::BAD_GATEWAY,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "relay_unauthorized",
            Self::NotFound => "relay_not_found",
            Self::PayloadTooLarge => "relay_payload_too_large",
            Self::BadRequest => "relay_bad_request",
            Self::NoUpstream => "relay_no_upstream",
            Self::BadGateway => "relay_bad_gateway",
            Self::Internal => "relay_internal_error",
            Self::ConversionUnsupported => "relay_conversion_unsupported",
            Self::ConversionInternal | Self::ConversionFailed => "relay_conversion_error",
        }
    }
}

/// 构造中转自身的 JSON 错误响应。
pub fn relay_error(kind: RelayErrorKind, message: impl Into<String>) -> Response<RelayBody> {
    let body = serde_json::json!({
        "error": { "type": kind.as_str(), "message": message.into() }
    });
    let bytes = Bytes::from(body.to_string());
    let mut response = Response::new(full(bytes));
    *response.status_mut() = kind.status();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// 把固定字节包装成 [`RelayBody`]。
pub fn full(bytes: Bytes) -> RelayBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn error_response_shape() {
        let response = relay_error(RelayErrorKind::NoUpstream, "no upstream for gemini");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["type"], "relay_no_upstream");
        assert_eq!(json["error"]["message"], "no upstream for gemini");
    }

    #[test]
    fn kinds_map_to_statuses() {
        assert_eq!(RelayErrorKind::Unauthorized.status(), 401);
        assert_eq!(RelayErrorKind::NotFound.status(), 404);
        assert_eq!(RelayErrorKind::PayloadTooLarge.status(), 413);
        assert_eq!(RelayErrorKind::BadRequest.status(), 400);
        assert_eq!(RelayErrorKind::BadGateway.status(), 502);
        assert_eq!(RelayErrorKind::Internal.status(), 500);
        assert_eq!(RelayErrorKind::ConversionUnsupported.status(), 400);
        assert_eq!(RelayErrorKind::ConversionInternal.status(), 500);
        assert_eq!(RelayErrorKind::ConversionFailed.status(), 502);
    }
}
