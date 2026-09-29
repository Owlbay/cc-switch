//! CC Switch 中转的协议转换组件：通过统一中间表示（IR）在 Anthropic Messages、
//! OpenAI Chat Completions、OpenAI Responses、Gemini 之间转换。
//!
//! 实现 `cc_proxy_core::convert::Converter`，由可执行程序在构造中转时注入；没有配置
//! 转换的上游不经过本 crate，仍逐字节透传。设计见 `docs/protocol-conversion-design-zh.md`。
//!
//! 已支持的方向：
//! - `claude → claude`：恒等转换，只改请求与响应中的模型名，其余字段原样保留
//! - `claude → openai_chat`

pub mod anthropic;
pub mod chat;
pub mod envelope;
pub mod ir;
pub mod sse;

use bytes::Bytes;
use cc_proxy_core::convert::{
    map_model, ConversionContext, ConvertError, Converter, InboundRequest, OutboundMeta,
    OutboundRequest, ResponseConverter,
};
use cc_proxy_core::Interface;
use http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use serde_json::Value;

/// 基于 IR 的协议转换器
#[derive(Debug, Default, Clone, Copy)]
pub struct IrConverter;

impl Converter for IrConverter {
    fn supports(&self, client: Interface, upstream: Interface) -> bool {
        matches!(
            (client, upstream),
            (Interface::Claude, Interface::Claude) | (Interface::Claude, Interface::OpenaiChat)
        )
    }

    fn convert_request(
        &self,
        ctx: &ConversionContext<'_>,
        request: &InboundRequest<'_>,
    ) -> Result<OutboundRequest, ConvertError> {
        match (ctx.client, ctx.upstream) {
            (client, upstream) if client == upstream => identity_request(ctx, request),
            (Interface::Claude, Interface::OpenaiChat) => claude_to_chat_request(ctx, request),
            (client, upstream) => Err(ConvertError::Unsupported(format!(
                "conversion {} -> {} is not implemented",
                client.as_str(),
                upstream.as_str()
            ))),
        }
    }

    fn response_converter(
        &self,
        ctx: &ConversionContext<'_>,
        meta: &OutboundMeta,
    ) -> Box<dyn ResponseConverter> {
        match (ctx.client, ctx.upstream) {
            (client, upstream) if client == upstream => {
                Box::new(IdentityResponse::new(client, meta))
            }
            _ => Box::new(ChatToAnthropic::new(meta)),
        }
    }
}

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

fn parse_json(body: &Bytes) -> Result<Value, ConvertError> {
    serde_json::from_slice(body)
        .map_err(|e| ConvertError::Unsupported(format!("request body is not valid JSON: {e}")))
}

/// 同协议 + model_map：只改 `model` 字段，其余字段（缓存标记、签名、未知扩展）原样保留
fn identity_request(
    ctx: &ConversionContext<'_>,
    request: &InboundRequest<'_>,
) -> Result<OutboundRequest, ConvertError> {
    let mut meta = OutboundMeta::default();
    let body = if request.body.is_empty() {
        request.body.clone()
    } else {
        let mut value = parse_json(request.body)?;
        meta.stream = value
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            meta.client_model = model.to_string();
            meta.upstream_model = map_model(ctx.model_map, model).to_string();
            value["model"] = Value::String(meta.upstream_model.clone());
        }
        Bytes::from(value.to_string())
    };
    let mut headers = request.headers.clone();
    headers.remove(header::CONTENT_LENGTH);
    Ok(OutboundRequest {
        method: request.method.clone(),
        path: request.path.to_string(),
        query: request.query.map(str::to_string),
        headers,
        body,
        meta,
    })
}

fn claude_to_chat_request(
    ctx: &ConversionContext<'_>,
    request: &InboundRequest<'_>,
) -> Result<OutboundRequest, ConvertError> {
    if request.method != Method::POST || request.path != "/v1/messages" {
        return Err(ConvertError::Unsupported(format!(
            "{} {} has no equivalent on an openai_chat upstream",
            request.method, request.path
        )));
    }
    let body = parse_json(request.body)?;
    let ir = anthropic::parse_request(&body).map_err(ConvertError::Unsupported)?;
    let upstream_model = map_model(ctx.model_map, &ir.model).to_string();
    let chat = chat::render_request(
        &ir,
        &chat::RenderOptions {
            upstream_model: &upstream_model,
            default_max_output_tokens: ctx.default_max_output_tokens,
        },
    )
    .map_err(ConvertError::Unsupported)?;

    let mut headers = protocol_neutral_headers(request.headers, &["anthropic-"]);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(OutboundRequest {
        method: Method::POST,
        path: "/v1/chat/completions".into(),
        query: None,
        headers,
        body: Bytes::from(chat.to_string()),
        meta: OutboundMeta {
            stream: ir.stream,
            client_model: ir.model.clone(),
            upstream_model,
            tool_names: ir.tools.iter().map(|t| t.name.clone()).collect(),
            ..Default::default()
        },
    })
}

/// 去掉源协议专有头与长度头（目标协议不认识，严格上游可能拒绝）
fn protocol_neutral_headers(headers: &HeaderMap, prefixes: &[&str]) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let lower = name.as_str();
        if name == header::CONTENT_LENGTH || prefixes.iter().any(|p| lower.starts_with(p)) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// 响应：恒等
// ---------------------------------------------------------------------------

/// 恒等转换的响应：只把模型名改回客户端请求的名字
struct IdentityResponse {
    protocol: Interface,
    client_model: String,
    parser: sse::SseParser,
}

impl IdentityResponse {
    fn new(protocol: Interface, meta: &OutboundMeta) -> Self {
        Self {
            protocol,
            client_model: meta.client_model.clone(),
            parser: sse::SseParser::new(),
        }
    }

    fn reframe(&self, events: Vec<sse::SseEvent>) -> Vec<Bytes> {
        events
            .into_iter()
            .map(|event| {
                let data = if event.event.as_deref() == Some("message_start")
                    && !self.client_model.is_empty()
                {
                    match serde_json::from_str::<Value>(&event.data) {
                        Ok(mut value) if value.pointer("/message/model").is_some() => {
                            value["message"]["model"] = Value::String(self.client_model.clone());
                            value.to_string()
                        }
                        _ => event.data,
                    }
                } else {
                    event.data
                };
                sse::frame(event.event.as_deref(), &data)
            })
            .collect()
    }
}

impl ResponseConverter for IdentityResponse {
    fn convert_head(&mut self, _status: StatusCode, headers: &HeaderMap) -> HeaderMap {
        headers.clone()
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ConvertError> {
        let events = self.parser.feed(chunk);
        Ok(self.reframe(events))
    }

    fn finish(&mut self, error: Option<&(dyn std::error::Error + 'static)>) -> Vec<Bytes> {
        let events = self.parser.finish();
        let mut out = self.reframe(events);
        if let (Some(error), Interface::Claude) = (error, self.protocol) {
            out.push(sse::frame(
                Some("error"),
                &anthropic::error_body("api_error", &error.to_string()).to_string(),
            ));
        }
        out
    }

    fn convert_full(&mut self, status: StatusCode, body: &[u8]) -> Result<Bytes, ConvertError> {
        if !status.is_success() || self.client_model.is_empty() {
            return Ok(Bytes::copy_from_slice(body));
        }
        match serde_json::from_slice::<Value>(body) {
            Ok(mut value) if value.get("model").is_some() => {
                value["model"] = Value::String(self.client_model.clone());
                Ok(Bytes::from(value.to_string()))
            }
            _ => Ok(Bytes::copy_from_slice(body)),
        }
    }
}

// ---------------------------------------------------------------------------
// 响应：Chat → Anthropic
// ---------------------------------------------------------------------------

struct ChatToAnthropic {
    stream: bool,
    client_model: String,
    parser: sse::SseParser,
    decoder: chat::StreamDecoder,
    encoder: anthropic::StreamEncoder,
}

impl ChatToAnthropic {
    fn new(meta: &OutboundMeta) -> Self {
        Self {
            stream: meta.stream,
            client_model: meta.client_model.clone(),
            parser: sse::SseParser::new(),
            decoder: chat::StreamDecoder::new(),
            encoder: anthropic::StreamEncoder::new(meta.client_model.clone()),
        }
    }
}

impl ResponseConverter for ChatToAnthropic {
    fn convert_head(&mut self, status: StatusCode, headers: &HeaderMap) -> HeaderMap {
        let mut out = protocol_neutral_headers(headers, &["openai-"]);
        out.remove(header::CONTENT_TYPE);
        let content_type = if self.stream && status.is_success() {
            "text/event-stream"
        } else {
            "application/json"
        };
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        out
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ConvertError> {
        let mut out = Vec::new();
        for event in self.parser.feed(chunk) {
            for ir_event in self.decoder.feed(&event.data) {
                out.extend(self.encoder.encode(ir_event));
            }
        }
        Ok(out)
    }

    fn finish(&mut self, error: Option<&(dyn std::error::Error + 'static)>) -> Vec<Bytes> {
        let mut out = Vec::new();
        for event in self.parser.finish() {
            for ir_event in self.decoder.feed(&event.data) {
                out.extend(self.encoder.encode(ir_event));
            }
        }
        match error {
            Some(error) => out.extend(self.encoder.finish(Some(&error.to_string()))),
            None => {
                for ir_event in self.decoder.finish() {
                    out.extend(self.encoder.encode(ir_event));
                }
                out.extend(self.encoder.finish(None));
            }
        }
        out
    }

    fn convert_full(&mut self, status: StatusCode, body: &[u8]) -> Result<Bytes, ConvertError> {
        if !status.is_success() {
            return Ok(Bytes::from(
                anthropic_error_from_upstream(status, body).to_string(),
            ));
        }
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| ConvertError::Response(format!("upstream returned invalid JSON: {e}")))?;
        let response = chat::parse_response(&value).map_err(ConvertError::Response)?;
        Ok(Bytes::from(
            anthropic::render_response(&response, &self.client_model).to_string(),
        ))
    }
}

/// 上游错误 body → Anthropic 错误格式（状态码由调用方保持）
fn anthropic_error_from_upstream(status: StatusCode, body: &[u8]) -> Value {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value.get("error").map(chat::error_message).or_else(|| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    let message = if message.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("upstream error")
            .to_string()
    } else {
        message
    };
    anthropic::error_body(anthropic::error_type_for_status(status.as_u16()), &message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cc_proxy_core::convert::ModelMap;
    use serde_json::json;

    fn ctx<'a>(client: Interface, upstream: Interface, map: &'a ModelMap) -> ConversionContext<'a> {
        ConversionContext {
            client,
            upstream,
            model_map: map,
            default_max_output_tokens: 16384,
        }
    }

    fn inbound<'a>(
        method: &'a Method,
        path: &'a str,
        headers: &'a HeaderMap,
        body: &'a Bytes,
    ) -> InboundRequest<'a> {
        InboundRequest {
            method,
            path,
            query: Some("beta=true"),
            headers,
            body,
        }
    }

    #[test]
    fn supported_directions() {
        let c = IrConverter;
        assert!(c.supports(Interface::Claude, Interface::Claude));
        assert!(c.supports(Interface::Claude, Interface::OpenaiChat));
        assert!(!c.supports(Interface::Claude, Interface::Gemini));
        assert!(!c.supports(Interface::OpenaiResponses, Interface::Claude));
    }

    #[test]
    fn identity_only_rewrites_the_model() {
        let mut map = ModelMap::new();
        map.insert("claude-sonnet-5".into(), "claude-sonnet-4-5".into());
        let body = json!({
            "model": "claude-sonnet-5",
            "stream": true,
            "system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}],
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "x-unknown-extension": 1,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let bytes = Bytes::from(body.to_string());
        let headers = HeaderMap::new();
        let method = Method::POST;
        let out = IrConverter
            .convert_request(
                &ctx(Interface::Claude, Interface::Claude, &map),
                &inbound(&method, "/v1/messages", &headers, &bytes),
            )
            .unwrap();
        let sent: Value = serde_json::from_slice(&out.body).unwrap();
        let mut expected = body.clone();
        expected["model"] = json!("claude-sonnet-4-5");
        assert_eq!(sent, expected);
        assert_eq!(out.path, "/v1/messages");
        assert_eq!(out.query.as_deref(), Some("beta=true"));
        assert!(out.meta.stream);
        assert_eq!(out.meta.client_model, "claude-sonnet-5");

        let mut response = IrConverter
            .response_converter(&ctx(Interface::Claude, Interface::Claude, &map), &out.meta);
        let converted = response
            .feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\",\"id\":\"m\"}}\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\n")
            .unwrap();
        let text: String = converted
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert!(text.contains("\"model\":\"claude-sonnet-5\""), "{text}");
        assert!(text.contains("event: ping"));

        let full = response
            .convert_full(
                StatusCode::OK,
                br#"{"id":"m","model":"claude-sonnet-4-5","content":[]}"#,
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&full).unwrap(),
            json!({"id": "m", "model": "claude-sonnet-5", "content": []})
        );
        // 错误 body 原样
        assert_eq!(
            response
                .convert_full(StatusCode::BAD_REQUEST, b"raw")
                .unwrap(),
            "raw"
        );
    }

    #[test]
    fn claude_to_chat_request_and_headers() {
        let map = ModelMap::from([("*".to_string(), "deepseek-chat".to_string())]);
        let body = Bytes::from(
            json!({
                "model": "claude-sonnet-5", "max_tokens": 1000, "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        );
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert("anthropic-beta", HeaderValue::from_static("x"));
        headers.insert("user-agent", HeaderValue::from_static("claude-cli"));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("99"));
        let method = Method::POST;
        let out = IrConverter
            .convert_request(
                &ctx(Interface::Claude, Interface::OpenaiChat, &map),
                &inbound(&method, "/v1/messages", &headers, &body),
            )
            .unwrap();
        assert_eq!(out.path, "/v1/chat/completions");
        assert_eq!(out.query, None);
        assert!(out.headers.get("anthropic-version").is_none());
        assert!(out.headers.get("anthropic-beta").is_none());
        assert!(out.headers.get(header::CONTENT_LENGTH).is_none());
        assert_eq!(out.headers["user-agent"], "claude-cli");
        let sent: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(sent["model"], "deepseek-chat");
        assert_eq!(sent["max_tokens"], 1000);
        assert_eq!(out.meta.client_model, "claude-sonnet-5");
        assert!(out.meta.stream);
    }

    #[test]
    fn claude_to_chat_rejects_other_endpoints_and_bad_bodies() {
        let map = ModelMap::new();
        let headers = HeaderMap::new();
        let body = Bytes::from_static(b"{}");
        let post = Method::POST;
        let c = ctx(Interface::Claude, Interface::OpenaiChat, &map);
        for path in ["/v1/messages/count_tokens", "/v1/models"] {
            assert!(matches!(
                IrConverter.convert_request(&c, &inbound(&post, path, &headers, &body)),
                Err(ConvertError::Unsupported(_))
            ));
        }
        let bad = Bytes::from_static(b"not json");
        assert!(matches!(
            IrConverter.convert_request(&c, &inbound(&post, "/v1/messages", &headers, &bad)),
            Err(ConvertError::Unsupported(_))
        ));
    }

    #[test]
    fn chat_errors_are_rewritten_for_anthropic_clients() {
        let meta = OutboundMeta {
            client_model: "claude".into(),
            ..Default::default()
        };
        let mut response = ChatToAnthropic::new(&meta);
        let body = response
            .convert_full(
                StatusCode::TOO_MANY_REQUESTS,
                br#"{"error":{"message":"slow down","type":"rate_limit"}}"#,
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}})
        );
        let body = response
            .convert_full(StatusCode::BAD_GATEWAY, b"<html>bad gateway</html>")
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"]["message"],
            "<html>bad gateway</html>"
        );
        assert!(matches!(
            response.convert_full(StatusCode::OK, br#"{"error":{"message":"inside 200"}}"#),
            Err(ConvertError::Response(_))
        ));
    }
}
