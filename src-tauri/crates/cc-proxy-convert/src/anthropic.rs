//! Anthropic Messages 协议（设计文档 §5）。
//!
//! - 客户端侧：请求解析、响应与流式编码、错误格式；
//! - 上游侧：请求生成（含 §5.2 合法性规范化）、响应与流式解析。

use std::collections::HashSet;

use cc_proxy_core::Interface;
use serde_json::{json, Map, Value};

use crate::chat::{clean_schema, RenderOptions};
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

/// 签名：封套按封套来源还原，否则视为来自 Anthropic 本身（值为整个块）。
/// 形似封套却解码失败（版本不认识、结构残缺）按“缺签名”处理（§5.5），不原样回放给上游
fn parse_signature(raw: &str, block: &Value) -> Option<Signature> {
    if envelope::looks_like(raw) {
        let decoded = envelope::decode(raw);
        if decoded.is_none() {
            tracing::warn!("无法解码的签名封套，按缺签名处理");
        }
        return decoded;
    }
    Some(Signature {
        source: Interface::Claude,
        value: block.clone(),
    })
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
                .and_then(|raw| parse_signature(raw, block)),
        }),
        "redacted_thinking" => {
            let raw = block.get("data")?.as_str()?;
            Some(Block::RedactedThinking {
                signature: parse_signature(raw, block)?,
            })
        }
        "server_tool_use" => parse_server_tool_use(block),
        "web_search_tool_result" => parse_web_search_result(block),
        other => {
            // code_execution 等其他内置工具的历史块：跨协议无法表达（§5.4）
            tracing::debug!(block_type = other, "丢弃无法跨协议表达的内容块");
            None
        }
    }
}

/// `server_tool_use` 块 → IR（请求历史与上游响应共用）
fn parse_server_tool_use(block: &Value) -> Option<Block> {
    Some(Block::ServerToolUse {
        id: block.get("id")?.as_str()?.to_string(),
        name: block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("web_search")
            .to_string(),
        input: block
            .get("input")
            .filter(|input| input.is_object())
            .cloned()
            .unwrap_or_else(|| json!({})),
    })
}

/// `web_search_tool_result` 块 → IR（请求历史与上游响应共用）
fn parse_web_search_result(block: &Value) -> Option<Block> {
    Some(Block::ServerToolResult {
        call_id: block.get("tool_use_id")?.as_str()?.to_string(),
        content: block.get("content").cloned().unwrap_or_else(|| json!([])),
    })
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

/// 带内置搜索次数的 usage：`web_search_requests` 为 server_tool_use 块数，为 0 时不写
fn usage_with_searches(usage: &Usage, web_search_requests: u64) -> Value {
    let mut value = usage_json(usage);
    if web_search_requests > 0 {
        value["server_tool_use"] = json!({ "web_search_requests": web_search_requests });
    }
    value
}

fn server_tool_use_json(id: &str, name: &str, input: &Value) -> Value {
    json!({ "type": "server_tool_use", "id": id, "name": name, "input": input })
}

fn web_search_result_json(call_id: &str, content: &Value) -> Value {
    json!({ "type": "web_search_tool_result", "tool_use_id": call_id, "content": content })
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
        Block::ServerToolUse { id, name, input } => server_tool_use_json(id, name, input),
        Block::ServerToolResult { call_id, content } => web_search_result_json(call_id, content),
        Block::Image { .. } | Block::Document { .. } | Block::ToolResult { .. } => return None,
    })
}

/// IR 响应 → Anthropic Messages 响应；`model` 用客户端请求的模型名
pub fn render_response(response: &Response, client_model: &str) -> Value {
    let searches = response
        .content
        .iter()
        .filter(|block| matches!(block, Block::ServerToolUse { .. }))
        .count() as u64;
    json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "model": client_model,
        "content": response.content.iter().filter_map(block_json).collect::<Vec<_>>(),
        "stop_reason": stop_reason_str(&response.stop_reason),
        "stop_sequence": Value::Null,
        "usage": usage_with_searches(&response.usage, searches),
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
    /// 已输出的 server_tool_use 块数（写入 usage.server_tool_use.web_search_requests）
    web_search_requests: u64,
}

impl StreamEncoder {
    pub fn new(client_model: impl Into<String>) -> Self {
        Self {
            client_model: client_model.into(),
            started: false,
            finished: false,
            pending_redacted: None,
            web_search_requests: 0,
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
                    BlockKind::ServerToolUse { id, name, input } => {
                        // 与官方流式形态一致：start 带空 input，完整 input 走一条 input_json_delta
                        self.web_search_requests += 1;
                        Self::emit(
                            &mut out,
                            "content_block_start",
                            json!({ "type": "content_block_start", "index": index, "content_block": server_tool_use_json(&id, &name, &json!({})) }),
                        );
                        Self::emit(
                            &mut out,
                            "content_block_delta",
                            json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": input.to_string() } }),
                        );
                        return out;
                    }
                    BlockKind::ServerToolResult { call_id, content } => {
                        web_search_result_json(&call_id, &content)
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
                        "usage": usage_with_searches(&usage, self.web_search_requests),
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

// ===========================================================================
// 上游侧：请求生成
// ===========================================================================

/// 首条消息不是 user 时补的占位文本
const LEADING_USER_PLACEHOLDER: &str = "(continuing the conversation)";
/// Anthropic 要求的最小 thinking 预算
const MIN_THINKING_BUDGET: u64 = 1024;

/// 是否为 adaptive thinking 模型族（按上游模型名判断）：4.6 及以上的 4.x 系列、5 及以上的
/// 各系列，以及 mythos-preview。`.` / `_` 视同 `-`，版本号只认 1～2 位数字（排除日期后缀）
pub fn is_adaptive_model(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase().replace(['.', '_'], "-");
    if normalized.contains("mythos-preview") {
        return true;
    }
    let tokens: Vec<&str> = normalized.split('-').collect();
    tokens.iter().enumerate().any(|(i, token)| {
        if !matches!(*token, "opus" | "sonnet" | "haiku" | "fable" | "mythos") {
            return false;
        }
        let Some(major) = tokens.get(i + 1).and_then(|t| version_number(t)) else {
            return false;
        };
        let minor = tokens
            .get(i + 2)
            .and_then(|t| version_number(t))
            .unwrap_or(0);
        major >= 5 || (major == 4 && minor >= 6)
    })
}

/// 取 token 开头的 1～2 位数字（允许 `6[1m]` 这类后缀）
fn version_number(token: &str) -> Option<u32> {
    let end = token
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(token.len());
    let digits = &token[..end];
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    digits.parse().ok()
}

/// 写给 Anthropic 的 effort：`xhigh` → `max`，`minimal` → `low`
fn anthropic_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Minimal | Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "max",
    }
}

/// 本次请求的 thinking 形态
#[derive(Debug, Clone, Copy, PartialEq)]
enum ThinkingPlan {
    /// 不写 `thinking`
    Omit,
    Disabled,
    Enabled(u64),
    Adaptive(Option<Effort>),
}

impl ThinkingPlan {
    fn is_on(self) -> bool {
        matches!(self, ThinkingPlan::Enabled(_) | ThinkingPlan::Adaptive(_))
    }
}

fn plan_thinking(reasoning: Reasoning, adaptive: bool, max_tokens: u64) -> ThinkingPlan {
    let budget = match reasoning {
        Reasoning::Unspecified => return ThinkingPlan::Omit,
        Reasoning::Disabled => return ThinkingPlan::Disabled,
        Reasoning::Adaptive(effort) if adaptive => return ThinkingPlan::Adaptive(effort),
        Reasoning::Effort(effort) if adaptive => return ThinkingPlan::Adaptive(Some(effort)),
        Reasoning::Budget(budget) if adaptive => {
            return ThinkingPlan::Adaptive(Some(Effort::from_budget(budget)))
        }
        Reasoning::Adaptive(effort) => effort.unwrap_or(Effort::Medium).budget_tokens(),
        Reasoning::Effort(effort) => effort.budget_tokens(),
        Reasoning::Budget(budget) => budget,
    };
    // 预算必须小于 max_tokens：钳到一半，给可见回答留空间；不抬高调用方的 max_tokens
    let budget = if budget >= max_tokens {
        max_tokens / 2
    } else {
        budget
    };
    if budget < MIN_THINKING_BUDGET {
        tracing::debug!(
            budget,
            max_tokens,
            "thinking 预算不足 1024，本次不开 thinking"
        );
        return ThinkingPlan::Omit;
    }
    ThinkingPlan::Enabled(budget)
}

/// 生成中的一条消息（块已是 Anthropic JSON）
#[derive(Debug)]
struct Turn {
    role: Role,
    blocks: Vec<Value>,
}

fn block_type(block: &Value) -> &str {
    block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn is_thinking_block(block: &Value) -> bool {
    matches!(block_type(block), "thinking" | "redacted_thinking")
}

fn media_json(source: &MediaSource) -> Value {
    match source {
        MediaSource::Base64 { media_type, data } => {
            json!({ "type": "base64", "media_type": media_type, "data": data })
        }
        MediaSource::Url(url) => json!({ "type": "url", "url": url }),
    }
}

fn document_json(source: &MediaSource, name: Option<&str>) -> Value {
    let mut value = json!({ "type": "document", "source": media_json(source) });
    if let Some(name) = name {
        value["title"] = json!(name);
    }
    value
}

/// 工具参数（JSON 文本）→ `input` 对象。空文本视为 `{}`；不是合法 JSON 对象时无法表达
fn request_tool_input(arguments: &str) -> Result<Value, String> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err("tool call arguments are not a JSON object".into()),
    }
}

/// 同源签名还原为原始块（§5.5）；不是 Anthropic 产生的签名返回 None（整块丢弃）。
/// 推理文本优先取原始块里的：签名覆盖的是原文，经其他客户端协议往返后 IR 文本可能被裁剪；
/// 原始块没有文本时才用 IR 中的 `text`
fn replay_signed_block(text: Option<&str>, signature: &Signature) -> Option<Value> {
    if signature.source != Interface::Claude {
        return None;
    }
    let raw = &signature.value;
    if block_type(raw) == "redacted_thinking" {
        let data = raw
            .get("data")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        return Some(json!({ "type": "redacted_thinking", "data": data }));
    }
    let signature = raw
        .get("signature")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let text = raw
        .get("thinking")
        .and_then(Value::as_str)
        .or(text)
        .unwrap_or_default();
    Some(json!({ "type": "thinking", "thinking": text, "signature": signature }))
}

fn tool_result_content(content: &[Block]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } if !text.trim().is_empty() => {
                Some(json!({ "type": "text", "text": text }))
            }
            Block::Image { source } => {
                Some(json!({ "type": "image", "source": media_json(source) }))
            }
            Block::Document { source, name } => Some(document_json(source, name.as_deref())),
            _ => None,
        })
        .collect()
}

/// IR 块 → Anthropic 块；该角色不能携带、或没有同源签名的块返回 None
fn render_request_block(block: &Block, role: Role) -> Result<Option<Value>, String> {
    let value = match (block, role) {
        (Block::Text { text }, _) => json!({ "type": "text", "text": text }),
        (Block::Image { source }, Role::User) => {
            json!({ "type": "image", "source": media_json(source) })
        }
        (Block::Document { source, name }, Role::User) => document_json(source, name.as_deref()),
        (
            Block::ToolCall {
                id,
                name,
                arguments,
                ..
            },
            Role::Assistant,
        ) => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": request_tool_input(arguments)?,
        }),
        (
            Block::ToolResult {
                call_id,
                content,
                is_error,
            },
            Role::User,
        ) => {
            let mut value = json!({ "type": "tool_result", "tool_use_id": call_id });
            let content = tool_result_content(content);
            if !content.is_empty() {
                value["content"] = Value::Array(content);
            }
            if *is_error {
                value["is_error"] = json!(true);
            }
            value
        }
        // 内置搜索的调用与结果都在 assistant 消息内，原样还原
        (Block::ServerToolUse { id, name, input }, Role::Assistant) => {
            server_tool_use_json(id, name, input)
        }
        (Block::ServerToolResult { call_id, content }, Role::Assistant) => {
            web_search_result_json(call_id, content)
        }
        // 无同源签名的 thinking 整块丢弃（§5.2 第 8 条）
        (Block::Thinking { text, signature }, Role::Assistant) => {
            match signature
                .as_ref()
                .and_then(|s| replay_signed_block(Some(text), s))
            {
                Some(value) => value,
                None => {
                    tracing::debug!("丢弃没有同源签名的 thinking 块");
                    return Ok(None);
                }
            }
        }
        (Block::RedactedThinking { signature }, Role::Assistant) => {
            match replay_signed_block(None, signature) {
                Some(value) => value,
                None => {
                    tracing::debug!("丢弃没有同源签名的 redacted_thinking 块");
                    return Ok(None);
                }
            }
        }
        (_, role) => {
            tracing::debug!(?role, "丢弃该角色无法携带的内容块");
            return Ok(None);
        }
    };
    Ok(Some(value))
}

/// 同一条 assistant 消息内 server_tool_use 与 web_search_tool_result 必须按 id 配对，
/// 缺一方的块（上游会拒绝）丢弃
fn drop_unpaired_server_blocks(turn: &mut Turn) {
    let uses = block_ids(turn, "server_tool_use", "id");
    let results = block_ids(turn, "web_search_tool_result", "tool_use_id");
    let before = turn.blocks.len();
    turn.blocks.retain(|block| match block_type(block) {
        "server_tool_use" => block
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| results.iter().any(|r| r == id)),
        "web_search_tool_result" => block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .is_some_and(|id| uses.iter().any(|u| u == id)),
        _ => true,
    });
    if turn.blocks.len() != before {
        tracing::debug!("丢弃不配对的 server_tool_use / web_search_tool_result 块");
    }
}

fn block_ids(turn: &Turn, kind: &str, field: &str) -> Vec<String> {
    turn.blocks
        .iter()
        .filter(|block| block_type(block) == kind)
        .map(|block| {
            block
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// tool_use 与 tool_result 的 id 一一对应（非空、无重复、集合相等）
fn tool_ids_match(uses: &[String], results: &[String]) -> bool {
    let use_set: HashSet<&str> = uses.iter().map(String::as_str).collect();
    let result_set: HashSet<&str> = results.iter().map(String::as_str).collect();
    uses.iter().chain(results).all(|id| !id.is_empty())
        && use_set.len() == uses.len()
        && result_set.len() == results.len()
        && use_set == result_set
}

fn strip_tool_results(turn: &mut Turn) {
    turn.blocks
        .retain(|block| block_type(block) != "tool_result");
}

/// 丢弃未完成工具轮：assistant 的 tool_use 必须由紧随的 user 消息全部回答，否则整条 assistant
/// 丢弃（不修改签名响应的子集），紧随 user 里的 tool_result 一并去掉；孤立的 tool_result 同样去掉
fn drop_incomplete_tool_turns(turns: Vec<Turn>) -> Vec<Turn> {
    let mut out = Vec::with_capacity(turns.len());
    let mut iter = turns.into_iter().peekable();
    while let Some(mut turn) = iter.next() {
        match turn.role {
            Role::Assistant => {
                let uses = block_ids(&turn, "tool_use", "id");
                if !uses.is_empty() {
                    let paired = iter.next_if(|next| next.role == Role::User);
                    let results = paired
                        .as_ref()
                        .map(|user| block_ids(user, "tool_result", "tool_use_id"))
                        .unwrap_or_default();
                    if tool_ids_match(&uses, &results) {
                        out.push(turn);
                        out.extend(paired);
                    } else {
                        tracing::warn!("丢弃未完成的工具轮（tool_use 与 tool_result 不配对）");
                        if let Some(mut user) = paired {
                            strip_tool_results(&mut user);
                            out.push(user);
                        }
                    }
                    continue;
                }
            }
            Role::User => strip_tool_results(&mut turn),
        }
        out.push(turn);
    }
    out
}

fn is_blank_text(block: &Value) -> bool {
    block_type(block) == "text"
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_none_or(|text| text.trim().is_empty())
}

/// 末尾 assistant 消息（预填充）的最后一个文本块去掉尾随空白，去空后整块删除
fn trim_trailing_assistant_text(turns: &mut [Turn]) {
    let Some(last) = turns.last_mut() else {
        return;
    };
    if last.role != Role::Assistant {
        return;
    }
    let Some(block) = last.blocks.last_mut() else {
        return;
    };
    if block_type(block) != "text" {
        return;
    }
    let text = block
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        last.blocks.pop();
    } else if trimmed.len() != text.len() {
        block["text"] = json!(trimmed.to_string());
    }
}

/// §5.2 规范化（顺序固定）：未完成工具轮 → 空文本与空消息 → 合并同角色 → 首条 user →
/// 末尾空白 → 空消息；最后调整块序（tool_result / thinking 在前）
fn normalize_turns(turns: Vec<Turn>) -> Result<Vec<Turn>, String> {
    let mut turns = drop_incomplete_tool_turns(turns);

    for turn in &mut turns {
        turn.blocks.retain(|block| !is_blank_text(block));
    }
    turns.retain(|turn| !turn.blocks.is_empty());

    let mut merged: Vec<Turn> = Vec::with_capacity(turns.len());
    for turn in turns {
        match merged.last_mut() {
            Some(last) if last.role == turn.role => last.blocks.extend(turn.blocks),
            _ => merged.push(turn),
        }
    }
    let mut turns = merged;

    if turns.first().is_some_and(|turn| turn.role != Role::User) {
        turns.insert(
            0,
            Turn {
                role: Role::User,
                blocks: vec![json!({ "type": "text", "text": LEADING_USER_PLACEHOLDER })],
            },
        );
    }

    trim_trailing_assistant_text(&mut turns);
    turns.retain(|turn| !turn.blocks.is_empty());
    if turns.is_empty() {
        return Err("request has no messages after normalization".into());
    }

    for turn in &mut turns {
        // 稳定排序：只把指定类型提到最前，其余保持原顺序
        match turn.role {
            Role::User => turn
                .blocks
                .sort_by_key(|block| block_type(block) != "tool_result"),
            Role::Assistant => turn.blocks.sort_by_key(|block| !is_thinking_block(block)),
        }
    }
    Ok(turns)
}

/// thinking 开启时，回答 tool_use 的 user 轮之前的 assistant 轮必须以签名 thinking 开头，
/// 否则本次只能关 thinking（§5.5 降级）。未签名的块此前已丢弃，剩下的 thinking 都带签名
fn trailing_turn_supports_thinking(turns: &[Turn]) -> bool {
    let Some(last) = turns.last() else {
        return true;
    };
    if last.role != Role::User || !last.blocks.iter().any(|b| block_type(b) == "tool_result") {
        return true;
    }
    let Some(assistant) = turns.iter().rev().find(|turn| turn.role == Role::Assistant) else {
        return true;
    };
    let has_tool_use = assistant
        .blocks
        .iter()
        .any(|block| block_type(block) == "tool_use");
    !has_tool_use || assistant.blocks.first().is_some_and(is_thinking_block)
}

/// `type` 形如 `web_search_<日期>` 的 Anthropic 内置搜索工具
fn is_anthropic_web_search(kind: &str) -> bool {
    kind.strip_prefix("web_search_")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
}

fn render_server_tool(tool: &ServerTool) -> Option<Value> {
    let kind = tool
        .raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if tool.kind == ServerToolKind::WebSearch {
        if is_anthropic_web_search(kind) {
            return Some(tool.raw.clone());
        }
        if kind.starts_with("web_search") {
            // Responses 内置搜索 → Anthropic web_search
            let mut value = json!({ "type": "web_search_20250305", "name": "web_search" });
            if let Some(max_uses) = tool
                .raw
                .get("max_tool_calls")
                .or_else(|| tool.raw.get("max_uses"))
                .and_then(Value::as_u64)
            {
                value["max_uses"] = json!(max_uses);
            }
            if let Some(domains) = tool
                .raw
                .pointer("/filters/allowed_domains")
                .filter(|d| d.is_array())
            {
                value["allowed_domains"] = domains.clone();
            }
            if let Some(location) = tool.raw.get("user_location").and_then(Value::as_object) {
                let mut mapped = Map::new();
                mapped.insert("type".into(), json!("approximate"));
                for key in ["city", "region", "country", "timezone"] {
                    if let Some(field) = location.get(key).filter(|v| v.is_string()) {
                        mapped.insert(key.into(), field.clone());
                    }
                }
                value["user_location"] = Value::Object(mapped);
            }
            return Some(value);
        }
    }
    tracing::warn!(
        tool = kind,
        "Anthropic 上游不支持该内置工具，已从请求中剔除"
    );
    None
}

fn render_tool(tool: &Tool) -> Value {
    let mut schema = clean_schema(&tool.parameters);
    // input_schema 必须是 object
    if let Some(object) = schema.as_object_mut() {
        if object.get("type") != Some(&json!("object")) {
            object.insert("type".into(), json!("object"));
        }
    }
    let mut value = Map::new();
    value.insert("name".into(), json!(tool.name));
    // description 缺失时省略而不是输出 null
    if let Some(description) = &tool.description {
        value.insert("description".into(), json!(description));
    }
    value.insert("input_schema".into(), schema);
    Value::Object(value)
}

/// IR → Anthropic Messages 请求（含 §5.2 合法性规范化）。目标协议无法表达时返回 `Err(原因)`
pub fn render_request(request: &Request, options: &RenderOptions<'_>) -> Result<Value, String> {
    let max_tokens = request
        .max_output_tokens
        .unwrap_or(options.default_max_output_tokens);

    let mut turns = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let mut blocks = Vec::with_capacity(message.content.len());
        for block in &message.content {
            if let Some(value) = render_request_block(block, message.role)? {
                blocks.push(value);
            }
        }
        let mut turn = Turn {
            role: message.role,
            blocks,
        };
        drop_unpaired_server_blocks(&mut turn);
        turns.push(turn);
    }
    let turns = normalize_turns(turns)?;

    let mut body = Map::new();
    body.insert("model".into(), json!(options.upstream_model));
    body.insert("max_tokens".into(), json!(max_tokens));

    // 上游就是 Anthropic：system 原样发送（不剥离计费行）
    let system: Vec<Value> = request
        .system
        .iter()
        .filter(|text| !text.trim().is_empty())
        .map(|text| json!({ "type": "text", "text": text }))
        .collect();
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }

    let messages = turns
        .iter()
        .map(|turn| {
            let role = match turn.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            json!({ "role": role, "content": turn.blocks })
        })
        .collect();
    body.insert("messages".into(), Value::Array(messages));

    let adaptive = is_adaptive_model(options.upstream_model);
    let mut plan = plan_thinking(request.reasoning, adaptive, max_tokens);

    let mut tools: Vec<Value> = request.tools.iter().map(render_tool).collect();
    tools.extend(request.server_tools.iter().filter_map(render_server_tool));
    // tool_choice 只在 tools 非空时发送
    if !tools.is_empty() {
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        let mut choice = match &request.tool_choice {
            ToolChoice::Auto => json!({ "type": "auto" }),
            ToolChoice::None => json!({ "type": "none" }),
            ToolChoice::Required => json!({ "type": "any" }),
            ToolChoice::Named(name) if names.contains(&name.as_str()) => {
                json!({ "type": "tool", "name": name })
            }
            // 指向不存在（或被剔除）的工具时退回 auto
            ToolChoice::Named(_) => json!({ "type": "auto" }),
        };
        if matches!(block_type(&choice), "any" | "tool") && plan.is_on() {
            // thinking 开启时不能强制工具：保留调用方的工具约束，本次不开 thinking
            tracing::debug!("强制 tool_choice 与 thinking 互斥，本次不开 thinking");
            plan = ThinkingPlan::Omit;
        }
        if request.parallel_tool_calls == Some(false) && block_type(&choice) != "none" {
            choice["disable_parallel_tool_use"] = json!(true);
        }
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), choice);
    }

    if plan.is_on() && !trailing_turn_supports_thinking(&turns) {
        tracing::warn!("最后一轮 tool_use 之前没有可回放的签名 thinking，本次关闭 thinking");
        plan = ThinkingPlan::Disabled;
    }
    match plan {
        ThinkingPlan::Omit => {}
        ThinkingPlan::Disabled => {
            body.insert("thinking".into(), json!({ "type": "disabled" }));
        }
        ThinkingPlan::Enabled(budget) => {
            body.insert(
                "thinking".into(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
        }
        ThinkingPlan::Adaptive(effort) => {
            body.insert("thinking".into(), json!({ "type": "adaptive" }));
            if let Some(effort) = effort {
                body.insert(
                    "output_config".into(),
                    json!({ "effort": anthropic_effort(effort) }),
                );
            }
        }
    }

    // thinking 开启时不发采样参数
    if !plan.is_on() {
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        // 较新的 Claude 模型不接受同时指定 temperature 与 top_p：两者都有时只保留 temperature
        match (request.temperature, request.top_p) {
            (Some(_), Some(_)) => tracing::debug!("temperature 与 top_p 同时存在，丢弃 top_p"),
            (None, Some(top_p)) => {
                body.insert("top_p".into(), json!(top_p));
            }
            _ => {}
        }
        if let Some(top_k) = request.top_k {
            body.insert("top_k".into(), json!(top_k));
        }
    }
    if !request.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(request.stop));
    }
    if let Some(user) = &request.user {
        body.insert("metadata".into(), json!({ "user_id": user }));
    }
    if request.stream {
        body.insert("stream".into(), json!(true));
    }
    Ok(Value::Object(body))
}

// ===========================================================================
// 上游侧：响应解析
// ===========================================================================

fn parse_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        None | Some("end_turn") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("tool_use") => StopReason::ToolUse,
        Some("stop_sequence") => StopReason::StopSequence,
        Some("refusal") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_string()),
    }
}

fn parse_usage(usage: &Value) -> Usage {
    let number = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: number("input_tokens"),
        output_tokens: number("output_tokens"),
        cache_read_tokens: number("cache_read_input_tokens"),
        cache_write_tokens: number("cache_creation_input_tokens"),
        reasoning_tokens: None,
    }
}

/// 上游返回的 thinking / redacted_thinking 块：签名来源为 Anthropic，值为整个块
fn upstream_signature(block: &Value) -> Signature {
    Signature {
        source: Interface::Claude,
        value: block.clone(),
    }
}

fn parse_response_block(block: &Value) -> Option<Block> {
    match block_type(block) {
        "text" => Some(Block::Text {
            text: block.get("text")?.as_str()?.to_string(),
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
                .map(|_| upstream_signature(block)),
        }),
        "redacted_thinking" => {
            block.get("data")?.as_str()?;
            Some(Block::RedactedThinking {
                signature: upstream_signature(block),
            })
        }
        "tool_use" => Some(Block::ToolCall {
            id: block.get("id")?.as_str()?.to_string(),
            name: block.get("name")?.as_str()?.to_string(),
            arguments: block
                .get("input")
                .map(Value::to_string)
                .unwrap_or_else(|| "{}".to_string()),
            signature: None,
        }),
        "server_tool_use" => parse_server_tool_use(block),
        "web_search_tool_result" => parse_web_search_result(block),
        other => {
            // 其他内置工具的结果块：暂不映射
            tracing::debug!(block_type = other, "丢弃上游响应中暂不支持的内容块");
            None
        }
    }
}

/// Anthropic 非流式响应 → IR。2xx 内 `type: error` 时返回 Err
pub fn parse_response(body: &Value) -> Result<Response, String> {
    if block_type(body) == "error" {
        return Err(error_message(body).unwrap_or_else(|| body.to_string()));
    }
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or("anthropic response has no content")?;
    Ok(Response {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg")
            .to_string(),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content: content.iter().filter_map(parse_response_block).collect(),
        stop_reason: parse_stop_reason(body.get("stop_reason").and_then(Value::as_str)),
        usage: body.get("usage").map(parse_usage).unwrap_or_default(),
    })
}

/// 从 Anthropic 错误 body 取 message（`{type:error, error:{message}}`，也接受顶层 message）
pub fn error_message(body: &Value) -> Option<String> {
    body.pointer("/error/message")
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ===========================================================================
// 上游侧：流式解码
// ===========================================================================

#[derive(Debug)]
enum OpenBlock {
    Text,
    Thinking {
        text: String,
        signature: String,
    },
    ToolUse {
        /// content_block_start 里的 input
        input: Value,
        /// 是否收到过非空 input_json_delta
        streamed: bool,
    },
    RedactedThinking,
    /// server_tool_use：input 通常以 input_json_delta 流式给出，而 IR 要求载荷一次到齐，
    /// 故到 content_block_stop 才分配 IR index 并整体输出
    ServerToolUse {
        id: String,
        name: String,
        /// content_block_start 里的 input
        input: Value,
        /// 累积的 input_json_delta
        partial: String,
    },
}

/// 延迟分配 IR index 的块（server_tool_use）在 `open` 中的占位 index
const DEFERRED_INDEX: usize = usize::MAX;

/// 认识的 SSE event 名；其余（含缺失）以 `data.type` 为准
const STREAM_EVENTS: [&str; 8] = [
    "message_start",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
    "ping",
    "error",
];

/// Anthropic SSE → 规范化事件。上游 index 重新映射为连续的 IR index（跳过的块不占 index）
#[derive(Debug, Default)]
pub struct StreamDecoder {
    started: bool,
    finished: bool,
    next_index: usize,
    /// (上游 index, IR index, 块状态)
    open: Vec<(u64, usize, OpenBlock)>,
    start_usage: Usage,
    final_usage: Option<Usage>,
    stop_reason: Option<String>,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一个 SSE 事件。`event` 为 SSE event 名（为 None 或不认识时看 `data.type`）
    pub fn feed(&mut self, event: Option<&str>, data: &str) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            tracing::debug!("忽略无法解析的 Anthropic 流数据帧");
            return out;
        };
        let kind = event
            .filter(|name| STREAM_EVENTS.contains(name))
            .unwrap_or_else(|| block_type(&value));
        match kind {
            "message_start" => self.message_start(&value, &mut out),
            "content_block_start" => self.block_start(&value, &mut out),
            "content_block_delta" => self.block_delta(&value, &mut out),
            "content_block_stop" => {
                let upstream = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(position) = self.open.iter().position(|(i, ..)| *i == upstream) {
                    self.close(position, &mut out);
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(usage) = value.get("usage") {
                    self.final_usage = Some(self.merge_usage(usage));
                }
            }
            "message_stop" => self.finalize(&mut out),
            "ping" => {}
            "error" => {
                out.push(Event::Error {
                    kind: value
                        .pointer("/error/type")
                        .and_then(Value::as_str)
                        .unwrap_or("api_error")
                        .to_string(),
                    message: error_message(&value).unwrap_or_else(|| value.to_string()),
                });
                self.finished = true;
            }
            other => tracing::debug!(event = other, "忽略未知的 Anthropic 流事件"),
        }
        out
    }

    /// 上游流结束：没收到 message_stop 时产生 Error 事件（已结束则为空）
    pub fn finish(&mut self) -> Vec<Event> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![Event::Error {
            kind: "api_error".into(),
            message: "upstream stream ended before the response completed".into(),
        }]
    }

    fn ensure_started(&mut self, id: &str, model: &str, out: &mut Vec<Event>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(Event::Start {
            id: id.to_string(),
            model: model.to_string(),
            usage: Usage {
                output_tokens: 0,
                ..self.start_usage
            },
        });
    }

    fn message_start(&mut self, value: &Value, out: &mut Vec<Event>) {
        if self.started {
            return;
        }
        let message = value.get("message").unwrap_or(&Value::Null);
        self.start_usage = message.get("usage").map(parse_usage).unwrap_or_default();
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg")
            .to_string();
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.ensure_started(&id, &model, out);
    }

    /// message_delta 的 usage 与 message_start 的合并：输入侧取非零者，输出取 delta
    fn merge_usage(&self, usage: &Value) -> Usage {
        let delta = parse_usage(usage);
        let start = self.start_usage;
        let pick = |new: u64, old: u64| if new != 0 { new } else { old };
        Usage {
            input_tokens: pick(delta.input_tokens, start.input_tokens),
            output_tokens: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(start.output_tokens),
            cache_read_tokens: pick(delta.cache_read_tokens, start.cache_read_tokens),
            cache_write_tokens: pick(delta.cache_write_tokens, start.cache_write_tokens),
            reasoning_tokens: None,
        }
    }

    fn block_start(&mut self, value: &Value, out: &mut Vec<Event>) {
        let upstream = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let block = value.get("content_block").unwrap_or(&Value::Null);
        let string = |key: &str| {
            block
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        if self.open.iter().any(|(i, ..)| *i == upstream) {
            tracing::debug!(index = upstream, "忽略重复的 content_block_start");
            return;
        }
        match block_type(block) {
            "server_tool_use" => {
                self.ensure_started("msg", "", out);
                self.open.push((
                    upstream,
                    DEFERRED_INDEX,
                    OpenBlock::ServerToolUse {
                        id: string("id"),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("web_search")
                            .to_string(),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                        partial: String::new(),
                    },
                ));
                return;
            }
            "web_search_tool_result" => {
                // 结果在 content_block_start 中完整给出，没有 delta：直接输出整块，
                // 随后的 content_block_stop 找不到打开的块而被忽略
                if let Some(Block::ServerToolResult { call_id, content }) =
                    parse_web_search_result(block)
                {
                    self.ensure_started("msg", "", out);
                    let index = self.next_index;
                    self.next_index += 1;
                    out.push(Event::BlockStart {
                        index,
                        kind: BlockKind::ServerToolResult { call_id, content },
                    });
                    out.push(Event::BlockStop { index });
                }
                return;
            }
            _ => {}
        }
        let (kind, state) = match block_type(block) {
            "text" => (BlockKind::Text, OpenBlock::Text),
            "thinking" => (
                BlockKind::Thinking,
                OpenBlock::Thinking {
                    text: String::new(),
                    signature: String::new(),
                },
            ),
            "tool_use" => (
                BlockKind::ToolCall {
                    id: string("id"),
                    name: string("name"),
                },
                OpenBlock::ToolUse {
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                    streamed: false,
                },
            ),
            "redacted_thinking" => (BlockKind::RedactedThinking, OpenBlock::RedactedThinking),
            other => {
                // 其他内置工具的结果块等：暂不映射，不占 IR index
                tracing::debug!(block_type = other, "跳过上游流中暂不支持的内容块");
                return;
            }
        };
        self.ensure_started("msg", "", out);
        let index = self.next_index;
        self.next_index += 1;
        out.push(Event::BlockStart { index, kind });
        self.open.push((upstream, index, state));
        let position = self.open.len() - 1;
        // content_block_start 自带的内容按 delta 处理
        match block_type(block) {
            "text" => self.apply_delta(position, "text_delta", &string("text"), out),
            "thinking" => {
                self.apply_delta(position, "thinking_delta", &string("thinking"), out);
                self.apply_delta(position, "signature_delta", &string("signature"), out);
            }
            "redacted_thinking" => out.push(Event::SignatureDelta {
                index,
                signature: upstream_signature(block),
            }),
            _ => {}
        }
    }

    fn block_delta(&mut self, value: &Value, out: &mut Vec<Event>) {
        let upstream = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let Some(position) = self.open.iter().position(|(i, ..)| *i == upstream) else {
            return; // 跳过的块或未开的块
        };
        let delta = value.get("delta").unwrap_or(&Value::Null);
        let kind = block_type(delta);
        let field = match kind {
            "text_delta" => "text",
            "thinking_delta" => "thinking",
            "input_json_delta" => "partial_json",
            "signature_delta" => "signature",
            other => {
                tracing::debug!(delta_type = other, "忽略未知的 content_block_delta");
                return;
            }
        };
        let text = delta.get(field).and_then(Value::as_str).unwrap_or_default();
        self.apply_delta(position, kind, text, out);
    }

    fn apply_delta(&mut self, position: usize, kind: &str, text: &str, out: &mut Vec<Event>) {
        if text.is_empty() {
            return;
        }
        let (_, index, state) = &mut self.open[position];
        let index = *index;
        match (kind, state) {
            ("text_delta", OpenBlock::Text) => out.push(Event::TextDelta {
                index,
                text: text.to_string(),
            }),
            ("thinking_delta", OpenBlock::Thinking { text: acc, .. }) => {
                acc.push_str(text);
                out.push(Event::ThinkingDelta {
                    index,
                    text: text.to_string(),
                });
            }
            // 签名累积到块结束再整体输出一次
            ("signature_delta", OpenBlock::Thinking { signature, .. }) => signature.push_str(text),
            ("input_json_delta", OpenBlock::ServerToolUse { partial, .. }) => {
                partial.push_str(text)
            }
            ("input_json_delta", OpenBlock::ToolUse { streamed, .. }) => {
                *streamed = true;
                out.push(Event::ToolArgumentsDelta {
                    index,
                    partial_json: text.to_string(),
                });
            }
            (kind, _) => tracing::debug!(delta_type = kind, "delta 类型与块类型不符，已忽略"),
        }
    }

    fn close(&mut self, position: usize, out: &mut Vec<Event>) {
        let (_, index, state) = self.open.remove(position);
        match state {
            OpenBlock::ServerToolUse {
                id,
                name,
                input,
                partial,
            } => {
                let input = if partial.trim().is_empty() {
                    input
                } else {
                    serde_json::from_str::<Value>(&partial).unwrap_or_else(|_| {
                        tracing::warn!("上游 server_tool_use 的 input 不是合法 JSON，替换为 {{}}");
                        json!({})
                    })
                };
                let input = if input.is_object() { input } else { json!({}) };
                let index = self.next_index;
                self.next_index += 1;
                out.push(Event::BlockStart {
                    index,
                    kind: BlockKind::ServerToolUse { id, name, input },
                });
                out.push(Event::BlockStop { index });
                return;
            }
            OpenBlock::Thinking { text, signature } if !signature.is_empty() => {
                out.push(Event::SignatureDelta {
                    index,
                    signature: Signature {
                        source: Interface::Claude,
                        value: json!({ "type": "thinking", "thinking": text, "signature": signature }),
                    },
                });
            }
            // 参数全部在 content_block_start 里给出、没有 input_json_delta
            OpenBlock::ToolUse {
                input,
                streamed: false,
            } if input.as_object().is_some_and(|o| !o.is_empty()) => {
                out.push(Event::ToolArgumentsDelta {
                    index,
                    partial_json: input.to_string(),
                });
            }
            _ => {}
        }
        out.push(Event::BlockStop { index });
    }

    fn finalize(&mut self, out: &mut Vec<Event>) {
        self.ensure_started("msg", "", out);
        // 没收到 content_block_stop 的块在结束前补关
        while !self.open.is_empty() {
            self.close(0, out);
        }
        self.finished = true;
        out.push(Event::Finish {
            stop_reason: parse_stop_reason(self.stop_reason.as_deref()),
            usage: self.final_usage.unwrap_or(self.start_usage),
        });
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

    // -----------------------------------------------------------------------
    // 上游侧
    // -----------------------------------------------------------------------

    fn opts(model: &str) -> RenderOptions<'_> {
        RenderOptions {
            upstream_model: model,
            default_max_output_tokens: 16384,
        }
    }

    fn text(text: &str) -> Block {
        Block::Text { text: text.into() }
    }

    fn user(content: Vec<Block>) -> Message {
        Message {
            role: Role::User,
            content,
        }
    }

    fn assistant(content: Vec<Block>) -> Message {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    fn tool_call(id: &str) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: "Bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
            signature: None,
        }
    }

    fn tool_result(id: &str) -> Block {
        Block::ToolResult {
            call_id: id.into(),
            content: vec![text("ok")],
            is_error: false,
        }
    }

    fn claude_thinking(text: &str, signature: &str) -> Block {
        Block::Thinking {
            text: text.into(),
            signature: Some(Signature {
                source: Interface::Claude,
                value: json!({"type": "thinking", "thinking": text, "signature": signature}),
            }),
        }
    }

    fn bash_tool() -> Tool {
        Tool {
            name: "Bash".into(),
            description: Some("run".into()),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            custom: false,
        }
    }

    fn simple(messages: Vec<Message>) -> Request {
        Request {
            model: "claude-sonnet-4-5".into(),
            messages,
            ..Default::default()
        }
    }

    fn render(request: &Request, model: &str) -> Value {
        render_request(request, &opts(model)).unwrap()
    }

    #[test]
    fn adaptive_model_detection() {
        for yes in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-sonnet-4.6",
            "claude-opus-4-7-20260101",
            "claude-opus-4-6[1m]",
            "us.anthropic.claude-opus-4-6-v1",
            "claude-sonnet-5",
            "claude-opus-5-1",
            "claude-fable-5",
            "claude-mythos-preview",
        ] {
            assert!(is_adaptive_model(yes), "{yes}");
        }
        for no in [
            "claude-sonnet-4-5",
            "claude-opus-4-1-20250805",
            "claude-sonnet-4-20250514",
            "claude-3-7-sonnet-20250219",
            "claude-haiku-4-5",
            "gpt-5",
        ] {
            assert!(!is_adaptive_model(no), "{no}");
        }
    }

    #[test]
    fn renders_basic_request_fields_and_tools() {
        let request = Request {
            system: vec![
                "x-anthropic-billing-header: cch=1\n\nYou are Claude Code.".into(),
                "  ".into(),
                "Rules".into(),
            ],
            messages: vec![user(vec![
                text("look"),
                Block::Image {
                    source: MediaSource::Url("https://x/a.png".into()),
                },
                Block::Document {
                    source: MediaSource::Base64 {
                        media_type: "application/pdf".into(),
                        data: "JVBER".into(),
                    },
                    name: Some("a.pdf".into()),
                },
            ])],
            tools: vec![
                Tool {
                    name: "Nothing".into(),
                    description: None,
                    parameters: json!({"type": "string", "properties": {"u": {"type": "string", "format": "uri"}}}),
                    custom: false,
                },
                Tool {
                    name: "apply_patch".into(),
                    description: Some("patch".into()),
                    parameters: json!({"type": "object", "properties": {"input": {"type": "string"}}}),
                    custom: true,
                },
            ],
            parallel_tool_calls: Some(false),
            stop: vec!["END".into()],
            user: Some("u-1".into()),
            stream: true,
            temperature: Some(0.5),
            top_p: Some(0.9),
            top_k: Some(5),
            ..simple(vec![])
        };
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 16384);
        // 计费行保留，空 system 丢弃
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "x-anthropic-billing-header: cch=1\n\nYou are Claude Code."},
                {"type": "text", "text": "Rules"}
            ])
        );
        let content = &body["messages"][0]["content"];
        assert_eq!(
            content[1],
            json!({"type": "image", "source": {"type": "url", "url": "https://x/a.png"}})
        );
        assert_eq!(
            content[2],
            json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBER"}, "title": "a.pdf"})
        );
        assert_eq!(
            body["tools"][0],
            json!({"name": "Nothing", "input_schema": {"type": "object", "properties": {"u": {"type": "string"}}}})
        );
        assert_eq!(body["tools"][1]["name"], "apply_patch");
        assert_eq!(body["tools"][1]["description"], "patch");
        assert_eq!(
            body["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );
        assert_eq!(body["stop_sequences"], json!(["END"]));
        assert_eq!(body["metadata"], json!({"user_id": "u-1"}));
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.5);
        assert!(
            body.get("top_p").is_none(),
            "top_p dropped next to temperature"
        );
        assert_eq!(body["top_k"], 5);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn thinking_budget_is_clamped() {
        let mut request = simple(vec![user(vec![text("hi")])]);
        request.temperature = Some(1.0);

        // 预算 ≥ max_tokens：钳到一半
        request.reasoning = Reasoning::Budget(16000);
        request.max_output_tokens = Some(16000);
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 8000})
        );
        assert!(body.get("temperature").is_none());

        // 小于 max_tokens：原样
        request.reasoning = Reasoning::Effort(Effort::Low);
        request.max_output_tokens = None;
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(body["thinking"]["budget_tokens"], 2048);

        // 钳后不足 1024：不开 thinking，恢复温度
        request.reasoning = Reasoning::Effort(Effort::High);
        request.max_output_tokens = Some(1500);
        let body = render(&request, "claude-sonnet-4-5");
        assert!(body.get("thinking").is_none());
        assert_eq!(body["temperature"], 1.0);
    }

    #[test]
    fn thinking_excludes_sampling_and_forced_tool_choice() {
        let mut request = simple(vec![user(vec![text("hi")])]);
        request.reasoning = Reasoning::Effort(Effort::Medium);
        request.temperature = Some(0.2);
        request.top_p = Some(0.8);
        request.top_k = Some(3);
        request.tools = vec![bash_tool()];

        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 8192})
        );
        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
        for key in ["temperature", "top_p", "top_k"] {
            assert!(body.get(key).is_none(), "{key}");
        }

        for (choice, expected) in [
            (ToolChoice::Required, json!({"type": "any"})),
            (
                ToolChoice::Named("Bash".into()),
                json!({"type": "tool", "name": "Bash"}),
            ),
        ] {
            request.tool_choice = choice;
            let body = render(&request, "claude-opus-4-6");
            assert_eq!(body["tool_choice"], expected);
            assert!(body.get("thinking").is_none());
            assert!(body.get("output_config").is_none());
            assert_eq!(body["temperature"], 0.2);
            assert_eq!(body["top_k"], 3);
        }

        // 指向不存在的工具 → auto，不算强制
        request.tool_choice = ToolChoice::Named("Missing".into());
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
        assert_eq!(body["thinking"]["type"], "enabled");

        request.tool_choice = ToolChoice::None;
        request.parallel_tool_calls = Some(false);
        assert_eq!(
            render(&request, "claude-sonnet-4-5")["tool_choice"],
            json!({"type": "none"})
        );

        // 没有工具时不写 tool_choice
        request.tools.clear();
        request.tool_choice = ToolChoice::Required;
        let body = render(&request, "claude-sonnet-4-5");
        assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn reasoning_forms_for_adaptive_and_budget_models() {
        let mut request = simple(vec![user(vec![text("hi")])]);
        let cases = [
            (
                Reasoning::Adaptive(Some(Effort::XHigh)),
                "claude-opus-4-6",
                json!({"type": "adaptive"}),
                Some("max"),
            ),
            (
                Reasoning::Adaptive(None),
                "claude-opus-4-6",
                json!({"type": "adaptive"}),
                None,
            ),
            (
                Reasoning::Effort(Effort::Minimal),
                "claude-sonnet-5",
                json!({"type": "adaptive"}),
                Some("low"),
            ),
            (
                Reasoning::Budget(16000),
                "claude-sonnet-4-6",
                json!({"type": "adaptive"}),
                Some("high"),
            ),
            (
                Reasoning::Adaptive(None),
                "claude-sonnet-4-5",
                json!({"type": "enabled", "budget_tokens": 8192}),
                None,
            ),
            (
                Reasoning::Adaptive(Some(Effort::XHigh)),
                "claude-sonnet-4-5",
                json!({"type": "enabled", "budget_tokens": 8192}),
                None,
            ),
            (
                Reasoning::Disabled,
                "claude-opus-4-6",
                json!({"type": "disabled"}),
                None,
            ),
        ];
        for (reasoning, model, thinking, effort) in cases {
            request.reasoning = reasoning;
            let body = render(&request, model);
            assert_eq!(body["thinking"], thinking, "{reasoning:?} {model}");
            assert_eq!(
                body.pointer("/output_config/effort")
                    .and_then(Value::as_str),
                effort,
                "{reasoning:?} {model}"
            );
        }
        // XHigh 预算 24576 ≥ 16384 → 钳到 8192（上面第 6 例）；Unspecified 不写
        request.reasoning = Reasoning::Unspecified;
        let body = render(&request, "claude-opus-4-6");
        assert!(body.get("thinking").is_none() && body.get("output_config").is_none());
    }

    #[test]
    fn message_normalization() {
        let request = simple(vec![
            // 首条是 assistant → 补占位 user
            assistant(vec![text("earlier answer")]),
            // 空文本与空消息丢弃
            user(vec![text("   ")]),
            user(vec![]),
            user(vec![text("q1")]),
            // 相邻同角色合并
            user(vec![text(""), text("q2")]),
            assistant(vec![
                text("partial"),
                claude_thinking("t", "sig"),
                tool_call("toolu_1"),
            ]),
            user(vec![text("after"), tool_result("toolu_1")]),
            // 末尾 assistant 预填充：尾随空白裁掉
            assistant(vec![text("prefill  \n")]),
        ]);
        let body = render(&request, "claude-sonnet-4-5");
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            vec![
                "user",
                "assistant",
                "user",
                "assistant",
                "user",
                "assistant"
            ]
        );
        assert_eq!(
            messages[0]["content"],
            json!([{"type": "text", "text": LEADING_USER_PLACEHOLDER}])
        );
        assert_eq!(
            messages[2]["content"],
            json!([{"type": "text", "text": "q1"}, {"type": "text", "text": "q2"}])
        );
        // thinking 排最前，其余保持原序
        let types: Vec<&str> = messages[3]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["thinking", "text", "tool_use"]);
        assert_eq!(
            messages[3]["content"][0],
            json!({"type": "thinking", "thinking": "t", "signature": "sig"})
        );
        assert_eq!(messages[3]["content"][2]["input"], json!({"command": "ls"}));
        // tool_result 排最前
        assert_eq!(messages[4]["content"][0]["type"], "tool_result");
        assert_eq!(
            messages[4]["content"][0]["content"],
            json!([{"type": "text", "text": "ok"}])
        );
        assert_eq!(messages[4]["content"][1]["text"], "after");
        assert_eq!(
            messages[5]["content"],
            json!([{"type": "text", "text": "prefill"}])
        );

        // 仅空白的末尾预填充整条删除
        let request = simple(vec![user(vec![text("q")]), assistant(vec![text("  ")])]);
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);

        // 规范化后为空 → Err
        assert!(render_request(&simple(vec![user(vec![text(" ")])]), &opts("m")).is_err());
        assert!(render_request(&simple(vec![]), &opts("m")).is_err());
    }

    #[test]
    fn incomplete_tool_turns_are_dropped() {
        let request = simple(vec![
            user(vec![text("go")]),
            // 没有对应 tool_result：整条 assistant 丢弃
            assistant(vec![text("calling"), tool_call("toolu_1")]),
            user(vec![text("never mind")]),
            // 部分回答：assistant 丢弃，user 中的 tool_result 去掉
            assistant(vec![tool_call("toolu_2"), tool_call("toolu_3")]),
            user(vec![tool_result("toolu_2"), text("still here")]),
            // 孤立的 tool_result
            user(vec![tool_result("toolu_9")]),
            // 完整的工具轮保留
            assistant(vec![tool_call("toolu_4")]),
            user(vec![tool_result("toolu_4")]),
            // 末尾未回答的 tool_use
            assistant(vec![tool_call("toolu_5")]),
        ]);
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "go"},
                    {"type": "text", "text": "never mind"},
                    {"type": "text", "text": "still here"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_4", "name": "Bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_4", "content": [{"type": "text", "text": "ok"}]}
                ]}
            ])
        );
    }

    #[test]
    fn tool_arguments_must_be_an_object() {
        let mut call = tool_call("toolu_1");
        if let Block::ToolCall { arguments, .. } = &mut call {
            *arguments = "[1,2]".into();
        }
        let request = simple(vec![
            user(vec![text("go")]),
            assistant(vec![call]),
            user(vec![tool_result("toolu_1")]),
        ]);
        assert!(render_request(&request, &opts("m")).is_err());

        // 空参数视为 {}
        let request = simple(vec![
            user(vec![text("go")]),
            assistant(vec![Block::ToolCall {
                id: "toolu_1".into(),
                name: "Bash".into(),
                arguments: String::new(),
                signature: None,
            }]),
            user(vec![Block::ToolResult {
                call_id: "toolu_1".into(),
                content: vec![text(" ")],
                is_error: true,
            }]),
        ]);
        let body = render(&request, "m");
        assert_eq!(body["messages"][1]["content"][0]["input"], json!({}));
        assert_eq!(
            body["messages"][2]["content"][0],
            json!({"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true})
        );
    }

    fn tool_turn_request(first_assistant_blocks: Vec<Block>) -> Request {
        let mut blocks = first_assistant_blocks;
        blocks.push(tool_call("toolu_1"));
        Request {
            reasoning: Reasoning::Adaptive(Some(Effort::High)),
            ..simple(vec![
                user(vec![text("go")]),
                assistant(blocks),
                user(vec![tool_result("toolu_1")]),
            ])
        }
    }

    #[test]
    fn same_source_signatures_are_replayed() {
        let request = tool_turn_request(vec![
            claude_thinking("plan", "sig-1"),
            Block::RedactedThinking {
                signature: Signature {
                    source: Interface::Claude,
                    value: json!({"type": "redacted_thinking", "data": "opaque"}),
                },
            },
        ]);
        let body = render(&request, "claude-opus-4-6");
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(body["output_config"], json!({"effort": "high"}));
        let content = &body["messages"][1]["content"];
        assert_eq!(
            content[0],
            json!({"type": "thinking", "thinking": "plan", "signature": "sig-1"})
        );
        assert_eq!(
            content[1],
            json!({"type": "redacted_thinking", "data": "opaque"})
        );
        assert_eq!(content[2]["type"], "tool_use");
    }

    #[test]
    fn foreign_or_missing_signatures_drop_blocks_and_downgrade_thinking() {
        let foreign = Signature {
            source: Interface::OpenaiResponses,
            value: json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAA"}),
        };
        let request = tool_turn_request(vec![
            Block::Thinking {
                text: "from responses".into(),
                signature: Some(foreign.clone()),
            },
            Block::Thinking {
                text: "no signature".into(),
                signature: None,
            },
            Block::RedactedThinking { signature: foreign },
        ]);
        let body = render(&request, "claude-opus-4-6");
        // 跨源 / 无签名的块全部丢弃
        assert_eq!(
            body["messages"][1]["content"],
            json!([{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}])
        );
        // 最后一轮 tool_use 前没有签名 thinking → 关 thinking，删 effort
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("output_config").is_none());

        // 非 adaptive 模型同理
        let mut request = tool_turn_request(vec![]);
        request.reasoning = Reasoning::Budget(4096);
        request.temperature = Some(0.3);
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert_eq!(body["temperature"], 0.3);

        // 最后一条是普通 user 提问：不降级
        let mut request = tool_turn_request(vec![]);
        request.messages.push(assistant(vec![text("done")]));
        request.messages.push(user(vec![text("next")]));
        let body = render(&request, "claude-opus-4-6");
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    }

    #[test]
    fn server_tools_from_both_client_protocols() {
        let mut request = simple(vec![user(vec![text("search")])]);
        request.server_tools = vec![
            ServerTool {
                kind: ServerToolKind::WebSearch,
                raw: json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5, "cache_control": {"type": "ephemeral"}}),
            },
            ServerTool {
                kind: ServerToolKind::Other,
                raw: json!({"type": "code_execution_20250522", "name": "code_execution"}),
            },
        ];
        request.tool_choice = ToolChoice::Named("web_search".into());
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(
            body["tools"],
            json!([{"type": "web_search_20250305", "name": "web_search", "max_uses": 5, "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "tool", "name": "web_search"})
        );

        request.server_tools = vec![ServerTool {
            kind: ServerToolKind::WebSearch,
            raw: json!({
                "type": "web_search",
                "max_tool_calls": 3,
                "filters": {"allowed_domains": ["example.com"]},
                "user_location": {"type": "approximate", "country": "US", "city": null}
            }),
        }];
        request.tool_choice = ToolChoice::Auto;
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(
            body["tools"],
            json!([{
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": 3,
                "allowed_domains": ["example.com"],
                "user_location": {"type": "approximate", "country": "US"}
            }])
        );

        // 只有被剔除的内置工具：tools 与 tool_choice 都不写
        request.server_tools = vec![ServerTool {
            kind: ServerToolKind::Other,
            raw: json!({"type": "computer_20250124"}),
        }];
        let body = render(&request, "claude-sonnet-4-5");
        assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
    }

    #[test]
    fn parses_an_upstream_response() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-6",
            "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "q"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []},
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"b": 1, "a": 2}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 30, "cache_creation_input_tokens": 40}
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.id, "msg_1");
        assert_eq!(response.model, "claude-opus-4-6");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_write_tokens: 40,
                reasoning_tokens: None
            }
        );
        assert_eq!(
            response.content,
            vec![
                Block::Thinking {
                    text: "hmm".into(),
                    signature: Some(Signature {
                        source: Interface::Claude,
                        value: json!({"type": "thinking", "thinking": "hmm", "signature": "sig"}),
                    }),
                },
                Block::RedactedThinking {
                    signature: Signature {
                        source: Interface::Claude,
                        value: json!({"type": "redacted_thinking", "data": "opaque"}),
                    },
                },
                Block::ServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: json!({"query": "q"}),
                },
                Block::ServerToolResult {
                    call_id: "srvtoolu_1".into(),
                    content: json!([]),
                },
                text("hi"),
                Block::ToolCall {
                    id: "toolu_1".into(),
                    name: "Bash".into(),
                    arguments: r#"{"b":1,"a":2}"#.into(),
                    signature: None,
                },
            ]
        );
        // 回放：解析结果再生成请求，签名块原样还原
        let replay = render_request(
            &simple(vec![
                user(vec![text("q")]),
                assistant(response.content.clone()),
                user(vec![tool_result("toolu_1")]),
            ]),
            &opts("claude-opus-4-6"),
        )
        .unwrap();
        assert_eq!(
            replay["messages"][1]["content"][0],
            json!({"type": "thinking", "thinking": "hmm", "signature": "sig"})
        );

        let error = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
        assert_eq!(parse_response(&error), Err("Overloaded".to_string()));
        assert_eq!(error_message(&error).as_deref(), Some("Overloaded"));
        assert_eq!(
            parse_stop_reason(Some("pause_turn")),
            StopReason::Other("pause_turn".into())
        );
    }

    /// 一条完整的上游流：thinking（签名分片）、参数分片的 server_tool_use 与搜索结果、redacted、text、
    /// 参数分片的 tool_use、只在 start 里给参数的 tool_use
    const UPSTREAM_STREAM: &[(&str, &str)] = &[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-6","content":[],"stop_reason":null,"usage":{"input_tokens":12,"cache_read_input_tokens":30,"cache_creation_input_tokens":4,"output_tokens":1}}}"#,
        ),
        ("ping", r#"{"type":"ping"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQB"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"more"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"q\"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":"https://example.com","title":"Example","encrypted_content":"enc"}]}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":2}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":3}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"你好"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":4}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":5,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":5}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":6,"content_block":{"type":"tool_use","id":"toolu_2","name":"Read","input":{"path":"a"}}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":6}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":42}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];

    fn expected_stream_events() -> Vec<Event> {
        vec![
            Event::Start {
                id: "msg_1".into(),
                model: "claude-opus-4-6".into(),
                usage: Usage {
                    input_tokens: 12,
                    cache_read_tokens: 30,
                    cache_write_tokens: 4,
                    ..Default::default()
                },
            },
            Event::BlockStart {
                index: 0,
                kind: BlockKind::Thinking,
            },
            Event::ThinkingDelta {
                index: 0,
                text: "Let me ".into(),
            },
            Event::ThinkingDelta {
                index: 0,
                text: "think".into(),
            },
            Event::SignatureDelta {
                index: 0,
                signature: Signature {
                    source: Interface::Claude,
                    value: json!({"type": "thinking", "thinking": "Let me think", "signature": "EqQBmore"}),
                },
            },
            Event::BlockStop { index: 0 },
            Event::BlockStart {
                index: 1,
                kind: BlockKind::ServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: json!({"query": "q"}),
                },
            },
            Event::BlockStop { index: 1 },
            Event::BlockStart {
                index: 2,
                kind: BlockKind::ServerToolResult {
                    call_id: "srvtoolu_1".into(),
                    content: json!([{"type": "web_search_result", "url": "https://example.com", "title": "Example", "encrypted_content": "enc"}]),
                },
            },
            Event::BlockStop { index: 2 },
            Event::BlockStart {
                index: 3,
                kind: BlockKind::RedactedThinking,
            },
            Event::SignatureDelta {
                index: 3,
                signature: Signature {
                    source: Interface::Claude,
                    value: json!({"type": "redacted_thinking", "data": "opaque"}),
                },
            },
            Event::BlockStop { index: 3 },
            Event::BlockStart {
                index: 4,
                kind: BlockKind::Text,
            },
            Event::TextDelta {
                index: 4,
                text: "你好".into(),
            },
            Event::BlockStop { index: 4 },
            Event::BlockStart {
                index: 5,
                kind: BlockKind::ToolCall {
                    id: "toolu_1".into(),
                    name: "Bash".into(),
                },
            },
            Event::ToolArgumentsDelta {
                index: 5,
                partial_json: "{\"command\":".into(),
            },
            Event::ToolArgumentsDelta {
                index: 5,
                partial_json: "\"ls\"}".into(),
            },
            Event::BlockStop { index: 5 },
            Event::BlockStart {
                index: 6,
                kind: BlockKind::ToolCall {
                    id: "toolu_2".into(),
                    name: "Read".into(),
                },
            },
            Event::ToolArgumentsDelta {
                index: 6,
                partial_json: r#"{"path":"a"}"#.into(),
            },
            Event::BlockStop { index: 6 },
            Event::Finish {
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 42,
                    cache_read_tokens: 30,
                    cache_write_tokens: 4,
                    reasoning_tokens: None,
                },
            },
        ]
    }

    fn decode_pairs(pairs: &[(&str, &str)], with_event_names: bool) -> Vec<Event> {
        let mut decoder = StreamDecoder::new();
        let mut events: Vec<Event> = pairs
            .iter()
            .flat_map(|(name, data)| decoder.feed(with_event_names.then_some(*name), data))
            .collect();
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn stream_decoder_full_sequence() {
        assert_eq!(
            decode_pairs(UPSTREAM_STREAM, true),
            expected_stream_events()
        );
        // 没有 event 名时按 data.type
        assert_eq!(
            decode_pairs(UPSTREAM_STREAM, false),
            expected_stream_events()
        );
    }

    #[test]
    fn stream_usage_merge_prefers_nonzero_input_fields() {
        let events = decode_pairs(
            &[
                (
                    "message_start",
                    r#"{"type":"message_start","message":{"id":"m","model":"x","usage":{"input_tokens":0,"cache_read_input_tokens":7,"output_tokens":1}}}"#,
                ),
                (
                    "message_delta",
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":55,"cache_read_input_tokens":0,"output_tokens":9}}"#,
                ),
                ("message_stop", r#"{"type":"message_stop"}"#),
            ],
            true,
        );
        assert_eq!(
            events.last(),
            Some(&Event::Finish {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 55,
                    output_tokens: 9,
                    cache_read_tokens: 7,
                    ..Default::default()
                }
            })
        );
    }

    #[test]
    fn stream_without_message_stop_or_with_error_event() {
        let truncated = &UPSTREAM_STREAM[..UPSTREAM_STREAM.len() - 1];
        let events = decode_pairs(truncated, true);
        assert!(matches!(
            events.last(),
            Some(Event::Error { kind, .. }) if kind == "api_error"
        ));
        assert!(!events.iter().any(|e| matches!(e, Event::Finish { .. })));

        let events = decode_pairs(
            &[
                ("message_start", UPSTREAM_STREAM[0].1),
                (
                    "error",
                    r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                ),
                ("message_stop", r#"{"type":"message_stop"}"#),
            ],
            true,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            Event::Error {
                kind: "overloaded_error".into(),
                message: "Overloaded".into()
            }
        );
    }

    #[test]
    fn stream_decoding_is_independent_of_chunking() {
        let text: String = UPSTREAM_STREAM
            .iter()
            .map(|(name, data)| format!("event: {name}\r\ndata: {data}\r\n\r\n"))
            .collect();
        let bytes = text.as_bytes();
        let decode_chunks = |size: usize| {
            let mut parser = sse::SseParser::new();
            let mut decoder = StreamDecoder::new();
            let mut events = Vec::new();
            for chunk in bytes.chunks(size) {
                for sse_event in parser.feed(chunk) {
                    events.extend(decoder.feed(sse_event.event.as_deref(), &sse_event.data));
                }
            }
            for sse_event in parser.finish() {
                events.extend(decoder.feed(sse_event.event.as_deref(), &sse_event.data));
            }
            events.extend(decoder.finish());
            events
        };
        let expected = expected_stream_events();
        for size in 1..=bytes.len() {
            assert_eq!(decode_chunks(size), expected, "chunk size {size}");
        }
    }

    // ---- 内置搜索（server_tool_use / web_search_tool_result）----

    fn search_blocks(id: &str) -> Vec<Block> {
        vec![
            Block::ServerToolUse {
                id: id.into(),
                name: "web_search".into(),
                input: json!({"query": "rust"}),
            },
            Block::ServerToolResult {
                call_id: id.into(),
                content: json!([{"type": "web_search_result", "url": "https://a.dev", "title": "A", "encrypted_content": "enc"}]),
            },
        ]
    }

    #[test]
    fn renders_server_tool_blocks_and_search_usage() {
        let mut content = search_blocks("srvtoolu_1");
        content.push(Block::Text { text: "ok".into() });
        let response = Response {
            id: "msg_1".into(),
            content,
            ..Default::default()
        };
        let value = render_response(&response, "m");
        assert_eq!(
            value["content"][0],
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "rust"}})
        );
        assert_eq!(value["content"][1]["type"], "web_search_tool_result");
        assert_eq!(value["content"][1]["tool_use_id"], "srvtoolu_1");
        assert_eq!(value["content"][1]["content"][0]["url"], "https://a.dev");
        assert_eq!(value["stop_reason"], "end_turn");
        assert_eq!(
            value["usage"]["server_tool_use"],
            json!({"web_search_requests": 1})
        );

        // 没有搜索时不写 server_tool_use
        let plain = render_response(&Response::default(), "m");
        assert!(plain["usage"].get("server_tool_use").is_none());
    }

    #[test]
    fn stream_encoder_emits_server_tool_blocks() {
        let mut encoder = StreamEncoder::new("m");
        let mut events = vec![Event::Start {
            id: "msg_1".into(),
            model: "up".into(),
            usage: Usage::default(),
        }];
        events.extend(
            crate::ir::response_events(&Response {
                content: search_blocks("srvtoolu_1"),
                ..Default::default()
            })[1..]
                .iter()
                .cloned(),
        );
        let frames: Vec<bytes::Bytes> = events
            .into_iter()
            .flat_map(|event| encoder.encode(event))
            .collect();
        let events = frames_to_events(&frames);
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            events[1].1["content_block"],
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}})
        );
        assert_eq!(
            events[2].1["delta"],
            json!({"type": "input_json_delta", "partial_json": r#"{"query":"rust"}"#})
        );
        assert_eq!(events[4].1["index"], 1);
        assert_eq!(
            events[4].1["content_block"]["type"],
            "web_search_tool_result"
        );
        assert_eq!(
            events[4].1["content_block"]["content"][0]["encrypted_content"],
            "enc"
        );
        assert_eq!(events[6].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(
            events[6].1["usage"]["server_tool_use"]["web_search_requests"],
            1
        );
    }

    #[test]
    fn search_history_is_parsed_and_replayed_to_anthropic() {
        let body = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 1000,
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            "messages": [
                {"role": "user", "content": "search rust"},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "rust"}},
                    {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                        {"type": "web_search_result", "url": "https://a.dev", "title": "A", "encrypted_content": "enc", "page_age": "1d"}
                    ]},
                    {"type": "text", "text": "Rust is a language."}
                ]},
                {"role": "user", "content": "more"}
            ]
        });
        let request = parse_request(&body).unwrap();
        let assistant = &request.messages[1].content;
        assert_eq!(
            assistant[0],
            Block::ServerToolUse {
                id: "srvtoolu_1".into(),
                name: "web_search".into(),
                input: json!({"query": "rust"}),
            }
        );
        assert!(
            matches!(&assistant[1], Block::ServerToolResult { call_id, content }
            if call_id == "srvtoolu_1" && content[0]["page_age"] == "1d")
        );

        let rendered = render_request(&request, &opts("claude-sonnet-4-5")).unwrap();
        assert_eq!(
            rendered["messages"][1]["content"], body["messages"][1]["content"],
            "search blocks are replayed verbatim"
        );
    }

    #[test]
    fn unpaired_or_misplaced_search_blocks_are_dropped() {
        let request = simple(vec![
            user(vec![text("q")]),
            assistant(vec![
                // 只有调用没有结果
                Block::ServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: json!({}),
                },
                // 只有结果没有调用
                Block::ServerToolResult {
                    call_id: "srvtoolu_2".into(),
                    content: json!([]),
                },
                text("answer"),
            ]),
            // user 消息不能携带搜索块
            user(search_blocks("srvtoolu_3")),
            user(vec![text("next")]),
        ]);
        let body = render(&request, "claude-sonnet-4-5");
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "answer"}]},
                {"role": "user", "content": [{"type": "text", "text": "next"}]}
            ])
        );
    }

    #[test]
    fn stream_server_tool_use_input_only_in_start() {
        let events = decode_pairs(
            &[
                (
                    "message_start",
                    r#"{"type":"message_start","message":{"id":"msg_1","model":"m","usage":{"input_tokens":1}}}"#,
                ),
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"q"}}}"#,
                ),
                (
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":0}"#,
                ),
                (
                    "message_delta",
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3,"server_tool_use":{"web_search_requests":1}}}"#,
                ),
                ("message_stop", r#"{"type":"message_stop"}"#),
            ],
            true,
        );
        assert_eq!(
            events[1],
            Event::BlockStart {
                index: 0,
                kind: BlockKind::ServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: json!({"query": "q"}),
                },
            }
        );
        assert_eq!(events[2], Event::BlockStop { index: 0 });
        assert!(matches!(events[3], Event::Finish { .. }));
    }
}
