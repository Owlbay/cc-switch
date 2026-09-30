//! OpenAI Responses 协议（设计文档 §5）：客户端侧请求解析、响应与流式编码、错误格式。
//!
//! 客户端为 Codex CLI。上游侧（请求生成、响应与流式解析）在 Responses 上游接入时实现。

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
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

/// OpenAI Responses 请求 → IR。无法表达的请求返回 Err（调用方映射为 Unsupported）
pub fn parse_request(body: &Value) -> Result<Request, ParseError> {
    let object = body
        .as_object()
        .ok_or("request body must be a JSON object")?;
    reject_unsupported(object)?;

    let mut request = Request {
        model: str_field(object, "model").unwrap_or_default().to_string(),
        stream: object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        max_output_tokens: object.get("max_output_tokens").and_then(Value::as_u64),
        temperature: object.get("temperature").and_then(Value::as_f64),
        top_p: object.get("top_p").and_then(Value::as_f64),
        parallel_tool_calls: object.get("parallel_tool_calls").and_then(Value::as_bool),
        user: str_field(object, "user")
            .or_else(|| str_field(object, "safety_identifier"))
            .map(str::to_string),
        ..Default::default()
    };

    match object.get("instructions") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => {
            if !text.is_empty() {
                request.system.push(text.clone());
            }
        }
        Some(_) => return Err("instructions must be a string".into()),
    }

    match object.get("input") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => push_block(
            &mut request.messages,
            Role::User,
            Block::Text { text: text.clone() },
        ),
        Some(Value::Array(items)) => {
            for item in items {
                parse_item(item, &mut request)?;
            }
        }
        Some(_) => return Err("input must be a string or an array".into()),
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
        request.tool_choice = parse_tool_choice(choice);
    }

    request.reasoning = parse_reasoning(object);
    Ok(request)
}

fn str_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

/// 目标协议无法表达、静默丢弃会改变结果的字段返回 Err；其余 Codex 默认字段静默忽略（§5.4）
fn reject_unsupported(object: &Map<String, Value>) -> Result<(), ParseError> {
    if str_field(object, "previous_response_id").is_some_and(|id| !id.is_empty()) {
        return Err("previous_response_id cannot be resolved on a converted upstream".into());
    }
    match object.get("conversation") {
        None | Some(Value::Null) => {}
        Some(Value::String(id)) if id.is_empty() => {}
        Some(_) => return Err("conversation cannot be resolved on a converted upstream".into()),
    }
    if object
        .get("n")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 1)
    {
        return Err("n > 1 is not supported".into());
    }
    if object.get("logprobs").and_then(Value::as_bool) == Some(true)
        || object
            .get("top_logprobs")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
    {
        return Err("logprobs are not supported".into());
    }
    if let Some(format) = object
        .get("text")
        .and_then(|t| t.get("format"))
        .and_then(|f| f.get("type"))
        .and_then(Value::as_str)
    {
        if matches!(format, "json_schema" | "json_object") {
            return Err(format!("text.format {format} is not supported"));
        }
    }
    if object.get("store").and_then(Value::as_bool) == Some(true) {
        tracing::warn!("Responses 请求带 store: true，转换后的上游不会保存响应");
    }
    Ok(())
}

/// 追加到最后一条同角色消息，否则新开一条（连续的同角色项合并）
fn push_block(messages: &mut Vec<Message>, role: Role, block: Block) {
    match messages.last_mut() {
        Some(last) if last.role == role => last.content.push(block),
        _ => messages.push(Message {
            role,
            content: vec![block],
        }),
    }
}

fn parse_item(item: &Value, request: &mut Request) -> Result<(), ParseError> {
    let kind = item.get("type").and_then(Value::as_str);
    let incomplete = item.get("status").and_then(Value::as_str) == Some("incomplete");
    match kind {
        Some("message") => parse_message_item(item, request),
        None if item.get("role").is_some() => parse_message_item(item, request),
        None => {
            tracing::warn!("丢弃既无 type 也无 role 的 input 项");
            Ok(())
        }
        Some("input_text" | "input_image" | "input_file") => {
            for block in parse_parts(std::slice::from_ref(item))? {
                push_block(&mut request.messages, Role::User, block);
            }
            Ok(())
        }
        Some(kind @ ("function_call" | "custom_tool_call")) if incomplete => {
            tracing::warn!(item_type = kind, "丢弃 status: incomplete 的历史工具调用");
            Ok(())
        }
        Some("function_call") => {
            let call_id = call_id(item);
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            let raw = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = if raw.trim().is_empty() {
                "{}".to_string()
            } else {
                match serde_json::from_str::<Value>(raw) {
                    Ok(Value::Object(_)) => raw.to_string(),
                    _ => {
                        return Err(format!(
                            "function_call arguments for '{name}' must be a JSON object"
                        ))
                    }
                }
            };
            push_block(
                &mut request.messages,
                Role::Assistant,
                Block::ToolCall {
                    id: call_id,
                    name: name.to_string(),
                    arguments,
                    signature: None,
                },
            );
            Ok(())
        }
        Some("custom_tool_call") => {
            let input = item.get("input").cloned().unwrap_or_else(|| json!(""));
            push_block(
                &mut request.messages,
                Role::Assistant,
                Block::ToolCall {
                    id: call_id(item),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: json!({ "input": input }).to_string(),
                    signature: None,
                },
            );
            Ok(())
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let content = tool_output(item.get("output"))?;
            push_block(
                &mut request.messages,
                Role::User,
                Block::ToolResult {
                    call_id: call_id(item),
                    content,
                    is_error: false,
                },
            );
            Ok(())
        }
        Some("reasoning") => {
            if let Some(block) = parse_reasoning_item(item) {
                push_block(&mut request.messages, Role::Assistant, block);
            }
            Ok(())
        }
        Some("item_reference") => {
            tracing::warn!("item_reference 无法在转换后的上游解引用，已丢弃");
            Ok(())
        }
        Some(other) => {
            // web_search_call、local_shell_call 等内置工具的历史项：有文本降级为文本，否则丢弃
            match item_text(item) {
                Some(text) => {
                    tracing::warn!(item_type = other, "内置工具历史项降级为文本");
                    let role = if other.ends_with("_output") {
                        Role::User
                    } else {
                        Role::Assistant
                    };
                    push_block(&mut request.messages, role, Block::Text { text });
                }
                None => tracing::warn!(item_type = other, "丢弃无法跨协议表达的 input 项"),
            }
            Ok(())
        }
    }
}

/// `call_id`，缺失时退回 `id`
fn call_id(item: &Value) -> String {
    item.get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

fn parse_message_item(item: &Value, request: &mut Request) -> Result<(), ParseError> {
    let role = match item.get("role").and_then(Value::as_str) {
        None | Some("user") => Some(Role::User),
        Some("assistant") => Some(Role::Assistant),
        Some("system" | "developer") => None,
        Some(other) => return Err(format!("unsupported message role {other:?}")),
    };
    let blocks = match item.get("content") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => vec![Block::Text { text: text.clone() }],
        Some(Value::Array(parts)) => parse_parts(parts)?,
        Some(_) => return Err("message content must be a string or an array".into()),
    };
    match role {
        Some(role) => {
            for block in blocks {
                push_block(&mut request.messages, role, block);
            }
        }
        None => {
            // system / developer 消息并入系统提示，只取文本
            let text = blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                request.system.push(text);
            }
        }
    }
    Ok(())
}

/// message content / 工具输出中的 parts
fn parse_parts(parts: &[Value]) -> Result<Vec<Block>, ParseError> {
    let mut blocks = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    blocks.push(Block::Text {
                        text: text.to_string(),
                    });
                }
            }
            Some("refusal") => {
                if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                    blocks.push(Block::Text {
                        text: text.to_string(),
                    });
                }
            }
            Some("input_image") => blocks.push(Block::Image {
                source: parse_image(part)?,
            }),
            Some("input_file") => {
                return Err("input_file is not supported on a converted upstream".into())
            }
            other => tracing::debug!(part_type = ?other, "丢弃无法跨协议表达的内容 part"),
        }
    }
    Ok(blocks)
}

/// `input_image.image_url`：data URL 解析为 Base64，其余按 URL 原样传递
fn parse_image(part: &Value) -> Result<MediaSource, ParseError> {
    let url = match part.get("image_url") {
        Some(Value::String(url)) => Some(url.as_str()),
        Some(object) => object.get("url").and_then(Value::as_str),
        None => None,
    }
    .filter(|url| !url.is_empty())
    .ok_or("input_image without image_url (file_id) is not supported")?;
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((header, data)) = rest.split_once(',') {
            if let Some(media_type) = header.strip_suffix(";base64") {
                return Ok(MediaSource::Base64 {
                    media_type: media_type.to_string(),
                    data: data.to_string(),
                });
            }
        }
    }
    Ok(MediaSource::Url(url.to_string()))
}

fn tool_output(output: Option<&Value>) -> Result<Vec<Block>, ParseError> {
    Ok(match output {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => vec![Block::Text { text: text.clone() }],
        Some(Value::Array(parts)) => parse_parts(parts)?,
        Some(other) => vec![Block::Text {
            text: other.to_string(),
        }],
    })
}

/// reasoning 项 → Thinking / RedactedThinking（§5.5）；既无签名又无摘要时丢弃
fn parse_reasoning_item(item: &Value) -> Option<Block> {
    let text = item
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n");
    let signature = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            // 本中转的封套按封套来源还原，否则视为来自 Responses 本身（值为整个 reasoning 项）
            envelope::looks_like(raw)
                .then(|| envelope::decode(raw))
                .flatten()
                .unwrap_or_else(|| Signature {
                    source: Interface::OpenaiResponses,
                    value: item.clone(),
                })
        });
    match (signature, text.is_empty()) {
        (signature, false) => Some(Block::Thinking { text, signature }),
        (Some(signature), true) => Some(Block::RedactedThinking { signature }),
        (None, true) => None,
    }
}

/// 未知项中可降级的文本
fn item_text(item: &Value) -> Option<String> {
    for key in ["text", "output", "content"] {
        match item.get(key) {
            Some(Value::String(text)) if !text.is_empty() => return Some(text.clone()),
            Some(Value::Array(parts)) => {
                let text = parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    return Some(text);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_tool(tool: &Value, request: &mut Request) {
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);
    match tool.get("type").and_then(Value::as_str) {
        Some("function") => {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                return;
            };
            request.tools.push(Tool {
                name: name.to_string(),
                description,
                parameters: tool.get("parameters").cloned().unwrap_or(json!({})),
                custom: false,
            });
        }
        Some("custom") => {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                return;
            };
            // 自由文本工具（如 apply_patch）降级为参数 {input: string} 的 function 工具（§5.2）
            request.tools.push(Tool {
                name: name.to_string(),
                description,
                parameters: json!({
                    "type": "object",
                    "properties": { "input": { "type": "string" } },
                    "required": ["input"]
                }),
                custom: true,
            });
        }
        Some(kind @ ("web_search" | "web_search_preview")) => {
            tracing::debug!(tool_type = kind, "web_search 工具记为内置能力");
            request.server_tools.push(ServerTool {
                kind: ServerToolKind::WebSearch,
                raw: tool.clone(),
            });
        }
        other => tracing::warn!(tool_type = ?other, "剔除无法跨协议表达的内置工具"),
    }
}

fn parse_tool_choice(choice: &Value) -> ToolChoice {
    match choice {
        Value::String(mode) => match mode.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Value::Object(_) => match choice.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => choice
                .get("name")
                .and_then(Value::as_str)
                .map(|name| ToolChoice::Named(name.to_string()))
                .unwrap_or_default(),
            _ => ToolChoice::Auto,
        },
        _ => ToolChoice::Auto,
    }
}

fn parse_reasoning(object: &Map<String, Value>) -> Reasoning {
    let Some(effort) = object
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    else {
        return Reasoning::Unspecified;
    };
    if effort.trim().eq_ignore_ascii_case("none") {
        return Reasoning::Disabled;
    }
    match Effort::parse(effort) {
        Some(effort) => Reasoning::Effort(effort),
        None => {
            tracing::warn!(effort, "无法识别的 reasoning.effort，交由上游决定");
            Reasoning::Unspecified
        }
    }
}

// ---------------------------------------------------------------------------
// 响应生成
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// 响应 id：统一 `resp_` 前缀；上游没给 id 时按时间生成
fn response_id(id: &str) -> String {
    if id.is_empty() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("resp_relay{nanos:x}")
    } else if id.starts_with("resp_") {
        id.to_string()
    } else {
        format!("resp_{id}")
    }
}

/// message / reasoning 项的 id：`<前缀>_<响应 id 主体>_<output_index>`，同一响应内唯一
fn output_item_id(prefix: &str, response_id: &str, output_index: usize) -> String {
    let base = response_id.strip_prefix("resp_").unwrap_or(response_id);
    format!("{prefix}_{base}_{output_index}")
}

fn tool_item_id(call_id: &str, custom: bool) -> String {
    if custom {
        format!("ctc_{call_id}")
    } else {
        format!("fc_{call_id}")
    }
}

fn usage_json(usage: &Usage) -> Value {
    let input = usage.total_input();
    json!({
        "input_tokens": input,
        "input_tokens_details": { "cached_tokens": usage.cache_read_tokens },
        "output_tokens": usage.output_tokens,
        "output_tokens_details": { "reasoning_tokens": usage.reasoning_tokens.unwrap_or(0) },
        "total_tokens": input + usage.output_tokens,
    })
}

/// stop reason → (`status`, `incomplete_details`)
fn status_for(reason: &StopReason) -> (&'static str, Value) {
    match reason {
        StopReason::MaxTokens => ("incomplete", json!({ "reason": "max_output_tokens" })),
        StopReason::Refusal => ("incomplete", json!({ "reason": "content_filter" })),
        StopReason::EndTurn | StopReason::ToolUse | StopReason::StopSequence => {
            ("completed", Value::Null)
        }
        StopReason::Other(other) => {
            tracing::debug!(stop_reason = %other, "未知的结束原因，按 completed 处理");
            ("completed", Value::Null)
        }
    }
}

struct ResponseObject<'a> {
    id: &'a str,
    created_at: u64,
    status: &'a str,
    model: &'a str,
    output: Vec<Value>,
    usage: Value,
    incomplete_details: Value,
}

impl ResponseObject<'_> {
    fn into_value(self) -> Value {
        json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": self.status,
            "model": self.model,
            "output": self.output,
            "usage": self.usage,
            "incomplete_details": self.incomplete_details,
        })
    }
}

fn message_item(id: &str, status: &str, text: Option<&str>) -> Value {
    let content = match text {
        Some(text) => json!([{ "type": "output_text", "text": text, "annotations": [] }]),
        None => json!([]),
    };
    json!({ "id": id, "type": "message", "status": status, "role": "assistant", "content": content })
}

fn output_text_part(text: &str) -> Value {
    json!({ "type": "output_text", "text": text, "annotations": [] })
}

/// 完成的 reasoning 项（不带 status）。签名来自 Responses 本身时回放原 `encrypted_content`
/// 并沿用原 id，否则写封套
fn reasoning_item(id: &str, text: &str, signature: Option<&Signature>) -> Value {
    let summary = if text.is_empty() {
        json!([])
    } else {
        json!([{ "type": "summary_text", "text": text }])
    };
    let mut item = json!({ "id": id, "type": "reasoning", "summary": summary });
    if let Some(signature) = signature {
        let original = (signature.source == Interface::OpenaiResponses
            && signature.value.get("type").and_then(Value::as_str) == Some("reasoning"))
        .then(|| {
            signature
                .value
                .get("encrypted_content")
                .and_then(Value::as_str)
        })
        .flatten();
        match original {
            Some(encrypted) => {
                if let Some(original_id) = signature.value.get("id").and_then(Value::as_str) {
                    item["id"] = json!(original_id);
                }
                item["encrypted_content"] = json!(encrypted);
            }
            None => item["encrypted_content"] = json!(envelope::encode(signature)),
        }
    }
    item
}

/// custom 工具的 `input`：参数中的 `input` 字符串，取不到时用整段参数文本
fn custom_input(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(object)) => match object.get("input") {
            Some(Value::String(input)) => input.clone(),
            _ => arguments.to_string(),
        },
        _ => arguments.to_string(),
    }
}

fn tool_call_item(call_id: &str, name: &str, arguments: &str, custom: bool, status: &str) -> Value {
    let id = tool_item_id(call_id, custom);
    if custom {
        json!({
            "id": id,
            "type": "custom_tool_call",
            "status": status,
            "call_id": call_id,
            "name": name,
            "input": custom_input(arguments),
        })
    } else {
        json!({
            "id": id,
            "type": "function_call",
            "status": status,
            "call_id": call_id,
            "name": name,
            "arguments": arguments,
        })
    }
}

fn is_custom(custom_tool_names: &[String], name: &str) -> bool {
    custom_tool_names.iter().any(|n| n == name)
}

/// IR 响应 → Responses 响应对象；`model` 用客户端模型名；`custom_tool_names` 中的工具调用
/// 还原为 `custom_tool_call`
pub fn render_response(
    response: &Response,
    client_model: &str,
    custom_tool_names: &[String],
) -> Value {
    let id = response_id(&response.id);
    let mut output = Vec::new();
    for block in &response.content {
        let index = output.len();
        let item = match block {
            Block::Text { text } => {
                message_item(&output_item_id("msg", &id, index), "completed", Some(text))
            }
            Block::Thinking { text, signature } => {
                reasoning_item(&output_item_id("rs", &id, index), text, signature.as_ref())
            }
            Block::RedactedThinking { signature } => {
                reasoning_item(&output_item_id("rs", &id, index), "", Some(signature))
            }
            Block::ToolCall {
                id: call_id,
                name,
                arguments,
                ..
            } => tool_call_item(
                call_id,
                name,
                arguments,
                is_custom(custom_tool_names, name),
                "completed",
            ),
            Block::Image { .. } | Block::Document { .. } | Block::ToolResult { .. } => continue,
        };
        output.push(item);
    }
    let (status, incomplete_details) = status_for(&response.stop_reason);
    let model = if client_model.is_empty() {
        &response.model
    } else {
        client_model
    };
    ResponseObject {
        id: &id,
        created_at: now_secs(),
        status,
        model,
        output,
        usage: usage_json(&response.usage),
        incomplete_details,
    }
    .into_value()
}

// ---------------------------------------------------------------------------
// 流式编码
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum OpenKind {
    Text,
    Thinking {
        /// 已发出 reasoning_summary_part.added（首个非空推理增量时才开）
        summary_open: bool,
    },
    /// 没有可见内容：等到 BlockStop 拿到签名再一次性输出 added + done
    Redacted,
    ToolCall {
        call_id: String,
        name: String,
        custom: bool,
    },
}

#[derive(Debug)]
struct OpenBlock {
    kind: OpenKind,
    output_index: usize,
    item_id: String,
    text: String,
    signature: Option<Signature>,
}

/// 规范化事件 → Responses SSE 帧
pub struct StreamEncoder {
    client_model: String,
    custom_tool_names: Vec<String>,
    created_at: u64,
    response_id: String,
    model: String,
    started: bool,
    finished: bool,
    sequence_number: u64,
    start_usage: Usage,
    next_output_index: usize,
    /// IR 块序号 → 未结束的输出项
    open: BTreeMap<usize, OpenBlock>,
    /// 已完成的输出项（按 output_index）
    output: BTreeMap<usize, Value>,
}

impl StreamEncoder {
    pub fn new(client_model: impl Into<String>, custom_tool_names: Vec<String>) -> Self {
        Self {
            client_model: client_model.into(),
            custom_tool_names,
            created_at: now_secs(),
            response_id: String::new(),
            model: String::new(),
            started: false,
            finished: false,
            sequence_number: 0,
            start_usage: Usage::default(),
            next_output_index: 0,
            open: BTreeMap::new(),
            output: BTreeMap::new(),
        }
    }

    /// 输出一帧；`type` 与递增的 `sequence_number` 写在最前
    fn emit(&mut self, out: &mut Vec<Bytes>, kind: &str, data: Value) {
        let mut object = Map::new();
        object.insert("type".into(), json!(kind));
        object.insert("sequence_number".into(), json!(self.sequence_number));
        self.sequence_number += 1;
        if let Value::Object(fields) = data {
            object.extend(fields);
        }
        out.push(sse::frame(Some(kind), &Value::Object(object).to_string()));
    }

    fn response_object(&self, status: &str, usage: Value, incomplete_details: Value) -> Value {
        ResponseObject {
            id: &self.response_id,
            created_at: self.created_at,
            status,
            model: &self.model,
            output: self.output.values().cloned().collect(),
            usage,
            incomplete_details,
        }
        .into_value()
    }

    fn ensure_started(&mut self, out: &mut Vec<Bytes>, id: &str, upstream_model: &str) {
        if self.started {
            return;
        }
        self.started = true;
        self.response_id = response_id(id);
        self.model = if self.client_model.is_empty() {
            upstream_model.to_string()
        } else {
            self.client_model.clone()
        };
        let response = self.response_object("in_progress", Value::Null, Value::Null);
        self.emit(out, "response.created", json!({ "response": response }));
        self.emit(out, "response.in_progress", json!({ "response": response }));
    }

    fn next_output_index(&mut self) -> usize {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    fn item_added(&mut self, out: &mut Vec<Bytes>, output_index: usize, item: Value) {
        self.emit(
            out,
            "response.output_item.added",
            json!({ "output_index": output_index, "item": item }),
        );
    }

    fn item_done(&mut self, out: &mut Vec<Bytes>, output_index: usize, item: Value) {
        self.emit(
            out,
            "response.output_item.done",
            json!({ "output_index": output_index, "item": item }),
        );
        self.output.insert(output_index, item);
    }

    fn open_block(&mut self, out: &mut Vec<Bytes>, index: usize, kind: BlockKind) {
        let block = match kind {
            BlockKind::Text => {
                let output_index = self.next_output_index();
                let item_id = output_item_id("msg", &self.response_id, output_index);
                self.item_added(
                    out,
                    output_index,
                    message_item(&item_id, "in_progress", None),
                );
                self.emit(
                    out,
                    "response.content_part.added",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "part": output_text_part(""),
                    }),
                );
                OpenBlock {
                    kind: OpenKind::Text,
                    output_index,
                    item_id,
                    text: String::new(),
                    signature: None,
                }
            }
            BlockKind::Thinking => {
                let output_index = self.next_output_index();
                let item_id = output_item_id("rs", &self.response_id, output_index);
                self.item_added(
                    out,
                    output_index,
                    json!({ "id": item_id, "type": "reasoning", "status": "in_progress", "summary": [] }),
                );
                OpenBlock {
                    kind: OpenKind::Thinking {
                        summary_open: false,
                    },
                    output_index,
                    item_id,
                    text: String::new(),
                    signature: None,
                }
            }
            BlockKind::RedactedThinking => OpenBlock {
                kind: OpenKind::Redacted,
                output_index: usize::MAX,
                item_id: String::new(),
                text: String::new(),
                signature: None,
            },
            BlockKind::ToolCall { id, name } => {
                let custom = is_custom(&self.custom_tool_names, &name);
                let output_index = self.next_output_index();
                let item = tool_call_item(&id, &name, "", custom, "in_progress");
                let item_id = tool_item_id(&id, custom);
                self.item_added(out, output_index, item);
                OpenBlock {
                    kind: OpenKind::ToolCall {
                        call_id: id,
                        name,
                        custom,
                    },
                    output_index,
                    item_id,
                    text: String::new(),
                    signature: None,
                }
            }
        };
        self.open.insert(index, block);
    }

    fn close_block(&mut self, out: &mut Vec<Bytes>, index: usize) {
        let Some(block) = self.open.remove(&index) else {
            return;
        };
        let OpenBlock {
            kind,
            output_index,
            item_id,
            text,
            signature,
        } = block;
        match kind {
            OpenKind::Text => {
                self.emit(
                    out,
                    "response.output_text.done",
                    json!({ "item_id": item_id, "output_index": output_index, "content_index": 0, "text": text }),
                );
                self.emit(
                    out,
                    "response.content_part.done",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "part": output_text_part(&text),
                    }),
                );
                let item = message_item(&item_id, "completed", Some(&text));
                self.item_done(out, output_index, item);
            }
            OpenKind::Thinking { summary_open } => {
                if summary_open {
                    self.emit(
                        out,
                        "response.reasoning_summary_text.done",
                        json!({ "item_id": item_id, "output_index": output_index, "summary_index": 0, "text": text }),
                    );
                    self.emit(
                        out,
                        "response.reasoning_summary_part.done",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "summary_index": 0,
                            "part": { "type": "summary_text", "text": text },
                        }),
                    );
                }
                let mut item = reasoning_item(&item_id, &text, signature.as_ref());
                // added 时已用生成的 id，done 必须一致
                item["id"] = json!(item_id);
                self.item_done(out, output_index, item);
            }
            OpenKind::Redacted => {
                let Some(signature) = signature else {
                    tracing::warn!("没有签名的 redacted thinking 块无法表达，已丢弃");
                    return;
                };
                let output_index = self.next_output_index();
                let item_id = output_item_id("rs", &self.response_id, output_index);
                self.item_added(
                    out,
                    output_index,
                    json!({ "id": item_id, "type": "reasoning", "status": "in_progress", "summary": [] }),
                );
                let mut item = reasoning_item(&item_id, "", Some(&signature));
                item["id"] = json!(item_id);
                self.item_done(out, output_index, item);
            }
            OpenKind::ToolCall {
                call_id,
                name,
                custom,
            } => {
                let item = tool_call_item(&call_id, &name, &text, custom, "completed");
                if custom {
                    self.emit(
                        out,
                        "response.custom_tool_call_input.done",
                        json!({ "item_id": item_id, "output_index": output_index, "input": item["input"] }),
                    );
                } else {
                    self.emit(
                        out,
                        "response.function_call_arguments.done",
                        json!({ "item_id": item_id, "output_index": output_index, "arguments": text }),
                    );
                }
                self.item_done(out, output_index, item);
            }
        }
    }

    fn close_all(&mut self, out: &mut Vec<Bytes>) {
        let indices: Vec<usize> = self.open.keys().copied().collect();
        for index in indices {
            self.close_block(out, index);
        }
    }

    fn fail(&mut self, out: &mut Vec<Bytes>, code: &str, message: &str) {
        let mut response = self.response_object("failed", Value::Null, Value::Null);
        response["error"] = json!({ "code": code, "message": message });
        self.emit(out, "response.failed", json!({ "response": response }));
        self.finished = true;
    }

    pub fn encode(&mut self, event: Event) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        match event {
            Event::Start { id, model, usage } => {
                self.start_usage = usage;
                self.ensure_started(&mut out, &id, &model);
            }
            Event::BlockStart { index, kind } => {
                self.ensure_started(&mut out, "", "");
                // 同一序号重复开块：先把旧块收尾
                self.close_block(&mut out, index);
                self.open_block(&mut out, index, kind);
            }
            Event::TextDelta { index, text } => {
                let Some(block) = self.open.get_mut(&index) else {
                    return out;
                };
                if !matches!(block.kind, OpenKind::Text) || text.is_empty() {
                    return out;
                }
                block.text.push_str(&text);
                let data = json!({
                    "item_id": block.item_id,
                    "output_index": block.output_index,
                    "content_index": 0,
                    "delta": text,
                });
                self.emit(&mut out, "response.output_text.delta", data);
            }
            Event::ThinkingDelta { index, text } => {
                let Some(block) = self.open.get_mut(&index) else {
                    return out;
                };
                let OpenKind::Thinking { summary_open } = &mut block.kind else {
                    return out;
                };
                if text.is_empty() {
                    return out;
                }
                let open_part = !*summary_open;
                *summary_open = true;
                block.text.push_str(&text);
                let (item_id, output_index) = (block.item_id.clone(), block.output_index);
                if open_part {
                    self.emit(
                        &mut out,
                        "response.reasoning_summary_part.added",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "summary_index": 0,
                            "part": { "type": "summary_text", "text": "" },
                        }),
                    );
                }
                self.emit(
                    &mut out,
                    "response.reasoning_summary_text.delta",
                    json!({ "item_id": item_id, "output_index": output_index, "summary_index": 0, "delta": text }),
                );
            }
            Event::SignatureDelta { index, signature } => {
                if let Some(block) = self.open.get_mut(&index) {
                    if matches!(block.kind, OpenKind::Thinking { .. } | OpenKind::Redacted) {
                        block.signature = Some(signature);
                    }
                }
            }
            Event::ToolArgumentsDelta {
                index,
                partial_json,
            } => {
                let Some(block) = self.open.get_mut(&index) else {
                    return out;
                };
                let OpenKind::ToolCall { custom, .. } = block.kind else {
                    return out;
                };
                block.text.push_str(&partial_json);
                // custom 工具的 input 要等参数完整后才能从 JSON 中取出，不发中途增量
                if !custom && !partial_json.is_empty() {
                    let data = json!({
                        "item_id": block.item_id,
                        "output_index": block.output_index,
                        "delta": partial_json,
                    });
                    self.emit(&mut out, "response.function_call_arguments.delta", data);
                }
            }
            Event::BlockStop { index } => self.close_block(&mut out, index),
            Event::Finish {
                stop_reason,
                mut usage,
            } => {
                self.ensure_started(&mut out, "", "");
                self.close_all(&mut out);
                if usage.total_input() == 0 {
                    // 部分上游只在开头给输入侧用量
                    usage.input_tokens = self.start_usage.input_tokens;
                    usage.cache_read_tokens = self.start_usage.cache_read_tokens;
                    usage.cache_write_tokens = self.start_usage.cache_write_tokens;
                }
                let (status, incomplete_details) = status_for(&stop_reason);
                // 收尾一律用 response.completed，status / incomplete_details 写在 response 对象里
                let response = self.response_object(status, usage_json(&usage), incomplete_details);
                self.emit(
                    &mut out,
                    "response.completed",
                    json!({ "response": response }),
                );
                self.finished = true;
            }
            Event::Error { kind, message } => {
                self.ensure_started(&mut out, "", "");
                self.fail(&mut out, &kind, &message);
            }
        }
        out
    }

    /// 上游流结束但没有 Finish：补发 response.failed（error 为原因，None 时用默认文案）；
    /// 已结束则返回空
    pub fn finish(&mut self, error: Option<&str>) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.ensure_started(&mut out, "", "");
        let message = error.unwrap_or("upstream stream ended before the response completed");
        self.fail(&mut out, "server_error", message);
        out
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// Responses 错误 body：`{"error":{"type","message","code"}}`
pub fn error_body(kind: &str, message: &str) -> Value {
    json!({ "error": { "type": kind, "message": message, "code": kind } })
}

/// HTTP 状态码 → Responses 错误类型
pub fn error_type_for_status(status: u16) -> &'static str {
    match status {
        400 | 404 | 413 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        429 => "rate_limit_error",
        500..=599 => "server_error",
        _ => "api_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir;
    use std::collections::HashMap;

    fn codex_request() -> Value {
        json!({
            "model": "gpt-5-codex",
            "instructions": "You are Codex.",
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "session-1",
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high", "summary": "auto"},
            "text": {"verbosity": "low"},
            "tool_choice": "auto",
            "input": [
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_text", "text": "<permissions>sandbox</permissions>"}
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "<environment_context/>"}
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "fix the bug"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBOR"}
                ]},
                {"type": "reasoning", "id": "rs_1", "summary": [
                    {"type": "summary_text", "text": "look"},
                    {"type": "summary_text", "text": "then patch"}
                ], "encrypted_content": "gAAAAopaque"},
                {"type": "function_call", "id": "fc_a", "call_id": "call_a", "name": "shell",
                 "arguments": "{\"command\":[\"ls\"],\"timeout\":5}"},
                {"type": "function_call", "id": "fc_b", "call_id": "call_b", "name": "shell",
                 "arguments": "{\"command\":[\"pwd\"]}"},
                {"type": "function_call", "call_id": "call_x", "name": "shell",
                 "arguments": "{\"comm", "status": "incomplete"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a.txt"},
                {"type": "function_call_output", "call_id": "call_b", "output": [
                    {"type": "input_text", "text": "/repo"},
                    {"type": "input_image", "image_url": "https://example.com/x.png"}
                ]},
                {"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "gAAAAredacted"},
                {"type": "reasoning", "id": "rs_3", "summary": []},
                {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_p", "name": "apply_patch",
                 "input": "*** Begin Patch\n*** End Patch"},
                {"type": "custom_tool_call_output", "call_id": "call_p", "output": "Done"},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "Patched."}
                ]},
                {"type": "item_reference", "id": "msg_old"},
                {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search"}},
                {"role": "user", "content": "thanks"}
            ],
            "tools": [
                {"type": "function", "name": "shell", "description": "run", "strict": false,
                 "parameters": {"type": "object", "properties": {"command": {"type": "array"}}}},
                {"type": "custom", "name": "apply_patch", "description": "patch",
                 "format": {"type": "grammar", "syntax": "lark", "definition": "..."}},
                {"type": "web_search"},
                {"type": "local_shell"},
                {"type": "file_search", "vector_store_ids": ["vs"]}
            ]
        })
    }

    #[test]
    fn parses_a_codex_request() {
        let request = parse_request(&codex_request()).unwrap();
        assert_eq!(request.model, "gpt-5-codex");
        assert!(request.stream);
        assert_eq!(
            request.system,
            vec!["You are Codex.", "<permissions>sandbox</permissions>"]
        );
        assert_eq!(request.reasoning, Reasoning::Effort(Effort::High));
        assert_eq!(request.parallel_tool_calls, Some(true));
        assert_eq!(request.tool_choice, ToolChoice::Auto);

        assert_eq!(request.tools.len(), 2);
        assert!(!request.tools[0].custom);
        assert_eq!(request.tools[1].name, "apply_patch");
        assert!(request.tools[1].custom);
        assert_eq!(
            request.tools[1].parameters,
            json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]})
        );
        assert_eq!(request.server_tools.len(), 1);
        assert_eq!(request.server_tools[0].kind, ServerToolKind::WebSearch);

        let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                Role::User,
                Role::Assistant,
                Role::User,
                Role::Assistant,
                Role::User,
                Role::Assistant,
                Role::User
            ]
        );

        // 连续的 user 消息合并；data URL 解析为 Base64
        let user = &request.messages[0].content;
        assert_eq!(user.len(), 3);
        assert_eq!(
            user[2],
            Block::Image {
                source: MediaSource::Base64 {
                    media_type: "image/png".into(),
                    data: "iVBOR".into()
                }
            }
        );

        // reasoning + 并行调用合并进同一条 assistant 消息；incomplete 调用被丢弃
        let assistant = &request.messages[1].content;
        assert_eq!(assistant.len(), 3);
        match &assistant[0] {
            Block::Thinking {
                text,
                signature: Some(signature),
            } => {
                assert_eq!(text, "look\n\nthen patch");
                assert_eq!(signature.source, Interface::OpenaiResponses);
                assert_eq!(signature.value["id"], "rs_1");
                assert_eq!(signature.value["encrypted_content"], "gAAAAopaque");
            }
            other => panic!("{other:?}"),
        }
        match &assistant[1] {
            Block::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "call_a");
                assert_eq!(name, "shell");
                assert_eq!(arguments, r#"{"command":["ls"],"timeout":5}"#);
            }
            other => panic!("{other:?}"),
        }

        // 连续的工具结果合并
        let results = &request.messages[2].content;
        assert_eq!(results.len(), 2);
        match &results[1] {
            Block::ToolResult {
                call_id, content, ..
            } => {
                assert_eq!(call_id, "call_b");
                assert_eq!(
                    content[1],
                    Block::Image {
                        source: MediaSource::Url("https://example.com/x.png".into())
                    }
                );
            }
            other => panic!("{other:?}"),
        }

        // 无摘要带签名 → RedactedThinking；无签名无摘要 → 丢弃；custom 调用降级
        let assistant = &request.messages[3].content;
        assert_eq!(assistant.len(), 2);
        assert!(
            matches!(&assistant[0], Block::RedactedThinking { signature }
            if signature.value["encrypted_content"] == "gAAAAredacted")
        );
        match &assistant[1] {
            Block::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "call_p");
                assert_eq!(name, "apply_patch");
                assert_eq!(
                    serde_json::from_str::<Value>(arguments).unwrap(),
                    json!({"input": "*** Begin Patch\n*** End Patch"})
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&request.messages[4].content[0],
            Block::ToolResult { call_id, .. } if call_id == "call_p"));
        // assistant 文本；item_reference 与无文本的 web_search_call 被丢弃
        assert_eq!(
            request.messages[5].content,
            vec![Block::Text {
                text: "Patched.".into()
            }]
        );
        assert_eq!(
            request.messages[6].content,
            vec![Block::Text {
                text: "thanks".into()
            }]
        );
    }

    #[test]
    fn parses_simple_forms() {
        let request = parse_request(&json!({
            "model": "m", "input": "hi", "user": "u", "safety_identifier": "s",
            "max_output_tokens": 100, "temperature": 0.5, "top_p": 0.9,
            "reasoning": {"effort": "none"},
            "tool_choice": {"type": "function", "name": "shell"}
        }))
        .unwrap();
        assert_eq!(
            request.messages,
            vec![Message {
                role: Role::User,
                content: vec![Block::Text { text: "hi".into() }]
            }]
        );
        assert_eq!(request.user.as_deref(), Some("u"));
        assert_eq!(request.max_output_tokens, Some(100));
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.top_p, Some(0.9));
        assert!(!request.stream);
        assert_eq!(request.reasoning, Reasoning::Disabled);
        assert_eq!(request.tool_choice, ToolChoice::Named("shell".into()));

        let request = parse_request(&json!({
            "model": "m", "safety_identifier": "s", "tool_choice": "required",
            "reasoning": {"effort": "max"},
            "input": [
                {"type": "input_text", "text": "top-level"},
                {"type": "local_shell_call_output", "output": "ok"},
                {"type": "function_call", "call_id": "c", "name": "f", "arguments": ""}
            ]
        }))
        .unwrap();
        assert_eq!(request.user.as_deref(), Some("s"));
        assert_eq!(request.tool_choice, ToolChoice::Required);
        assert_eq!(request.reasoning, Reasoning::Effort(Effort::XHigh));
        assert_eq!(request.messages[0].content.len(), 2);
        assert!(matches!(&request.messages[1].content[0],
            Block::ToolCall { arguments, .. } if arguments == "{}"));
        assert_eq!(
            parse_request(&json!({"model": "m", "input": []}))
                .unwrap()
                .reasoning,
            Reasoning::Unspecified
        );
    }

    #[test]
    fn envelope_signatures_are_decoded() {
        let signature = Signature {
            source: Interface::Claude,
            value: json!({"type": "thinking", "thinking": "t", "signature": "EqQB"}),
        };
        let item = json!({"type": "reasoning", "summary": [], "encrypted_content": envelope::encode(&signature)});
        assert_eq!(
            parse_reasoning_item(&item),
            Some(Block::RedactedThinking { signature })
        );
        // 形似封套但解码失败：按 Responses 原值处理
        let item = json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "x"}], "encrypted_content": "ccsw1.claude.!!!"});
        match parse_reasoning_item(&item) {
            Some(Block::Thinking {
                signature: Some(signature),
                ..
            }) => assert_eq!(signature.source, Interface::OpenaiResponses),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rejects_unsupported_requests() {
        let base = |extra: Value| {
            let mut body = json!({"model": "m", "input": "hi"});
            for (k, v) in extra.as_object().unwrap() {
                body[k] = v.clone();
            }
            parse_request(&body)
        };
        assert!(parse_request(&json!([])).is_err());
        for extra in [
            json!({"previous_response_id": "resp_1"}),
            json!({"conversation": "conv_1"}),
            json!({"conversation": {"id": "conv_1"}}),
            json!({"n": 2}),
            json!({"logprobs": true}),
            json!({"top_logprobs": 3}),
            json!({"text": {"format": {"type": "json_schema", "name": "x", "schema": {}}}}),
            json!({"text": {"format": {"type": "json_object"}}}),
            json!({"input": 1}),
            json!({"instructions": 1}),
            json!({"input": [{"type": "input_file", "file_data": "x"}]}),
            json!({"input": [{"role": "user", "content": [{"type": "input_file", "file_id": "f"}]}]}),
            json!({"input": [{"role": "user", "content": [{"type": "input_image", "file_id": "f"}]}]}),
            json!({"input": [{"role": "tool", "content": "x"}]}),
            json!({"input": [{"type": "function_call", "call_id": "c", "name": "f", "arguments": "[1]"}]}),
            json!({"input": [{"type": "function_call", "call_id": "c", "name": "f", "arguments": "{bad"}]}),
        ] {
            assert!(base(extra.clone()).is_err(), "{extra}");
        }
        // 静默接受的字段
        for extra in [
            json!({"previous_response_id": ""}),
            json!({"conversation": null}),
            json!({"n": 1}),
            json!({"top_logprobs": 0}),
            json!({"text": {"format": {"type": "text"}, "verbosity": "high"}}),
            json!({"store": true, "service_tier": "auto", "truncation": "auto", "metadata": {"a": "b"}}),
        ] {
            assert!(base(extra.clone()).is_ok(), "{extra}");
        }
    }

    fn sample_response(stop_reason: StopReason) -> Response {
        Response {
            id: "chatcmpl-1".into(),
            model: "deepseek-chat".into(),
            content: vec![
                Block::Thinking {
                    text: "why".into(),
                    signature: Some(Signature {
                        source: Interface::Claude,
                        value: json!({"type": "thinking", "thinking": "why", "signature": "EqQB"}),
                    }),
                },
                Block::RedactedThinking {
                    signature: Signature {
                        source: Interface::Gemini,
                        value: json!({"sig": "s", "call_id": "call_1"}),
                    },
                },
                Block::Text { text: "hi".into() },
                Block::ToolCall {
                    id: "call_1".into(),
                    name: "shell".into(),
                    arguments: r#"{"b":1,"a":2}"#.into(),
                    signature: None,
                },
                Block::ToolCall {
                    id: "call_2".into(),
                    name: "apply_patch".into(),
                    arguments: r#"{"input":"*** Begin Patch"}"#.into(),
                    signature: None,
                },
                Block::ToolCall {
                    id: "call_3".into(),
                    name: "apply_patch".into(),
                    arguments: "raw patch".into(),
                    signature: None,
                },
            ],
            stop_reason,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 7,
                cache_read_tokens: 3,
                cache_write_tokens: 2,
                reasoning_tokens: Some(4),
            },
        }
    }

    fn custom_names() -> Vec<String> {
        vec!["apply_patch".to_string()]
    }

    #[test]
    fn renders_a_response() {
        let response = sample_response(StopReason::ToolUse);
        let value = render_response(&response, "gpt-5-codex", &custom_names());
        assert_eq!(value["id"], "resp_chatcmpl-1");
        assert_eq!(value["object"], "response");
        assert!(value["created_at"].as_u64().unwrap() > 1_600_000_000);
        assert_eq!(value["model"], "gpt-5-codex");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["incomplete_details"], Value::Null);
        assert_eq!(
            value["usage"],
            json!({
                "input_tokens": 10,
                "input_tokens_details": {"cached_tokens": 3},
                "output_tokens": 7,
                "output_tokens_details": {"reasoning_tokens": 4},
                "total_tokens": 17
            })
        );
        let output = value["output"].as_array().unwrap();
        assert_eq!(output.len(), 6);
        assert_eq!(output[0]["type"], "reasoning");
        assert!(output[0].get("status").is_none());
        assert_eq!(
            output[0]["summary"],
            json!([{"type": "summary_text", "text": "why"}])
        );
        let decoded = envelope::decode(output[0]["encrypted_content"].as_str().unwrap()).unwrap();
        assert_eq!(decoded.source, Interface::Claude);
        assert_eq!(output[1]["summary"], json!([]));
        assert!(output[1]["encrypted_content"]
            .as_str()
            .unwrap()
            .starts_with("ccsw1.gemini."));
        assert_eq!(
            output[2],
            json!({"id": "msg_chatcmpl-1_2", "type": "message", "status": "completed", "role": "assistant",
                   "content": [{"type": "output_text", "text": "hi", "annotations": []}]})
        );
        assert_eq!(
            output[3],
            json!({"id": "fc_call_1", "type": "function_call", "status": "completed",
                   "call_id": "call_1", "name": "shell", "arguments": r#"{"b":1,"a":2}"#})
        );
        assert_eq!(
            output[4],
            json!({"id": "ctc_call_2", "type": "custom_tool_call", "status": "completed",
                   "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch"})
        );
        assert_eq!(output[5]["input"], "raw patch");

        // 默认 reasoning_tokens 为 0；已带 resp_ 前缀的 id 不重复加
        let mut plain = sample_response(StopReason::EndTurn);
        plain.id = "resp_abc".into();
        plain.usage.reasoning_tokens = None;
        let value = render_response(&plain, "", &[]);
        assert_eq!(value["id"], "resp_abc");
        assert_eq!(value["model"], "deepseek-chat");
        assert_eq!(
            value["usage"]["output_tokens_details"]["reasoning_tokens"],
            0
        );
        assert_eq!(value["output"][4]["type"], "function_call");
    }

    #[test]
    fn render_stop_reasons() {
        for (reason, status, details) in [
            (StopReason::EndTurn, "completed", Value::Null),
            (StopReason::ToolUse, "completed", Value::Null),
            (StopReason::StopSequence, "completed", Value::Null),
            (StopReason::Other("weird".into()), "completed", Value::Null),
            (
                StopReason::MaxTokens,
                "incomplete",
                json!({"reason": "max_output_tokens"}),
            ),
            (
                StopReason::Refusal,
                "incomplete",
                json!({"reason": "content_filter"}),
            ),
        ] {
            let value = render_response(&sample_response(reason.clone()), "m", &[]);
            assert_eq!(value["status"], status, "{reason:?}");
            assert_eq!(value["incomplete_details"], details, "{reason:?}");
        }
    }

    #[test]
    fn render_replays_responses_reasoning_items() {
        let original = json!({"type": "reasoning", "id": "rs_orig", "summary": [], "encrypted_content": "gAAAA"});
        let response = Response {
            id: "resp_1".into(),
            content: vec![Block::Thinking {
                text: "t".into(),
                signature: Some(Signature {
                    source: Interface::OpenaiResponses,
                    value: original,
                }),
            }],
            ..Default::default()
        };
        let value = render_response(&response, "m", &[]);
        assert_eq!(value["output"][0]["id"], "rs_orig");
        assert_eq!(value["output"][0]["encrypted_content"], "gAAAA");
    }

    fn frames_to_events(frames: &[Bytes]) -> Vec<(String, Value)> {
        let mut parser = sse::SseParser::new();
        frames
            .iter()
            .flat_map(|frame| parser.feed(frame))
            .map(|e| (e.event.unwrap(), serde_json::from_str(&e.data).unwrap()))
            .collect()
    }

    fn encode_all(encoder: &mut StreamEncoder, events: Vec<Event>) -> Vec<(String, Value)> {
        let mut frames = Vec::new();
        for event in events {
            frames.extend(encoder.encode(event));
        }
        frames.extend(encoder.finish(None));
        frames_to_events(&frames)
    }

    /// 通用结构校验：帧名与 type 一致、sequence_number 从 0 严格递增、每个 added 都有对应 done
    fn check_structure(events: &[(String, Value)]) {
        for (i, (name, data)) in events.iter().enumerate() {
            assert_eq!(data["type"], name.as_str());
            assert_eq!(data["sequence_number"], i as u64, "{name}");
        }
        let mut open: HashMap<String, usize> = HashMap::new();
        for (name, data) in events {
            let key = |suffix: &str| format!("{}:{suffix}", data["output_index"]);
            match name.as_str() {
                "response.output_item.added" => *open.entry(key("item")).or_default() += 1,
                "response.output_item.done" => *open.entry(key("item")).or_default() -= 1,
                "response.content_part.added" => *open.entry(key("part")).or_default() += 1,
                "response.content_part.done" => *open.entry(key("part")).or_default() -= 1,
                "response.reasoning_summary_part.added" => {
                    *open.entry(key("summary")).or_default() += 1
                }
                "response.reasoning_summary_part.done" => {
                    *open.entry(key("summary")).or_default() -= 1
                }
                _ => {}
            }
        }
        assert!(open.values().all(|n| *n == 0), "{open:?}");
        assert_eq!(events[0].0, "response.created");
        assert_eq!(events[1].0, "response.in_progress");
        assert_eq!(events[1].1["response"]["status"], "in_progress");
        assert_eq!(events[1].1["response"]["output"], json!([]));
    }

    fn names(events: &[(String, Value)]) -> Vec<&str> {
        events
            .iter()
            .map(|(n, _)| n.strip_prefix("response.").unwrap())
            .collect()
    }

    #[test]
    fn stream_encoder_full_sequence() {
        let mut encoder = StreamEncoder::new("gpt-5-codex", custom_names());
        let signature = Signature {
            source: Interface::Claude,
            value: json!({"type": "thinking", "thinking": "hmm", "signature": "EqQB"}),
        };
        let events = encode_all(
            &mut encoder,
            vec![
                Event::Start {
                    id: "msg_1".into(),
                    model: "claude".into(),
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
                    text: "hm".into(),
                },
                Event::ThinkingDelta {
                    index: 0,
                    text: "m".into(),
                },
                Event::SignatureDelta {
                    index: 0,
                    signature: signature.clone(),
                },
                Event::BlockStop { index: 0 },
                Event::BlockStart {
                    index: 1,
                    kind: BlockKind::Text,
                },
                Event::TextDelta {
                    index: 1,
                    text: "he".into(),
                },
                Event::TextDelta {
                    index: 1,
                    text: "llo".into(),
                },
                Event::BlockStop { index: 1 },
                Event::BlockStart {
                    index: 2,
                    kind: BlockKind::ToolCall {
                        id: "call_1".into(),
                        name: "shell".into(),
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
                Event::BlockStart {
                    index: 3,
                    kind: BlockKind::ToolCall {
                        id: "call_2".into(),
                        name: "apply_patch".into(),
                    },
                },
                Event::ToolArgumentsDelta {
                    index: 3,
                    partial_json: "{\"input\":\"*** Begin".into(),
                },
                Event::ToolArgumentsDelta {
                    index: 3,
                    partial_json: " Patch\"}".into(),
                },
                Event::BlockStop { index: 3 },
                Event::Finish {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 9,
                        output_tokens: 4,
                        ..Default::default()
                    },
                },
                // 结束后的事件被忽略
                Event::TextDelta {
                    index: 1,
                    text: "late".into(),
                },
            ],
        );
        check_structure(&events);
        assert_eq!(
            names(&events),
            vec![
                "created",
                "in_progress",
                "output_item.added",
                "reasoning_summary_part.added",
                "reasoning_summary_text.delta",
                "reasoning_summary_text.delta",
                "reasoning_summary_text.done",
                "reasoning_summary_part.done",
                "output_item.done",
                "output_item.added",
                "content_part.added",
                "output_text.delta",
                "output_text.delta",
                "output_text.done",
                "content_part.done",
                "output_item.done",
                "output_item.added",
                "function_call_arguments.delta",
                "function_call_arguments.delta",
                "function_call_arguments.done",
                "output_item.done",
                "output_item.added",
                "custom_tool_call_input.done",
                "output_item.done",
                "completed",
            ]
        );
        let created = &events[0].1["response"];
        assert_eq!(created["id"], "resp_msg_1");
        assert_eq!(created["model"], "gpt-5-codex");

        // reasoning
        assert_eq!(events[2].1["output_index"], 0);
        assert_eq!(events[2].1["item"]["summary"], json!([]));
        let rs_id = events[2].1["item"]["id"].as_str().unwrap().to_string();
        assert_eq!(events[4].1["item_id"], rs_id.as_str());
        assert_eq!(events[4].1["summary_index"], 0);
        assert_eq!(events[6].1["text"], "hmm");
        assert_eq!(
            events[7].1["part"],
            json!({"type": "summary_text", "text": "hmm"})
        );
        let reasoning = &events[8].1["item"];
        assert_eq!(reasoning["id"], rs_id.as_str());
        assert!(reasoning.get("status").is_none());
        assert_eq!(
            reasoning["encrypted_content"],
            envelope::encode(&signature).as_str()
        );

        // 文本
        assert_eq!(events[9].1["output_index"], 1);
        assert_eq!(events[9].1["item"]["status"], "in_progress");
        assert_eq!(events[9].1["item"]["content"], json!([]));
        assert_eq!(events[10].1["part"]["text"], "");
        assert_eq!(events[11].1["content_index"], 0);
        assert_eq!(events[13].1["text"], "hello");
        assert_eq!(events[14].1["part"]["text"], "hello");
        assert_eq!(events[15].1["item"]["content"][0]["text"], "hello");

        // function_call
        assert_eq!(events[16].1["item"]["id"], "fc_call_1");
        assert_eq!(events[16].1["item"]["arguments"], "");
        assert_eq!(events[17].1["item_id"], "fc_call_1");
        assert_eq!(events[19].1["arguments"], r#"{"a":1}"#);
        assert_eq!(events[20].1["item"]["arguments"], r#"{"a":1}"#);
        assert_eq!(events[20].1["item"]["status"], "completed");

        // custom 工具：无中途增量
        assert_eq!(events[21].1["item"]["type"], "custom_tool_call");
        assert_eq!(events[21].1["item"]["input"], "");
        assert_eq!(events[22].1["item_id"], "ctc_call_2");
        assert_eq!(events[22].1["input"], "*** Begin Patch");
        assert_eq!(events[23].1["item"]["input"], "*** Begin Patch");
        assert_eq!(events[23].1["output_index"], 3);

        let completed = &events[24].1["response"];
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["output"].as_array().unwrap().len(), 4);
        assert_eq!(completed["usage"]["input_tokens"], 9);
        assert_eq!(completed["usage"]["output_tokens"], 4);
        assert_eq!(completed["usage"]["total_tokens"], 13);
        assert!(encoder.finish(Some("late")).is_empty());
    }

    #[test]
    fn stream_encoder_max_tokens_and_redacted() {
        let mut encoder = StreamEncoder::new("m", Vec::new());
        let signature = Signature {
            source: Interface::Gemini,
            value: json!({"sig": "s", "call_id": "c"}),
        };
        let events = encode_all(
            &mut encoder,
            vec![
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::RedactedThinking,
                },
                Event::SignatureDelta {
                    index: 0,
                    signature: signature.clone(),
                },
                Event::BlockStop { index: 0 },
                // 没有签名的 redacted 块整块不输出
                Event::BlockStart {
                    index: 1,
                    kind: BlockKind::RedactedThinking,
                },
                Event::BlockStop { index: 1 },
                Event::BlockStart {
                    index: 2,
                    kind: BlockKind::Text,
                },
                Event::TextDelta {
                    index: 2,
                    text: "trunc".into(),
                },
                Event::Finish {
                    stop_reason: StopReason::MaxTokens,
                    usage: Usage {
                        output_tokens: 3,
                        ..Default::default()
                    },
                },
            ],
        );
        check_structure(&events);
        assert_eq!(
            names(&events),
            vec![
                "created",
                "in_progress",
                "output_item.added",
                "output_item.done",
                "output_item.added",
                "content_part.added",
                "output_text.delta",
                "output_text.done",
                "content_part.done",
                "output_item.done",
                "completed",
            ]
        );
        assert!(events[0].1["response"]["id"]
            .as_str()
            .unwrap()
            .starts_with("resp_"));
        assert_eq!(events[3].1["item"]["summary"], json!([]));
        assert_eq!(
            events[3].1["item"]["encrypted_content"],
            envelope::encode(&signature).as_str()
        );
        assert_eq!(events[4].1["output_index"], 1);
        let response = &events[10].1["response"];
        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"],
            json!({"reason": "max_output_tokens"})
        );
        assert_eq!(response["output"][1]["content"][0]["text"], "trunc");
    }

    #[test]
    fn stream_encoder_errors() {
        let mut encoder = StreamEncoder::new("m", Vec::new());
        let events = encode_all(
            &mut encoder,
            vec![
                Event::Start {
                    id: "x".into(),
                    model: "up".into(),
                    usage: Usage::default(),
                },
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                },
                Event::TextDelta {
                    index: 0,
                    text: "a".into(),
                },
                Event::BlockStop { index: 0 },
                Event::Error {
                    kind: "rate_limit_error".into(),
                    message: "slow down".into(),
                },
            ],
        );
        let (name, data) = events.last().unwrap();
        assert_eq!(name, "response.failed");
        assert_eq!(data["response"]["status"], "failed");
        assert_eq!(
            data["response"]["error"],
            json!({"code": "rate_limit_error", "message": "slow down"})
        );
        assert_eq!(data["response"]["output"].as_array().unwrap().len(), 1);
        for (i, (_, data)) in events.iter().enumerate() {
            assert_eq!(data["sequence_number"], i as u64);
        }

        // 上游提前结束
        let mut encoder = StreamEncoder::new("m", Vec::new());
        let mut frames = encoder.encode(Event::Start {
            id: "x".into(),
            model: "up".into(),
            usage: Usage::default(),
        });
        frames.extend(encoder.finish(Some("upstream broke")));
        let events = frames_to_events(&frames);
        assert_eq!(names(&events), vec!["created", "in_progress", "failed"]);
        assert_eq!(
            events[2].1["response"]["error"]["message"],
            "upstream broke"
        );
        assert!(encoder.finish(None).is_empty());
        assert!(encoder
            .encode(Event::Finish {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default()
            })
            .is_empty());

        let events = frames_to_events(&StreamEncoder::new("m", Vec::new()).finish(None));
        assert_eq!(names(&events), vec!["created", "in_progress", "failed"]);
        assert_eq!(
            events[2].1["response"]["error"]["message"],
            "upstream stream ended before the response completed"
        );
    }

    #[test]
    fn stream_output_matches_render_response() {
        for reason in [StopReason::ToolUse, StopReason::MaxTokens] {
            let response = sample_response(reason);
            let rendered = render_response(&response, "gpt-5-codex", &custom_names());
            let mut encoder = StreamEncoder::new("gpt-5-codex", custom_names());
            let events = encode_all(&mut encoder, ir::response_events(&response));
            check_structure(&events);

            let mut rebuilt: BTreeMap<u64, Value> = BTreeMap::new();
            for (name, data) in &events {
                if name == "response.output_item.done" {
                    rebuilt.insert(data["output_index"].as_u64().unwrap(), data["item"].clone());
                }
            }
            let rebuilt: Vec<Value> = rebuilt.into_values().collect();
            assert_eq!(Value::Array(rebuilt), rendered["output"]);

            let completed = &events.last().unwrap().1["response"];
            for key in [
                "id",
                "status",
                "model",
                "output",
                "usage",
                "incomplete_details",
            ] {
                assert_eq!(completed[key], rendered[key], "{key}");
            }
        }
    }

    #[test]
    fn error_formats() {
        assert_eq!(
            error_body("invalid_request_error", "bad"),
            json!({"error": {"type": "invalid_request_error", "message": "bad", "code": "invalid_request_error"}})
        );
        assert_eq!(error_type_for_status(400), "invalid_request_error");
        assert_eq!(error_type_for_status(401), "authentication_error");
        assert_eq!(error_type_for_status(429), "rate_limit_error");
        assert_eq!(error_type_for_status(503), "server_error");
        assert_eq!(error_type_for_status(418), "api_error");
    }
}
