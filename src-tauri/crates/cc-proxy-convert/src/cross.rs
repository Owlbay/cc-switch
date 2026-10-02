//! 跨协议转换：客户端协议 → IR → 上游协议，响应方向反之（设计文档 §2、§6、§7）。
//!
//! 请求侧按客户端协议选解析器、按上游协议选生成器；响应侧按上游协议选解码器、按客户端
//! 协议选编码器。任意一对组合都经过同一条管道，新增协议只需补对应的编解码器。

use bytes::Bytes;
use cc_proxy_core::convert::{
    map_model, ConversionContext, ConvertError, InboundRequest, OutboundMeta, OutboundRequest,
    ResponseConverter,
};
use cc_proxy_core::Interface;
use http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use serde_json::Value;

use crate::ir::{self, Block, BlockKind, Event, Request, Response, ServerToolKind};
use crate::{anthropic, chat, gemini, responses, responses_upstream, sse};

/// 客户端协议可以作为转换源
pub fn is_client(protocol: Interface) -> bool {
    matches!(protocol, Interface::Claude | Interface::OpenaiResponses)
}

/// 已实现上游侧编解码的协议
pub fn is_upstream(protocol: Interface) -> bool {
    matches!(
        protocol,
        Interface::Claude | Interface::OpenaiChat | Interface::OpenaiResponses | Interface::Gemini
    )
}

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

/// 客户端协议中唯一可转换的端点（其他端点如 count_tokens、/v1/models、按 id 取响应，
/// 在其他协议没有等价物，返回 Unsupported 交给同接口的透传上游）
fn conversion_endpoint(client: Interface) -> &'static str {
    match client {
        Interface::Claude => "/v1/messages",
        _ => "/v1/responses",
    }
}

/// 客户端协议专有的请求头前缀：发往其他协议的上游前删除
fn client_header_prefixes(client: Interface) -> &'static [&'static str] {
    match client {
        Interface::Claude => &["anthropic-", "x-stainless-"],
        _ => &["openai-", "x-stainless-", "chatgpt-"],
    }
}

/// 上游协议专有的响应头前缀：发回客户端前删除
fn upstream_header_prefixes(upstream: Interface) -> &'static [&'static str] {
    match upstream {
        Interface::Claude => &["anthropic-"],
        Interface::Gemini => &["x-goog-"],
        _ => &["openai-"],
    }
}

/// 去掉给定前缀的头与长度头
pub(crate) fn strip_headers(headers: &HeaderMap, prefixes: &[&str]) -> HeaderMap {
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

fn parse_client_request(client: Interface, body: &Value) -> Result<Request, String> {
    match client {
        Interface::Claude => anthropic::parse_request(body),
        Interface::OpenaiResponses => responses::parse_request(body),
        other => Err(format!("{} is not a client protocol", other.as_str())),
    }
}

/// 上游请求的 path、query 与 body
fn render_upstream_request(
    upstream: Interface,
    request: &Request,
    options: &chat::RenderOptions<'_>,
) -> Result<(String, Option<String>, Value), String> {
    match upstream {
        Interface::OpenaiChat => Ok((
            "/v1/chat/completions".into(),
            None,
            chat::render_request(request, options)?,
        )),
        Interface::OpenaiResponses => Ok((
            "/v1/responses".into(),
            None,
            responses_upstream::render_request(request, options)?,
        )),
        Interface::Claude => Ok((
            "/v1/messages".into(),
            None,
            anthropic::render_request(request, options)?,
        )),
        Interface::Gemini => {
            let (path, query) = gemini::request_path(options.upstream_model, request.stream);
            Ok((path, query, gemini::render_request(request, options)?))
        }
    }
}

/// 客户端声明的内置搜索工具在客户端侧的名字（Anthropic 为 `name`，缺省 `web_search`）
fn hosted_web_search(request: &Request) -> Option<String> {
    request
        .server_tools
        .iter()
        .find(|tool| tool.kind == ServerToolKind::WebSearch)
        .map(|tool| {
            tool.raw
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("web_search")
                .to_string()
        })
}

pub fn convert_request(
    ctx: &ConversionContext<'_>,
    request: &InboundRequest<'_>,
) -> Result<OutboundRequest, ConvertError> {
    let endpoint = conversion_endpoint(ctx.client);
    if request.method != Method::POST || request.path != endpoint {
        return Err(ConvertError::Unsupported(format!(
            "{} {} has no equivalent on an {} upstream",
            request.method,
            request.path,
            ctx.upstream.as_str()
        )));
    }
    let body: Value = serde_json::from_slice(request.body)
        .map_err(|e| ConvertError::Unsupported(format!("request body is not valid JSON: {e}")))?;
    let ir = parse_client_request(ctx.client, &body).map_err(ConvertError::Unsupported)?;
    let upstream_model = map_model(ctx.model_map, &ir.model).to_string();
    let (path, query, rendered) = render_upstream_request(
        ctx.upstream,
        &ir,
        &chat::RenderOptions {
            upstream_model: &upstream_model,
            default_max_output_tokens: ctx.default_max_output_tokens,
        },
    )
    .map_err(ConvertError::Unsupported)?;

    let mut headers = strip_headers(request.headers, client_header_prefixes(ctx.client));
    // 响应要被解析：不协商压缩（§8.4）
    headers.remove(header::ACCEPT_ENCODING);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if ctx.upstream == Interface::Claude && !headers.contains_key("anthropic-version") {
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    Ok(OutboundRequest {
        method: Method::POST,
        path,
        query,
        headers,
        body: Bytes::from(rendered.to_string()),
        meta: OutboundMeta {
            stream: ir.stream,
            client_model: ir.model.clone(),
            upstream_model,
            tool_names: ir.tools.iter().map(|t| t.name.clone()).collect(),
            custom_tool_names: ir
                .tools
                .iter()
                .filter(|t| t.custom)
                .map(|t| t.name.clone())
                .collect(),
            hosted_web_search: hosted_web_search(&ir),
        },
    })
}

// ---------------------------------------------------------------------------
// 响应：解码器与编码器
// ---------------------------------------------------------------------------

/// 上游流 → 规范化事件
enum Decoder {
    Anthropic(anthropic::StreamDecoder),
    Chat(chat::StreamDecoder),
    Gemini(gemini::StreamDecoder),
    Responses(responses_upstream::StreamDecoder),
}

impl Decoder {
    fn new(upstream: Interface, upstream_model: &str) -> Self {
        match upstream {
            Interface::Gemini => Decoder::Gemini(gemini::StreamDecoder::new(upstream_model)),
            Interface::Claude => Decoder::Anthropic(anthropic::StreamDecoder::new()),
            Interface::OpenaiResponses => {
                Decoder::Responses(responses_upstream::StreamDecoder::new())
            }
            _ => Decoder::Chat(chat::StreamDecoder::new()),
        }
    }

    fn feed(&mut self, event: &sse::SseEvent) -> Vec<Event> {
        match self {
            Decoder::Anthropic(decoder) => decoder.feed(event.event.as_deref(), &event.data),
            Decoder::Chat(decoder) => decoder.feed(&event.data),
            Decoder::Gemini(decoder) => decoder.feed(&event.data),
            Decoder::Responses(decoder) => decoder.feed(event.event.as_deref(), &event.data),
        }
    }

    fn finish(&mut self) -> Vec<Event> {
        match self {
            Decoder::Anthropic(decoder) => decoder.finish(),
            Decoder::Chat(decoder) => decoder.finish(),
            Decoder::Gemini(decoder) => decoder.finish(),
            Decoder::Responses(decoder) => decoder.finish(),
        }
    }
}

/// 规范化事件 → 客户端流
enum Encoder {
    Anthropic(anthropic::StreamEncoder),
    Responses(responses::StreamEncoder),
}

impl Encoder {
    fn new(client: Interface, meta: &OutboundMeta) -> Self {
        match client {
            Interface::OpenaiResponses => Encoder::Responses(responses::StreamEncoder::new(
                meta.client_model.clone(),
                meta.custom_tool_names.clone(),
            )),
            _ => Encoder::Anthropic(anthropic::StreamEncoder::new(meta.client_model.clone())),
        }
    }

    fn encode(&mut self, event: Event) -> Vec<Bytes> {
        match self {
            Encoder::Anthropic(encoder) => encoder.encode(event),
            Encoder::Responses(encoder) => encoder.encode(event),
        }
    }

    fn finish(&mut self, error: Option<&str>) -> Vec<Bytes> {
        match self {
            Encoder::Anthropic(encoder) => encoder.finish(error),
            Encoder::Responses(encoder) => encoder.finish(error),
        }
    }
}

fn parse_upstream_response(
    upstream: Interface,
    body: &Value,
    upstream_model: &str,
) -> Result<Response, String> {
    match upstream {
        Interface::Gemini => gemini::parse_response(body, upstream_model),
        Interface::Claude => anthropic::parse_response(body),
        Interface::OpenaiResponses => responses_upstream::parse_response(body),
        _ => chat::parse_response(body),
    }
}

fn render_client_response(
    client: Interface,
    response: &Response,
    meta_model: &str,
    custom_tool_names: &[String],
) -> Value {
    match client {
        Interface::OpenaiResponses => {
            responses::render_response(response, meta_model, custom_tool_names)
        }
        _ => anthropic::render_response(response, meta_model),
    }
}

/// 上游错误 body 中的可读 message（各协议都把它放在 `error.message` 或顶层 `message`）
fn upstream_error_message(upstream: Interface, body: &[u8]) -> String {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let message = parsed.as_ref().and_then(|value| match upstream {
        Interface::Claude => anthropic::error_message(value),
        Interface::OpenaiResponses => responses_upstream::error_message(value),
        Interface::Gemini => gemini::error_message(value),
        _ => value.get("error").map(chat::error_message),
    });
    let message = message
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    message
}

/// 按客户端协议构造错误 body
pub(crate) fn client_error_body(client: Interface, status: StatusCode, message: &str) -> Value {
    let message = if message.is_empty() {
        status.canonical_reason().unwrap_or("upstream error")
    } else {
        message
    };
    match client {
        Interface::OpenaiResponses => {
            responses::error_body(responses::error_type_for_status(status.as_u16()), message)
        }
        _ => anthropic::error_body(anthropic::error_type_for_status(status.as_u16()), message),
    }
}

// ---------------------------------------------------------------------------
// 响应转换器
// ---------------------------------------------------------------------------

/// 客户端要求流式、上游却返回完整 JSON 时，缓冲整段 body 的上限
const JSON_FALLBACK_LIMIT: usize = 64 * 1024 * 1024;

/// 成功响应但 content-type 是 JSON：上游忽略了 `stream`（或端点本来就不流式）
pub(crate) fn is_json_response(status: StatusCode, headers: &HeaderMap) -> bool {
    status.is_success()
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("json"))
}

/// 缓冲一段 body；超过上限视为上游响应错误
pub(crate) fn buffer_json(buffer: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ConvertError> {
    if buffer.len() + chunk.len() > JSON_FALLBACK_LIMIT {
        return Err(ConvertError::Response(format!(
            "non-streaming upstream response exceeds {JSON_FALLBACK_LIMIT} bytes"
        )));
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}

pub struct CrossResponse {
    client: Interface,
    upstream: Interface,
    stream: bool,
    client_model: String,
    upstream_model: String,
    custom_tool_names: Vec<String>,
    /// 客户端侧的内置搜索工具名：上游返回的 ServerToolUse 改用该名字
    hosted_web_search: Option<String>,
    parser: sse::SseParser,
    decoder: Decoder,
    encoder: Encoder,
    /// 流式请求收到了 JSON 响应：缓冲后整体转换成客户端事件流
    json: Option<Vec<u8>>,
}

impl CrossResponse {
    pub fn new(ctx: &ConversionContext<'_>, meta: &OutboundMeta) -> Self {
        Self {
            client: ctx.client,
            upstream: ctx.upstream,
            stream: meta.stream,
            client_model: meta.client_model.clone(),
            upstream_model: meta.upstream_model.clone(),
            custom_tool_names: meta.custom_tool_names.clone(),
            hosted_web_search: meta.hosted_web_search.clone(),
            parser: sse::SseParser::new(),
            decoder: Decoder::new(ctx.upstream, &meta.upstream_model),
            encoder: Encoder::new(ctx.client, meta),
            json: None,
        }
    }

    fn encode_events(&mut self, events: Vec<Event>) -> Vec<Bytes> {
        let mut out = Vec::new();
        for mut event in events {
            if let (
                Some(hosted),
                Event::BlockStart {
                    kind: BlockKind::ServerToolUse { name, .. },
                    ..
                },
            ) = (&self.hosted_web_search, &mut event)
            {
                name.clone_from(hosted);
            }
            out.extend(self.encoder.encode(event));
        }
        out
    }

    /// 非流式响应中的 ServerToolUse 改用客户端侧工具名
    fn rename_server_tools(&self, response: &mut Response) {
        let Some(hosted) = &self.hosted_web_search else {
            return;
        };
        for block in &mut response.content {
            if let Block::ServerToolUse { name, .. } = block {
                name.clone_from(hosted);
            }
        }
    }

    fn encode_buffered(&mut self, buffer: &[u8]) -> Vec<Bytes> {
        let response = serde_json::from_slice::<Value>(buffer)
            .map_err(|e| format!("upstream returned invalid JSON: {e}"))
            .and_then(|value| parse_upstream_response(self.upstream, &value, &self.upstream_model));
        match response {
            Ok(response) => self.encode_events(ir::response_events(&response)),
            Err(message) => self.encoder.finish(Some(&message)),
        }
    }
}

impl ResponseConverter for CrossResponse {
    fn convert_head(&mut self, status: StatusCode, headers: &HeaderMap) -> HeaderMap {
        if self.stream && is_json_response(status, headers) {
            self.json = Some(Vec::new());
        }
        let mut out = strip_headers(headers, upstream_header_prefixes(self.upstream));
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
        let mut events = Vec::new();
        for event in self.parser.feed(chunk) {
            events.extend(self.decoder.feed(&event));
        }
        Ok(self.encode_events(events))
    }

    fn finish(&mut self, error: Option<&(dyn std::error::Error + 'static)>) -> Vec<Bytes> {
        if let Some(buffer) = self.json.take() {
            return match error {
                Some(error) => self.encoder.finish(Some(&error.to_string())),
                None => self.encode_buffered(&buffer),
            };
        }
        let mut events = Vec::new();
        for event in self.parser.finish() {
            events.extend(self.decoder.feed(&event));
        }
        let mut out = self.encode_events(events);
        match error {
            Some(error) => out.extend(self.encoder.finish(Some(&error.to_string()))),
            None => {
                let tail = self.decoder.finish();
                out.extend(self.encode_events(tail));
                out.extend(self.encoder.finish(None));
            }
        }
        out
    }

    fn convert_full(&mut self, status: StatusCode, body: &[u8]) -> Result<Bytes, ConvertError> {
        if !status.is_success() {
            let message = upstream_error_message(self.upstream, body);
            return Ok(Bytes::from(
                client_error_body(self.client, status, &message).to_string(),
            ));
        }
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| ConvertError::Response(format!("upstream returned invalid JSON: {e}")))?;
        let mut response = parse_upstream_response(self.upstream, &value, &self.upstream_model)
            .map_err(ConvertError::Response)?;
        self.rename_server_tools(&mut response);
        Ok(Bytes::from(
            render_client_response(
                self.client,
                &response,
                &self.client_model,
                &self.custom_tool_names,
            )
            .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude_search_request(tool_name: Option<&str>) -> Request {
        let mut tool = json!({"type": "web_search_20250305"});
        if let Some(name) = tool_name {
            tool["name"] = json!(name);
        }
        anthropic::parse_request(&json!({
            "model": "claude-sonnet-5",
            "max_tokens": 100,
            "tools": [tool],
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "rust"}},
                    {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []},
                    {"type": "text", "text": "answer"}
                ]},
                {"role": "user", "content": "more"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn hosted_web_search_uses_the_client_tool_name() {
        assert_eq!(
            hosted_web_search(&claude_search_request(Some("web_search"))).as_deref(),
            Some("web_search")
        );
        assert_eq!(
            hosted_web_search(&claude_search_request(None)).as_deref(),
            Some("web_search")
        );
        let codex = responses::parse_request(&json!({
            "model": "gpt-5-codex",
            "tools": [{"type": "web_search"}],
            "input": "q"
        }))
        .unwrap();
        assert_eq!(hosted_web_search(&codex).as_deref(), Some("web_search"));
        let mut plain = claude_search_request(None);
        plain.server_tools.clear();
        assert_eq!(hosted_web_search(&plain), None);
    }

    #[test]
    fn search_history_is_dropped_for_chat_and_gemini_upstreams() {
        let request = claude_search_request(None);
        let options = chat::RenderOptions {
            upstream_model: "m",
            default_max_output_tokens: 100,
        };
        for upstream in [Interface::OpenaiChat, Interface::Gemini] {
            let (_, _, body) = render_upstream_request(upstream, &request, &options).unwrap();
            let text = body.to_string();
            assert!(
                !text.contains("srvtoolu_1") && text.contains("answer"),
                "{}: {text}",
                upstream.as_str()
            );
        }
    }
}
