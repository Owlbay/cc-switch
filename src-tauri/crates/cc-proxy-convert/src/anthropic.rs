//! Anthropic Messages 协议（设计文档 §5）：客户端侧请求解析、响应与流式编码、错误格式。
//!
//! 上游侧（请求生成、响应与流式解析）在 Responses 客户端接入时实现。

use cc_proxy_core::Interface;
use serde_json::{json, Map, Value};

use crate::envelope;
use crate::ir::{
    Block, BlockKind, Effort, Event, MediaSource, Message, Reasoning, Request, Response, Role,
    ServerTool, ServerToolKind, Signature, StopReason, Tool, ToolChoice, Usage,
};
use crate::sse;

/// 请求解析失败（客户端请求本身不合法或无法表达）
pub type ParseError = String;

// ---------------------------------------------------------------------------
// 请求解析
// ---------------------------------------------------------------------------

/// Anthropic Messages 请求 → IR
pub fn parse_request(body: &Value) -> Result<Request, ParseError> {
    let object = body
        .as_object()
        .ok_or("request body must be a JSON object")?;
    let mut request = Request {
        model: str_field(object, "model").unwrap_or_default().to_string(),
        stream: object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        max_output_tokens: object.get("max_tokens").and_then(Value::as_u64),
        temperature: object.get("temperature").and_then(Value::as_f64),
        top_p: object.get("top_p").and_then(Value::as_f64),
        top_k: object.get("top_k").and_then(Value::as_u64),
        user: object
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        ..Default::default()
    };

    request.system = match object.get("system") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        Some(_) => return Err("system must be a string or an array of text blocks".into()),
    };

    if let Some(stops) = object.get("stop_sequences").and_then(Value::as_array) {
        request.stop = stops
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }

    for message in object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages must be an array")?
    {
        request.messages.push(parse_message(message)?);
    }

    for tool in object
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        parse_tool(tool, &mut request);
    }

    if let Some(choice) = object.get("tool_choice") {
        let (tool_choice, disable_parallel) = parse_tool_choice(choice);
        request.tool_choice = tool_choice;
        if let Some(disable) = disable_parallel {
            request.parallel_tool_calls = Some(!disable);
        }
    }

    request.reasoning = parse_reasoning(object);
    Ok(request)
}

fn str_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn parse_message(message: &Value) -> Result<Message, ParseError> {
    let role = match message.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        other => return Err(format!("unsupported message role {other:?}")),
    };
    let content = match message.get("content") {
        Some(Value::String(text)) => vec![Block::Text { text: text.clone() }],
        Some(Value::Array(blocks)) => blocks.iter().filter_map(parse_block).collect(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => return Err("message content must be a string or an array".into()),
    };
    Ok(Message { role, content })
}

fn parse_media(source: &Value) -> Option<MediaSource> {
    match source.get("type").and_then(Value::as_str)? {
        "base64" => Some(MediaSource::Base64 {
            media_type: source.get("media_type")?.as_str()?.to_string(),
            data: source.get("data")?.as_str()?.to_string(),
        }),
        "url" => Some(MediaSource::Url(source.get("url")?.as_str()?.to_string())),
        _ => None,
    }
}

/// 签名：封套按封套来源还原，否则视为来自 Anthropic 本身（值为整个块）
fn parse_signature(raw: &str, block: &Value) -> Signature {
    if envelope::looks_like(raw) {
        if let Some(signature) = envelope::decode(raw) {
            return signature;
        }
    }
    Signature {
        source: Interface::Claude,
        value: block.clone(),
    }
}

fn parse_block(block: &Value) -> Option<Block> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: block.get("text")?.as_str()?.to_string(),
        }),
        "image" => Some(Block::Image {
            source: parse_media(block.get("source")?)?,
        }),
        "document" => Some(Block::Document {
            source: parse_media(block.get("source")?)?,
            name: block
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "tool_use" => Some(Block::ToolCall {
            id: block.get("id")?.as_str()?.to_string(),
            name: block.get("name")?.as_str()?.to_string(),
            arguments: block
                .get("input")
                .map(Value::to_string)
                .unwrap_or_else(|| "{}".to_string()),
            signature: None,
        }),
        "tool_result" => Some(Block::ToolResult {
            call_id: block.get("tool_use_id")?.as_str()?.to_string(),
            content: match block.get("content") {
                Some(Value::String(text)) => vec![Block::Text { text: text.clone() }],
                Some(Value::Array(parts)) => parts.iter().filter_map(parse_block).collect(),
                _ => Vec::new(),
            },
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "thinking" => Some(Block::Thinking {
            text: block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            signature: block
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|raw| parse_signature(raw, block)),
        }),
        "redacted_thinking" => {
            let raw = block.get("data")?.as_str()?;
            Some(Block::RedactedThinking {
                signature: parse_signature(raw, block),
            })
        }
        other => {
            // server_tool_use、web_search_tool_result 等内置工具的历史块：跨协议无法表达（§5.4）
            tracing::debug!(block_type = other, "丢弃无法跨协议表达的内容块");
            None
        }
    }
}

fn parse_tool(tool: &Value, request: &mut Request) {
    let kind = tool.get("type").and_then(Value::as_str);
    match kind {
        None | Some("custom") => {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                return;
            };
            request.tools.push(Tool {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: tool.get("input_schema").cloned().unwrap_or(json!({})),
                custom: false,
            });
        }
        Some(kind) => request.server_tools.push(ServerTool {
            kind: if kind.starts_with("web_search") {
                ServerToolKind::WebSearch
            } else {
                ServerToolKind::Other
            },
            raw: tool.clone(),
        }),
    }
}

fn parse_tool_choice(choice: &Value) -> (ToolChoice, Option<bool>) {
    let disable_parallel = choice
        .get("disable_parallel_tool_use")
        .and_then(Value::as_bool);
    let tool_choice = match choice.get("type").and_then(Value::as_str) {
        Some("any") => ToolChoice::Required,
        Some("none") => ToolChoice::None,
        Some("tool") => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named(name.to_string()))
            .unwrap_or_default(),
        _ => ToolChoice::Auto,
    };
    (tool_choice, disable_parallel)
}

fn parse_reasoning(object: &Map<String, Value>) -> Reasoning {
    let effort = object
        .get("output_config")
        .and_then(|c| c.get("effort"))
        .and_then(Value::as_str)
        .and_then(Effort::parse);
    match object
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
    {
        Some("enabled") => match object
            .get("thinking")
            .and_then(|t| t.get("budget_tokens"))
            .and_then(Value::as_u64)
        {
            Some(budget) => Reasoning::Budget(budget),
            None => Reasoning::Effort(effort.unwrap_or(Effort::High)),
        },
        Some("disabled") => Reasoning::Disabled,
        Some("adaptive") => Reasoning::Adaptive(effort),
        _ => effort.map(Reasoning::Effort).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// 响应生成
// ---------------------------------------------------------------------------

fn stop_reason_str(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ToolUse => "tool_use",
        StopReason::StopSequence => "stop_sequence",
        StopReason::Refusal => "refusal",
        StopReason::Other(other) => {
            tracing::warn!(stop_reason = %other, "未知的结束原因，按 end_turn 处理");
            "end_turn"
        }
    }
}

fn usage_json(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_input_tokens": usage.cache_read_tokens,
        "cache_creation_input_tokens": usage.cache_write_tokens,
    })
}

/// 发给 Claude 客户端的签名字符串：同源时为原始签名，跨协议时为封套
fn thinking_signature(signature: &Signature) -> String {
    if signature.source == Interface::Claude {
        if let Some(raw) = signature.value.get("signature").and_then(Value::as_str) {
            return raw.to_string();
        }
    }
    envelope::encode(signature)
}

fn redacted_data(signature: &Signature) -> String {
    if signature.source == Interface::Claude {
        if let Some(raw) = signature.value.get("data").and_then(Value::as_str) {
            return raw.to_string();
        }
    }
    envelope::encode(signature)
}

/// 工具参数（JSON 文本）→ Anthropic `input` 对象；不是合法对象时用 `{}` 并告警
fn tool_input(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => value,
        _ => {
            tracing::warn!("上游返回的工具参数不是 JSON 对象，替换为 {{}}");
            json!({})
        }
    }
}

fn block_json(block: &Block) -> Option<Value> {
    Some(match block {
        Block::Text { text } => json!({ "type": "text", "text": text }),
        Block::Thinking { text, signature } => {
            let mut value = json!({ "type": "thinking", "thinking": text });
            if let Some(signature) = signature {
                value["signature"] = json!(thinking_signature(signature));
            }
            value
        }
        Block::RedactedThinking { signature } => {
            json!({ "type": "redacted_thinking", "data": redacted_data(signature) })
        }
        Block::ToolCall {
            id,
            name,
            arguments,
            ..
        } => json!({ "type": "tool_use", "id": id, "name": name, "input": tool_input(arguments) }),
        Block::Image { .. } | Block::Document { .. } | Block::ToolResult { .. } => return None,
    })
}

/// IR 响应 → Anthropic Messages 响应；`model` 用客户端请求的模型名
pub fn render_response(response: &Response, client_model: &str) -> Value {
    json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "model": client_model,
        "content": response.content.iter().filter_map(block_json).collect::<Vec<_>>(),
        "stop_reason": stop_reason_str(&response.stop_reason),
        "stop_sequence": Value::Null,
        "usage": usage_json(&response.usage),
    })
}

// ---------------------------------------------------------------------------
// 流式编码
// ---------------------------------------------------------------------------

/// 规范化事件 → Anthropic SSE 帧
pub struct StreamEncoder {
    client_model: String,
    started: bool,
    finished: bool,
    /// redacted_thinking 必须在 content_block_start 中带上数据：等到签名再输出
    pending_redacted: Option<usize>,
}

impl StreamEncoder {
    pub fn new(client_model: impl Into<String>) -> Self {
        Self {
            client_model: client_model.into(),
            started: false,
            finished: false,
            pending_redacted: None,
        }
    }

    fn emit(out: &mut Vec<bytes::Bytes>, event: &str, data: Value) {
        out.push(sse::frame(Some(event), &data.to_string()));
    }

    fn ensure_started(&mut self, out: &mut Vec<bytes::Bytes>, id: &str, usage: &Usage) {
        if self.started {
            return;
        }
        self.started = true;
        let mut usage = usage_json(usage);
        usage["output_tokens"] = json!(0);
        Self::emit(
            out,
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.client_model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": usage,
                }
            }),
        );
    }

    pub fn encode(&mut self, event: Event) -> Vec<bytes::Bytes> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        match event {
            Event::Start { id, usage, .. } => self.ensure_started(&mut out, &id, &usage),
            Event::BlockStart { index, kind } => {
                self.ensure_started(&mut out, "msg_relay", &Usage::default());
                let block = match kind {
                    BlockKind::Text => json!({ "type": "text", "text": "" }),
                    BlockKind::Thinking => json!({ "type": "thinking", "thinking": "" }),
                    BlockKind::RedactedThinking => {
                        self.pending_redacted = Some(index);
                        return out;
                    }
                    BlockKind::ToolCall { id, name } => {
                        json!({ "type": "tool_use", "id": id, "name": name, "input": {} })
                    }
                };
                Self::emit(
                    &mut out,
                    "content_block_start",
                    json!({ "type": "content_block_start", "index": index, "content_block": block }),
                );
            }
            Event::TextDelta { index, text } => Self::emit(
                &mut out,
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": index, "delta": { "type": "text_delta", "text": text } }),
            ),
            Event::ThinkingDelta { index, text } => Self::emit(
                &mut out,
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": index, "delta": { "type": "thinking_delta", "thinking": text } }),
            ),
            Event::SignatureDelta { index, signature } => {
                if self.pending_redacted == Some(index) {
                    self.pending_redacted = None;
                    Self::emit(
                        &mut out,
                        "content_block_start",
                        json!({ "type": "content_block_start", "index": index, "content_block": { "type": "redacted_thinking", "data": redacted_data(&signature) } }),
                    );
                } else {
                    Self::emit(
                        &mut out,
                        "content_block_delta",
                        json!({ "type": "content_block_delta", "index": index, "delta": { "type": "signature_delta", "signature": thinking_signature(&signature) } }),
                    );
                }
            }
            Event::ToolArgumentsDelta {
                index,
                partial_json,
            } => Self::emit(
                &mut out,
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": partial_json } }),
            ),
            Event::BlockStop { index } => {
                if self.pending_redacted == Some(index) {
                    // 没拿到数据的 redacted_thinking：无法表达，整块不输出
                    self.pending_redacted = None;
                    return out;
                }
                Self::emit(
                    &mut out,
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": index }),
                );
            }
            Event::Finish { stop_reason, usage } => {
                self.ensure_started(&mut out, "msg_relay", &Usage::default());
                Self::emit(
                    &mut out,
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": stop_reason_str(&stop_reason), "stop_sequence": Value::Null },
                        "usage": usage_json(&usage),
                    }),
                );
                Self::emit(&mut out, "message_stop", json!({ "type": "message_stop" }));
                self.finished = true;
            }
            Event::Error { kind, message } => {
                Self::emit(&mut out, "error", error_body(&kind, &message));
                self.finished = true;
            }
        }
        out
    }

    /// 上游流结束：没有正常 Finish 时补发错误事件
    pub fn finish(&mut self, error: Option<&str>) -> Vec<bytes::Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let message = error.unwrap_or("upstream stream ended before the response completed");
        let mut out = Vec::new();
        Self::emit(&mut out, "error", error_body("api_error", message));
        out
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// Anthropic 错误 body
pub fn error_body(kind: &str, message: &str) -> Value {
    json!({ "type": "error", "error": { "type": kind, "message": message } })
}

/// HTTP 状态码 → Anthropic 错误类型
pub fn error_type_for_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_code_request() -> Value {
        json!({
            "model": "claude-sonnet-5",
            "max_tokens": 32000,
            "stream": true,
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2; cch=abc\nYou are Claude Code."},
                {"type": "text", "text": "Rules", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [
                {"role": "user", "content": "list files"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "use ls", "signature": "EqQBsig"},
                    {"type": "text", "text": "Running ls."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls", "timeout": 5}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "text", "text": "a.txt"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
                    ]},
                    {"type": "text", "text": "and?"}
                ]}
            ],
            "tools": [
                {"name": "Bash", "description": "run", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}},
                {"type": "web_search_20250305", "name": "web_search", "max_uses": 5}
            ],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "metadata": {"user_id": "u-1"},
            "stop_sequences": ["END"],
            "temperature": 1
        })
    }

    #[test]
    fn parses_a_claude_code_request() {
        let request = parse_request(&claude_code_request()).unwrap();
        assert_eq!(request.model, "claude-sonnet-5");
        assert!(request.stream);
        assert_eq!(request.max_output_tokens, Some(32000));
        assert_eq!(request.system.len(), 2);
        assert_eq!(request.reasoning, Reasoning::Adaptive(Some(Effort::High)));
        assert_eq!(request.parallel_tool_calls, Some(false));
        assert_eq!(request.tool_choice, ToolChoice::Auto);
        assert_eq!(request.user.as_deref(), Some("u-1"));
        assert_eq!(request.stop, vec!["END"]);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.server_tools.len(), 1);
        assert_eq!(request.server_tools[0].kind, ServerToolKind::WebSearch);

        let assistant = &request.messages[1];
        assert_eq!(assistant.role, Role::Assistant);
        match &assistant.content[0] {
            Block::Thinking {
                text,
                signature: Some(signature),
            } => {
                assert_eq!(text, "use ls");
                assert_eq!(signature.source, Interface::Claude);
                assert_eq!(signature.value["signature"], "EqQBsig");
            }
            other => panic!("{other:?}"),
        }
        match &assistant.content[2] {
            Block::ToolCall { id, arguments, .. } => {
                assert_eq!(id, "toolu_1");
                // 键序保持（preserve_order）
                assert_eq!(arguments, r#"{"command":"ls","timeout":5}"#);
            }
            other => panic!("{other:?}"),
        }
        match &request.messages[2].content[0] {
            Block::ToolResult {
                call_id, content, ..
            } => {
                assert_eq!(call_id, "toolu_1");
                assert_eq!(content.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reasoning_forms() {
        let parse = |v: Value| parse_reasoning(v.as_object().unwrap());
        assert_eq!(
            parse(json!({"thinking": {"type": "enabled", "budget_tokens": 5000}})),
            Reasoning::Budget(5000)
        );
        assert_eq!(
            parse(json!({"thinking": {"type": "disabled"}})),
            Reasoning::Disabled
        );
        assert_eq!(
            parse(json!({"thinking": {"type": "adaptive"}})),
            Reasoning::Adaptive(None)
        );
        assert_eq!(parse(json!({})), Reasoning::Unspecified);
    }

    #[test]
    fn envelope_signatures_are_decoded() {
        let signature = Signature {
            source: Interface::OpenaiResponses,
            value: json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAA"}),
        };
        let block =
            json!({"type": "thinking", "thinking": "t", "signature": envelope::encode(&signature)});
        match parse_block(&block) {
            Some(Block::Thinking {
                signature: Some(got),
                ..
            }) => assert_eq!(got, signature),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_requests() {
        assert!(parse_request(&json!([])).is_err());
        assert!(parse_request(&json!({"model": "m"})).is_err());
        assert!(parse_request(
            &json!({"model": "m", "messages": [{"role": "system", "content": "x"}]})
        )
        .is_err());
    }

    #[test]
    fn renders_a_response() {
        let response = Response {
            id: "chatcmpl-1".into(),
            model: "deepseek-chat".into(),
            content: vec![
                Block::Thinking {
                    text: "why".into(),
                    signature: None,
                },
                Block::Text { text: "hi".into() },
                Block::ToolCall {
                    id: "call_1".into(),
                    name: "Bash".into(),
                    arguments: r#"{"b":1,"a":2}"#.into(),
                    signature: None,
                },
                Block::ToolCall {
                    id: "call_2".into(),
                    name: "Bash".into(),
                    arguments: "not json".into(),
                    signature: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 7,
                cache_read_tokens: 3,
                ..Default::default()
            },
        };
        let value = render_response(&response, "claude-sonnet-5");
        assert_eq!(value["model"], "claude-sonnet-5");
        assert_eq!(value["stop_reason"], "tool_use");
        assert_eq!(
            value["content"][0],
            json!({"type": "thinking", "thinking": "why"})
        );
        assert_eq!(value["content"][2]["input"].to_string(), r#"{"b":1,"a":2}"#);
        assert_eq!(value["content"][3]["input"], json!({}));
        assert_eq!(value["usage"]["cache_read_input_tokens"], 3);
    }

    fn frames_to_events(frames: &[bytes::Bytes]) -> Vec<(String, Value)> {
        let mut parser = sse::SseParser::new();
        frames
            .iter()
            .flat_map(|frame| parser.feed(frame))
            .map(|e| (e.event.unwrap(), serde_json::from_str(&e.data).unwrap()))
            .collect()
    }

    #[test]
    fn stream_encoder_emits_a_valid_sequence() {
        let mut encoder = StreamEncoder::new("claude-sonnet-5");
        let mut frames = Vec::new();
        for event in [
            Event::Start {
                id: "msg_1".into(),
                model: "up".into(),
                usage: Usage {
                    input_tokens: 9,
                    ..Default::default()
                },
            },
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            Event::ThinkingDelta {
                index: 0,
                text: "hmm".into(),
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::Text,
            },
            Event::TextDelta {
                index: 1,
                text: "hi".into(),
            },
            Event::BlockStop { index: 1 },
            Event::BlockStart {
                index: 2,
                kind: BlockKind::ToolCall {
                    id: "call_1".into(),
                    name: "Bash".into(),
                },
            },
            Event::ToolArgumentsDelta {
                index: 2,
                partial_json: "{\"a\":".into(),
            },
            Event::ToolArgumentsDelta {
                index: 2,
                partial_json: "1}".into(),
            },
            Event::BlockStop { index: 2 },
            Event::Finish {
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 9,
                    output_tokens: 4,
                    ..Default::default()
                },
            },
        ] {
            frames.extend(encoder.encode(event));
        }
        frames.extend(encoder.finish(None));
        let events = frames_to_events(&frames);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[0].1["message"]["model"], "claude-sonnet-5");
        assert_eq!(events[0].1["message"]["usage"]["input_tokens"], 9);
        assert_eq!(events[0].1["message"]["usage"]["output_tokens"], 0);
        assert_eq!(events[7].1["content_block"]["type"], "tool_use");
        assert_eq!(events[11].1["delta"]["stop_reason"], "tool_use");
        assert_eq!(events[11].1["usage"]["output_tokens"], 4);
    }

    #[test]
    fn stream_encoder_redacted_waits_for_data_and_errors_on_early_end() {
        let mut encoder = StreamEncoder::new("m");
        let mut frames = Vec::new();
        frames.extend(encoder.encode(Event::BlockStart {
            index: 0,
            kind: BlockKind::RedactedThinking,
        }));
        assert!(frames
            .iter()
            .all(|f| !String::from_utf8_lossy(f).contains("redacted")));
        let signature = Signature {
            source: Interface::Gemini,
            value: json!({"sig": "s", "call_id": "c"}),
        };
        frames.extend(encoder.encode(Event::SignatureDelta {
            index: 0,
            signature: signature.clone(),
        }));
        frames.extend(encoder.encode(Event::BlockStop { index: 0 }));
        frames.extend(encoder.finish(Some("upstream broke")));
        let events = frames_to_events(&frames);
        assert_eq!(events[1].1["content_block"]["type"], "redacted_thinking");
        assert_eq!(
            events[1].1["content_block"]["data"],
            envelope::encode(&signature)
        );
        assert_eq!(events.last().unwrap().0, "error");
        assert_eq!(
            events.last().unwrap().1["error"]["message"],
            "upstream broke"
        );
    }
}
