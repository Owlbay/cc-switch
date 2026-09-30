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
pub mod responses;
pub mod responses_upstream;
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
            (Interface::Claude, Interface::Claude)
                | (Interface::Claude, Interface::OpenaiChat)
                | (Interface::OpenaiResponses, Interface::OpenaiResponses)
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

    let mut headers = protocol_neutral_headers(request.headers, &["anthropic-", "x-stainless-"]);
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

/// 客户端要求流式、上游却返回完整 JSON 时，缓冲整段 body 的上限
const JSON_FALLBACK_LIMIT: usize = 64 * 1024 * 1024;

/// 成功响应但 content-type 是 JSON：上游忽略了 `stream`（或端点本来就不流式）
fn is_json_response(status: StatusCode, headers: &HeaderMap) -> bool {
    status.is_success()
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("json"))
}

/// 缓冲一段 body；超过上限视为上游响应错误
fn buffer_json(buffer: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ConvertError> {
    if buffer.len() + chunk.len() > JSON_FALLBACK_LIMIT {
        return Err(ConvertError::Response(format!(
            "non-streaming upstream response exceeds {JSON_FALLBACK_LIMIT} bytes"
        )));
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}

/// 恒等转换的响应：只把模型名改回客户端请求的名字
struct IdentityResponse {
    protocol: Interface,
    client_model: String,
    parser: sse::SseParser,
    /// 流式请求收到了 JSON 响应：缓冲后按非流式改写
    json: Option<Vec<u8>>,
}

impl IdentityResponse {
    fn new(protocol: Interface, meta: &OutboundMeta) -> Self {
        Self {
            protocol,
            client_model: meta.client_model.clone(),
            parser: sse::SseParser::new(),
            json: None,
        }
    }

    fn reframe(&self, events: Vec<sse::SseEvent>) -> Vec<Bytes> {
        events
            .into_iter()
            .map(|event| {
                let data = self.rewrite_model(event.event.as_deref(), event.data);
                sse::frame(event.event.as_deref(), &data)
            })
            .collect()
    }

    /// 流式事件中的模型名：Anthropic 只在 `message_start.message.model`；Responses 的
    /// `response.*` 生命周期事件都带完整 response 对象（`response.model`）
    fn rewrite_model(&self, event: Option<&str>, data: String) -> String {
        if self.client_model.is_empty() {
            return data;
        }
        let pointer = match (self.protocol, event) {
            (Interface::Claude, Some("message_start")) => "/message/model",
            (Interface::OpenaiResponses, Some(name)) if name.starts_with("response.") => {
                "/response/model"
            }
            _ => return data,
        };
        match serde_json::from_str::<Value>(&data) {
            Ok(mut value) if value.pointer(pointer).is_some() => {
                *value.pointer_mut(pointer).expect("checked above") =
                    Value::String(self.client_model.clone());
                value.to_string()
            }
            _ => data,
        }
    }

    /// 内层出错时补发的客户端协议错误帧
    fn error_frame(&self, message: &str) -> Option<Bytes> {
        match self.protocol {
            Interface::Claude => Some(sse::frame(
                Some("error"),
                &anthropic::error_body("api_error", message).to_string(),
            )),
            Interface::OpenaiResponses => Some(sse::frame(
                Some("response.failed"),
                &serde_json::json!({
                    "type": "response.failed",
                    "response": {
                        "object": "response",
                        "status": "failed",
                        "model": self.client_model,
                        "error": { "code": "server_error", "message": message },
                    }
                })
                .to_string(),
            )),
            _ => None,
        }
    }
}

impl ResponseConverter for IdentityResponse {
    fn convert_head(&mut self, status: StatusCode, headers: &HeaderMap) -> HeaderMap {
        if is_json_response(status, headers) {
            self.json = Some(Vec::new());
        }
        headers.clone()
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ConvertError> {
        if let Some(buffer) = &mut self.json {
            buffer_json(buffer, chunk)?;
            return Ok(Vec::new());
        }
        let events = self.parser.feed(chunk);
        Ok(self.reframe(events))
    }

    fn finish(&mut self, error: Option<&(dyn std::error::Error + 'static)>) -> Vec<Bytes> {
        if let Some(buffer) = self.json.take() {
            // 出错时 body 已不完整，原样交出已收到的部分，由客户端按截断处理
            if error.is_some() {
                return vec![Bytes::from(buffer)];
            }
            let body = self.convert_full(StatusCode::OK, &buffer);
            return vec![body.unwrap_or_else(|_| Bytes::from(buffer))];
        }
        let events = self.parser.finish();
        let mut out = self.reframe(events);
        if let Some(frame) = error.and_then(|error| self.error_frame(&error.to_string())) {
            out.push(frame);
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
    /// 流式请求收到了 JSON 响应：缓冲后整体转换成 Anthropic 事件流
    json: Option<Vec<u8>>,
}

impl ChatToAnthropic {
    fn new(meta: &OutboundMeta) -> Self {
        Self {
            stream: meta.stream,
            client_model: meta.client_model.clone(),
            parser: sse::SseParser::new(),
            decoder: chat::StreamDecoder::new(),
            encoder: anthropic::StreamEncoder::new(meta.client_model.clone()),
            json: None,
        }
    }

    fn encode_buffered(&mut self, buffer: &[u8]) -> Vec<Bytes> {
        let response = serde_json::from_slice::<Value>(buffer)
            .map_err(|e| format!("upstream returned invalid JSON: {e}"))
            .and_then(|value| chat::parse_response(&value));
        match response {
            Ok(response) => ir::response_events(&response)
                .into_iter()
                .flat_map(|event| self.encoder.encode(event))
                .collect(),
            Err(message) => self.encoder.finish(Some(&message)),
        }
    }
}

impl ResponseConverter for ChatToAnthropic {
    fn convert_head(&mut self, status: StatusCode, headers: &HeaderMap) -> HeaderMap {
        if self.stream && is_json_response(status, headers) {
            self.json = Some(Vec::new());
        }
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
        if let Some(buffer) = &mut self.json {
            buffer_json(buffer, chunk)?;
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for event in self.parser.feed(chunk) {
            for ir_event in self.decoder.feed(&event.data) {
                out.extend(self.encoder.encode(ir_event));
            }
        }
        Ok(out)
    }

    fn finish(&mut self, error: Option<&(dyn std::error::Error + 'static)>) -> Vec<Bytes> {
        if let Some(buffer) = self.json.take() {
            return match error {
                Some(error) => self.encoder.finish(Some(&error.to_string())),
                None => self.encode_buffered(&buffer),
            };
        }
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
        assert!(c.supports(Interface::OpenaiResponses, Interface::OpenaiResponses));
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

    #[test]
    fn responses_identity_rewrites_every_lifecycle_event() {
        let map = ModelMap::from([("gpt-5-codex".to_string(), "gpt-5.1-codex".to_string())]);
        let body = Bytes::from(
            json!({"model": "gpt-5-codex", "stream": true, "store": false, "input": "hi"})
                .to_string(),
        );
        let headers = HeaderMap::new();
        let method = Method::POST;
        let c = ctx(Interface::OpenaiResponses, Interface::OpenaiResponses, &map);
        let out = IrConverter
            .convert_request(&c, &inbound(&method, "/v1/responses", &headers, &body))
            .unwrap();
        let sent: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(
            sent,
            json!({"model": "gpt-5.1-codex", "stream": true, "store": false, "input": "hi"})
        );

        let mut response = IrConverter.response_converter(&c, &out.meta);
        let frames = response
            .feed(concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.1-codex\"}}\n\n",
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"gpt-5.1-codex\"}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.1-codex\"}}\n\n",
            ).as_bytes())
            .unwrap();
        let text: String = frames
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert_eq!(
            text.matches("\"model\":\"gpt-5-codex\"").count(),
            2,
            "{text}"
        );
        assert!(
            text.contains("\"delta\":\"gpt-5.1-codex\""),
            "text content is untouched"
        );

        let error = std::io::Error::other("upstream reset");
        let tail = response.finish(Some(&error));
        let tail = String::from_utf8_lossy(&tail[0]).into_owned();
        assert!(tail.starts_with("event: response.failed\n"), "{tail}");
        assert!(tail.contains("upstream reset"));
    }
}
