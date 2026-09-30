//! Gemini（generativelanguage v1beta）协议（设计文档 §5、§6.2）：上游侧请求生成、响应与流式解析。
//!
//! 要点：
//! - 模型名在 path 中（[`request_path`]），body 不带 `model`。
//! - Gemini 的 `functionCall` 可能没有 `id`：解析时生成全局唯一的 `call_<随机十六进制>`；
//!   生成请求时不回传这类 id，只按本请求内 `ToolCall.id → name` 回填函数名（§4.1）。
//! - `thoughtSignature`（§5.5）：`functionCall` 上的签名解析为紧邻该调用**之前**的
//!   `RedactedThinking` 块（载荷 `{"sig", "call_id"}`）；文本 / 思考 part 上的签名附到
//!   `Thinking` 块（载荷 `{"sig", "text": true}`）。
//! - 非流式响应复用流式解码器：整段 body 视为唯一一个片段，再由事件组装为 [`Response`]，
//!   两条路径的块划分与签名落点完全一致。

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cc_proxy_core::Interface;
use serde_json::{json, Map, Value};

use crate::chat::{clean_schema, strip_leading_billing_header, RenderOptions};
use crate::ir::{
    Block, BlockKind, Event, MediaSource, Message, Reasoning, Request, Response, Role,
    ServerToolKind, Signature, StopReason, Tool, ToolChoice, Usage,
};

/// 由本模块生成的调用 id 前缀：这类 id 不写回 Gemini 请求
const GENERATED_ID_PREFIX: &str = "call_";

/// 32 位随机十六进制：`RandomState` 的随机种子 + 时间 + 进程内计数器，不引入额外依赖
fn random_hex() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let state = RandomState::new();
    let mut words = [0u64; 2];
    for (salt, word) in words.iter_mut().enumerate() {
        let mut hasher = state.build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u64(count);
        hasher.write_usize(salt);
        *word = hasher.finish();
    }
    format!("{:016x}{:016x}", words[0], words[1])
}

fn generated_call_id() -> String {
    format!("{GENERATED_ID_PREFIX}{}", random_hex())
}

/// 可以写回 Gemini 的调用 id：非空且不是本模块生成的
fn forwardable_id(id: &str) -> Option<&str> {
    (!id.is_empty() && !id.starts_with(GENERATED_ID_PREFIX)).then_some(id)
}

/// 上游 path 与 query：`/v1beta/models/{model}:generateContent` 或
/// `:streamGenerateContent` + `alt=sse`。模型名带 `models/` 前缀时去掉，避免重复
pub fn request_path(upstream_model: &str, stream: bool) -> (String, Option<String>) {
    let model = upstream_model
        .strip_prefix("models/")
        .unwrap_or(upstream_model);
    if stream {
        (
            format!("/v1beta/models/{model}:streamGenerateContent"),
            Some("alt=sse".into()),
        )
    } else {
        (format!("/v1beta/models/{model}:generateContent"), None)
    }
}

// ---------------------------------------------------------------------------
// 请求生成
// ---------------------------------------------------------------------------

/// Gemini 签名载荷（§5.5 封套中的 `{"sig", "call_id"}` / `{"sig", "text": true}`）
struct GeminiSignature {
    sig: String,
    /// `None` 表示文本型签名
    call_id: Option<String>,
}

fn gemini_signature(signature: &Signature) -> Option<GeminiSignature> {
    if signature.source != Interface::Gemini {
        return None;
    }
    let sig = signature.value.get("sig")?.as_str()?.to_string();
    if let Some(call_id) = signature.value.get("call_id").and_then(Value::as_str) {
        return Some(GeminiSignature {
            sig,
            call_id: Some(call_id.to_string()),
        });
    }
    (signature.value.get("text").and_then(Value::as_bool) == Some(true))
        .then_some(GeminiSignature { sig, call_id: None })
}

fn call_signature(sig: &str, call_id: &str) -> Signature {
    Signature {
        source: Interface::Gemini,
        value: json!({ "sig": sig, "call_id": call_id }),
    }
}

fn text_signature(sig: &str) -> Signature {
    Signature {
        source: Interface::Gemini,
        value: json!({ "sig": sig, "text": true }),
    }
}

/// 图片 / 文档 → `inlineData`。Gemini API Key 模式只接受内联数据，不在中转内下载 URL（§5.4）
fn media_part(source: &MediaSource) -> Result<Value, String> {
    match source {
        MediaSource::Base64 { media_type, data } => Ok(json!({
            "inlineData": { "mimeType": media_type, "data": data }
        })),
        MediaSource::Url(_) => Err(
            "Gemini upstreams only accept inline base64 media; URL sources are not supported"
                .into(),
        ),
    }
}

/// 一个 `contents[]` 轮次
struct Turn {
    role: &'static str,
    parts: Vec<Value>,
    /// 文本型签名：输出时写到该轮最后一个 part
    text_signature: Option<String>,
}

impl Turn {
    fn into_value(mut self) -> Value {
        if let Some(sig) = self.text_signature {
            if let Some(last) = self.parts.last_mut() {
                // 最后一个 part 是自带签名的 functionCall 时保留调用签名
                if last.get("thoughtSignature").is_none() {
                    last["thoughtSignature"] = json!(sig);
                }
            }
        }
        json!({ "role": self.role, "parts": self.parts })
    }
}

/// 追加一轮；与上一轮同角色时合并（Gemini 要求角色交替）
fn push_turn(turns: &mut Vec<Turn>, turn: Turn) {
    if turn.parts.is_empty() {
        return;
    }
    if let Some(last) = turns.last_mut() {
        if last.role == turn.role {
            last.parts.extend(turn.parts);
            if turn.text_signature.is_some() {
                last.text_signature = turn.text_signature;
            }
            return;
        }
    }
    turns.push(turn);
}

/// IR → Gemini 请求体。无法表达时返回 `Err(原因)`（调用方映射为 Unsupported）
pub fn render_request(request: &Request, options: &RenderOptions<'_>) -> Result<Value, String> {
    let mut body = Map::new();

    let system: Vec<Value> = request
        .system
        .iter()
        .enumerate()
        .map(|(i, text)| {
            if i == 0 {
                strip_leading_billing_header(text)
            } else {
                text.as_str()
            }
        })
        .filter(|text| !text.is_empty())
        .map(|text| json!({ "text": text }))
        .collect();
    if !system.is_empty() {
        body.insert("systemInstruction".into(), json!({ "parts": system }));
    }

    // 本请求内 ToolCall.id → name，用于回填 functionResponse.name
    let names: HashMap<&str, &str> = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Block::ToolCall { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();

    let mut turns = Vec::new();
    for message in &request.messages {
        let turn = match message.role {
            Role::User => render_user(message, &names)?,
            Role::Assistant => render_model(message)?,
        };
        push_turn(&mut turns, turn);
    }
    body.insert(
        "contents".into(),
        Value::Array(turns.into_iter().map(Turn::into_value).collect()),
    );

    render_tools(request, &mut body);
    body.insert(
        "generationConfig".into(),
        generation_config(request, options),
    );
    Ok(Value::Object(body))
}

fn render_user(message: &Message, names: &HashMap<&str, &str>) -> Result<Turn, String> {
    // 工具结果在前（紧接上一 model 轮的 functionCall），其余内容在后
    let mut parts = Vec::new();
    let mut rest = Vec::new();
    for block in &message.content {
        match block {
            Block::Text { text } if !text.is_empty() => rest.push(json!({ "text": text })),
            Block::Image { source } | Block::Document { source, .. } => {
                rest.push(media_part(source)?)
            }
            Block::ToolResult {
                call_id, content, ..
            } => render_tool_result(call_id, content, names, &mut parts)?,
            _ => {}
        }
    }
    parts.extend(rest);
    Ok(Turn {
        role: "user",
        parts,
        text_signature: None,
    })
}

fn render_tool_result(
    call_id: &str,
    content: &[Block],
    names: &HashMap<&str, &str>,
    parts: &mut Vec<Value>,
) -> Result<(), String> {
    let mut texts = Vec::new();
    let mut media = Vec::new();
    for block in content {
        match block {
            Block::Text { text } => texts.push(text.as_str()),
            Block::Image { source } | Block::Document { source, .. } => {
                media.push(media_part(source)?)
            }
            _ => {}
        }
    }
    let text = texts.join("\n");
    match names.get(call_id) {
        Some(name) => {
            let mut response = Map::new();
            response.insert("name".into(), json!(name));
            if let Some(id) = forwardable_id(call_id) {
                response.insert("id".into(), json!(id));
            }
            // response 必须是对象
            response.insert("response".into(), json!({ "content": text }));
            parts.push(json!({ "functionResponse": response }));
        }
        None => {
            tracing::warn!(
                call_id,
                "Gemini 请求中找不到工具结果对应的调用，已降级为用户文本"
            );
            parts.push(json!({ "text": format!("[tool result {call_id}]\n{text}") }));
        }
    }
    // 结果中的媒体作为紧随的 inlineData part
    parts.extend(media);
    Ok(())
}

fn render_model(message: &Message) -> Result<Turn, String> {
    // 同一 assistant 消息中推理块承载的签名：按 call_id 归属调用，文本型取最后一个
    let mut call_signatures: HashMap<String, String> = HashMap::new();
    let mut text_sig = None;
    for block in &message.content {
        let signature = match block {
            Block::Thinking {
                signature: Some(signature),
                ..
            }
            | Block::RedactedThinking { signature } => signature,
            _ => continue,
        };
        if let Some(parsed) = gemini_signature(signature) {
            match parsed.call_id {
                Some(call_id) => {
                    call_signatures.insert(call_id, parsed.sig);
                }
                None => text_sig = Some(parsed.sig),
            }
        }
    }

    let mut parts = Vec::new();
    for block in &message.content {
        match block {
            Block::Text { text } if !text.is_empty() => parts.push(json!({ "text": text })),
            Block::Image { source } | Block::Document { source, .. } => {
                parts.push(media_part(source)?)
            }
            Block::ToolCall {
                id,
                name,
                arguments,
                signature,
            } => {
                let args = parse_arguments(name, arguments)?;
                let mut call = Map::new();
                call.insert("name".into(), json!(name));
                call.insert("args".into(), args);
                if let Some(id) = forwardable_id(id) {
                    call.insert("id".into(), json!(id));
                }
                let mut part = json!({ "functionCall": call });
                let sig = signature
                    .as_ref()
                    .and_then(gemini_signature)
                    .map(|parsed| parsed.sig)
                    .or_else(|| call_signatures.get(id).cloned());
                if let Some(sig) = sig {
                    part["thoughtSignature"] = json!(sig);
                }
                parts.push(part);
            }
            // 思考文本不回放：Gemini 不需要历史中的思考摘要，推理上下文由签名承载，
            // 回放文本只会增加输入 token；其他来源的推理块对 Gemini 无意义
            _ => {}
        }
    }
    Ok(Turn {
        role: "model",
        parts,
        text_signature: text_sig,
    })
}

/// `ToolCall.arguments` → `functionCall.args`：必须是 JSON 对象（空文本按 `{}`）
fn parse_arguments(name: &str, arguments: &str) -> Result<Value, String> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(format!(
            "arguments of tool call `{name}` are not a JSON object"
        )),
    }
}

/// schema 是否只含 Gemini `parameters` 接受的 OpenAPI 子集（§5.2）；未知关键字按保守处理
fn is_openapi_subset(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    object.iter().all(|(key, value)| match key.as_str() {
        "type" => value.is_string(),
        "title" | "description" | "nullable" | "format" | "default" | "enum" | "required"
        | "minimum" | "maximum" | "minItems" | "maxItems" | "minLength" | "maxLength"
        | "pattern" => true,
        "properties" => value
            .as_object()
            .is_some_and(|properties| properties.values().all(is_openapi_subset)),
        "items" => is_openapi_subset(value),
        "anyOf" => value
            .as_array()
            .is_some_and(|members| members.iter().all(is_openapi_subset)),
        _ => false,
    })
}

fn function_declaration(tool: &Tool) -> Value {
    let mut declaration = Map::new();
    declaration.insert("name".into(), json!(tool.name));
    if let Some(description) = &tool.description {
        declaration.insert("description".into(), json!(description));
    }
    let schema = clean_schema(&tool.parameters);
    if is_openapi_subset(&schema) {
        declaration.insert("parameters".into(), schema);
    } else {
        let mut schema = schema;
        if let Some(object) = schema.as_object_mut() {
            // 元数据关键字对校验无意义，部分上游会拒绝
            object.remove("$schema");
            object.remove("$id");
        }
        declaration.insert("parametersJsonSchema".into(), schema);
    }
    Value::Object(declaration)
}

fn render_tools(request: &Request, body: &mut Map<String, Value>) {
    let declarations: Vec<Value> = request.tools.iter().map(function_declaration).collect();
    let mut tools = Vec::new();
    if !declarations.is_empty() {
        tools.push(json!({ "functionDeclarations": declarations }));
    }
    let mut search = false;
    for server_tool in &request.server_tools {
        match server_tool.kind {
            ServerToolKind::WebSearch if !search => {
                search = true;
                tools.push(json!({ "googleSearch": {} }));
            }
            ServerToolKind::WebSearch => {}
            ServerToolKind::Other => {
                tracing::warn!("Gemini 上游不支持该内置工具，已从请求中剔除")
            }
        }
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if declarations.is_empty() {
        return;
    }
    let config = match &request.tool_choice {
        ToolChoice::Auto => json!({ "mode": "AUTO" }),
        ToolChoice::None => json!({ "mode": "NONE" }),
        ToolChoice::Required => json!({ "mode": "ANY" }),
        ToolChoice::Named(name) if request.tools.iter().any(|t| &t.name == name) => {
            json!({ "mode": "ANY", "allowedFunctionNames": [name] })
        }
        // 指向被剔除的内置工具或不存在的函数：退回 AUTO
        ToolChoice::Named(_) => json!({ "mode": "AUTO" }),
    };
    body.insert(
        "toolConfig".into(),
        json!({ "functionCallingConfig": config }),
    );
}

fn generation_config(request: &Request, options: &RenderOptions<'_>) -> Value {
    let mut config = Map::new();
    config.insert(
        "maxOutputTokens".into(),
        json!(request
            .max_output_tokens
            .unwrap_or(options.default_max_output_tokens)),
    );
    if let Some(temperature) = request.temperature {
        config.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        config.insert("topP".into(), json!(top_p));
    }
    if let Some(top_k) = request.top_k {
        config.insert("topK".into(), json!(top_k));
    }
    if !request.stop.is_empty() {
        config.insert("stopSequences".into(), json!(request.stop));
    }
    let budget = match request.reasoning {
        Reasoning::Unspecified | Reasoning::Adaptive(None) => None,
        Reasoning::Disabled => Some(0),
        Reasoning::Effort(effort) | Reasoning::Adaptive(Some(effort)) => {
            Some(effort.budget_tokens())
        }
        Reasoning::Budget(budget) => Some(budget),
    };
    if let Some(budget) = budget {
        let thinking = if budget == 0 {
            json!({ "thinkingBudget": 0 })
        } else {
            json!({ "thinkingBudget": budget, "includeThoughts": true })
        };
        config.insert("thinkingConfig".into(), thinking);
    }
    Value::Object(config)
}

// ---------------------------------------------------------------------------
// 响应解析
// ---------------------------------------------------------------------------

/// 顶层 `error` 的可读 message（流式错误可能是单元素数组）；没有错误时为 None
pub fn error_message(body: &Value) -> Option<String> {
    let body = match body {
        Value::Array(items) => items.first()?,
        other => other,
    };
    let error = body.get("error")?;
    Some(match error {
        Value::String(message) => message.clone(),
        _ => error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.get("status").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
    })
}

/// Gemini usageMetadata → IR（三桶互斥）。
///
/// Gemini 的 `candidatesTokenCount` 不含思考 token（`thoughtsTokenCount` 单列，但同样按输出
/// 计费，`totalTokenCount = prompt + candidates + thoughts (+ toolUsePrompt)`），所以 output
/// 取两者之和；`candidatesTokenCount` 缺失时退化为 `total − prompt − toolUsePrompt`
pub fn parse_usage(usage: &Value) -> Usage {
    let number = |key: &str| usage.get(key).and_then(Value::as_u64);
    let prompt = number("promptTokenCount").unwrap_or(0);
    let cached = number("cachedContentTokenCount").unwrap_or(0);
    let thoughts = number("thoughtsTokenCount");
    let output = match number("candidatesTokenCount") {
        Some(candidates) => candidates + thoughts.unwrap_or(0),
        None => number("totalTokenCount")
            .unwrap_or(0)
            .saturating_sub(prompt + number("toolUsePromptTokenCount").unwrap_or(0)),
    };
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: output,
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        reasoning_tokens: thoughts,
    }
}

fn block_reason(body: &Value) -> Option<&str> {
    body.pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
}

fn stop_reason(reason: Option<&str>, has_tool_calls: bool) -> StopReason {
    match reason {
        None | Some("STOP") | Some("FINISH_REASON_UNSPECIFIED") => {
            if has_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
        Some("MAX_TOKENS") => StopReason::MaxTokens,
        Some("SAFETY")
        | Some("RECITATION")
        | Some("SPII")
        | Some("BLOCKLIST")
        | Some("PROHIBITED_CONTENT") => StopReason::Refusal,
        Some(other) => {
            tracing::warn!(finish_reason = other, "未知的 Gemini finishReason");
            StopReason::Other(other.to_string())
        }
    }
}

/// 非流式 Gemini 响应 → IR。顶层 `error` 返回 Err；`promptFeedback.blockReason` → Refusal 且无内容
pub fn parse_response(body: &Value, fallback_model: &str) -> Result<Response, String> {
    if let Some(message) = error_message(body) {
        return Err(message);
    }
    if block_reason(body).is_none() && body.pointer("/candidates/0").is_none() {
        return Err("gemini response has no candidates".into());
    }
    let mut decoder = StreamDecoder::new(fallback_model);
    let mut events = Vec::new();
    decoder.feed_value(body, &mut events);
    // 非流式缺 finishReason 按 STOP 处理，不视为截断
    decoder.finalize(&mut events);
    assemble(events)
}

/// 规范化事件 → 非流式响应（块 index 从 0 连续递增，即 content 下标）
fn assemble(events: Vec<Event>) -> Result<Response, String> {
    let mut response = Response::default();
    for event in events {
        match event {
            Event::Start { id, model, .. } => {
                response.id = id;
                response.model = model;
            }
            Event::BlockStart { kind, .. } => response.content.push(match kind {
                BlockKind::Text => Block::Text {
                    text: String::new(),
                },
                BlockKind::Thinking => Block::Thinking {
                    text: String::new(),
                    signature: None,
                },
                BlockKind::RedactedThinking => Block::RedactedThinking {
                    signature: Signature {
                        source: Interface::Gemini,
                        value: Value::Null,
                    },
                },
                BlockKind::ToolCall { id, name } => Block::ToolCall {
                    id,
                    name,
                    arguments: String::new(),
                    signature: None,
                },
                BlockKind::ServerToolUse { id, name, input } => {
                    Block::ServerToolUse { id, name, input }
                }
                BlockKind::ServerToolResult { call_id, content } => {
                    Block::ServerToolResult { call_id, content }
                }
            }),
            Event::TextDelta { index, text } => {
                if let Some(Block::Text { text: buffer }) = response.content.get_mut(index) {
                    buffer.push_str(&text);
                }
            }
            Event::ThinkingDelta { index, text } => {
                if let Some(Block::Thinking { text: buffer, .. }) = response.content.get_mut(index)
                {
                    buffer.push_str(&text);
                }
            }
            Event::SignatureDelta { index, signature } => match response.content.get_mut(index) {
                Some(Block::Thinking {
                    signature: slot, ..
                }) => *slot = Some(signature),
                Some(Block::RedactedThinking { signature: slot }) => *slot = signature,
                _ => {}
            },
            Event::ToolArgumentsDelta {
                index,
                partial_json,
            } => {
                if let Some(Block::ToolCall { arguments, .. }) = response.content.get_mut(index) {
                    arguments.push_str(&partial_json);
                }
            }
            Event::BlockStop { .. } => {}
            Event::Finish { stop_reason, usage } => {
                response.stop_reason = stop_reason;
                response.usage = usage;
            }
            Event::Error { message, .. } => return Err(message),
        }
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// 流式解码
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenKind {
    Text,
    Thinking,
}

#[derive(Debug)]
struct SeenCall {
    id: String,
    /// 已输出过承载签名的 RedactedThinking 块
    signed: bool,
}

/// 从文本开头跳过 `skip` 字节（累计快照中已输出的部分），返回剩余差量
fn consume<'a>(skip: &mut usize, text: &'a str) -> &'a str {
    if *skip >= text.len() {
        *skip -= text.len();
        return "";
    }
    let delta = text.get(*skip..).unwrap_or(text);
    *skip = 0;
    delta
}

/// Gemini SSE 数据帧（每个为一个 `GenerateContentResponse` 片段）→ 规范化事件
#[derive(Debug)]
pub struct StreamDecoder {
    fallback_model: String,
    started: bool,
    finished: bool,
    next_index: usize,
    open: Option<(OpenKind, usize)>,
    /// 当前打开的 Thinking 块待发的文本型签名（在 BlockStop 前发出，保证至多一次）
    pending_signature: Option<String>,
    /// 已输出的普通文本 / 思考文本，用于识别累计快照
    text: String,
    thought: String,
    /// 出现过不以已累计文本为前缀的片段：确定为增量模式，不再做快照判定
    incremental: bool,
    /// 已处理的文本型签名（快照会重复发送）
    text_signatures: HashSet<String>,
    /// 去重键（真实 id 或 name + args）→ 按出现顺序的调用
    calls: HashMap<String, Vec<SeenCall>>,
    saw_tool_calls: bool,
    finish_reason: Option<String>,
    blocked: bool,
    usage: Option<Usage>,
}

impl StreamDecoder {
    pub fn new(fallback_model: impl Into<String>) -> Self {
        Self {
            fallback_model: fallback_model.into(),
            started: false,
            finished: false,
            next_index: 0,
            open: None,
            pending_signature: None,
            text: String::new(),
            thought: String::new(),
            incremental: false,
            text_signatures: HashSet::new(),
            calls: HashMap::new(),
            saw_tool_calls: false,
            finish_reason: None,
            blocked: false,
            usage: None,
        }
    }

    /// 处理一个 SSE 事件的 data
    pub fn feed(&mut self, data: &str) -> Vec<Event> {
        let mut out = Vec::new();
        let data = data.trim();
        if data.is_empty() || self.finished {
            return out;
        }
        match serde_json::from_str::<Value>(data) {
            // 未加 alt=sse 的端点返回 JSON 数组：逐个处理
            Ok(Value::Array(chunks)) => {
                for chunk in &chunks {
                    self.feed_value(chunk, &mut out);
                }
            }
            Ok(chunk) => self.feed_value(&chunk, &mut out),
            Err(_) => tracing::debug!("忽略无法解析的 Gemini 流数据帧"),
        }
        out
    }

    /// 上游流结束：没收到 finishReason（或 blockReason）时产生 Error；已结束则为空
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if self.finish_reason.is_none() && !self.blocked {
            self.finished = true;
            out.push(Event::Error {
                kind: "api_error".into(),
                message: "upstream stream ended before the response completed".into(),
            });
            return out;
        }
        self.finalize(&mut out);
        out
    }

    fn feed_value(&mut self, chunk: &Value, out: &mut Vec<Event>) {
        if self.finished {
            return;
        }
        if let Some(message) = error_message(chunk) {
            out.push(Event::Error {
                kind: "api_error".into(),
                message,
            });
            self.finished = true;
            return;
        }
        if let Some(usage) = chunk.get("usageMetadata").filter(|u| u.is_object()) {
            self.usage = Some(parse_usage(usage));
        }
        // finishReason 之后的片段只补 usage，随即结束
        if self.finish_reason.is_some() || self.blocked {
            self.finalize(out);
            return;
        }
        self.start(chunk, out);
        if block_reason(chunk).is_some() {
            self.blocked = true;
            return;
        }
        let Some(candidate) = chunk.pointer("/candidates/0") else {
            return;
        };
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            self.parts(parts, out);
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
    }

    fn start(&mut self, chunk: &Value, out: &mut Vec<Event>) {
        if self.started {
            return;
        }
        self.started = true;
        let usage = self
            .usage
            .map(|usage| Usage {
                input_tokens: usage.input_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                ..Default::default()
            })
            .unwrap_or_default();
        out.push(Event::Start {
            id: chunk
                .get("responseId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("resp_{}", random_hex())),
            model: chunk
                .get("modelVersion")
                .and_then(Value::as_str)
                .unwrap_or(&self.fallback_model)
                .to_string(),
            usage,
        });
    }

    /// 本片段某类文本应跳过的字节数：全部同类文本以已输出文本为前缀时视为累计快照
    fn snapshot_skip(&mut self, parts: &[Value], thought: bool) -> usize {
        let accumulated = if thought { &self.thought } else { &self.text };
        let total: String = parts
            .iter()
            .filter(|part| part.get("functionCall").is_none())
            .filter(|part| (part.get("thought").and_then(Value::as_bool) == Some(true)) == thought)
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect();
        if self.incremental || accumulated.is_empty() || total.is_empty() {
            return 0;
        }
        if total.starts_with(accumulated.as_str()) {
            accumulated.len()
        } else {
            self.incremental = true;
            0
        }
    }

    fn parts(&mut self, parts: &[Value], out: &mut Vec<Event>) {
        let mut text_skip = self.snapshot_skip(parts, false);
        let mut thought_skip = self.snapshot_skip(parts, true);
        // 本片段内各去重键的出现次数：同一片段内相同的并行调用各自保留
        let mut occurrences: HashMap<String, usize> = HashMap::new();
        for part in parts {
            let signature = part
                .get("thoughtSignature")
                .and_then(Value::as_str)
                .filter(|sig| !sig.is_empty());
            if let Some(call) = part.get("functionCall") {
                self.function_call(call, signature, &mut occurrences, out);
                continue;
            }
            let text = part.get("text").and_then(Value::as_str);
            if text.is_none() && signature.is_none() {
                continue; // inlineData、executableCode 等不建模
            }
            let text = text.unwrap_or_default();
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                let delta = consume(&mut thought_skip, text);
                if !delta.is_empty() {
                    self.thought.push_str(delta);
                    let index = self.open_block(OpenKind::Thinking, out);
                    out.push(Event::ThinkingDelta {
                        index,
                        text: delta.to_string(),
                    });
                }
                if let Some(sig) = signature {
                    self.text_signature(sig, out);
                }
            } else {
                // 签名先于该 part 的文本落位：附到当前 Thinking 块，或在文本之前新建一个
                if let Some(sig) = signature {
                    self.text_signature(sig, out);
                }
                let delta = consume(&mut text_skip, text);
                if !delta.is_empty() {
                    self.text.push_str(delta);
                    let index = self.open_block(OpenKind::Text, out);
                    out.push(Event::TextDelta {
                        index,
                        text: delta.to_string(),
                    });
                }
            }
        }
    }

    fn alloc_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn close_open(&mut self, out: &mut Vec<Event>) {
        if let Some((kind, index)) = self.open.take() {
            if kind == OpenKind::Thinking {
                if let Some(sig) = self.pending_signature.take() {
                    out.push(Event::SignatureDelta {
                        index,
                        signature: text_signature(&sig),
                    });
                }
            }
            out.push(Event::BlockStop { index });
        }
    }

    /// 打开（或沿用）文本 / 思考块，返回其 index
    fn open_block(&mut self, kind: OpenKind, out: &mut Vec<Event>) -> usize {
        if let Some((open_kind, index)) = self.open {
            if open_kind == kind {
                return index;
            }
        }
        self.close_open(out);
        let index = self.alloc_index();
        out.push(Event::BlockStart {
            index,
            kind: match kind {
                OpenKind::Text => BlockKind::Text,
                OpenKind::Thinking => BlockKind::Thinking,
            },
        });
        self.open = Some((kind, index));
        index
    }

    /// 文本 / 思考 part 上的签名：附到当前打开的 Thinking 块；没有则新建一个空文本 Thinking 块承载
    fn text_signature(&mut self, sig: &str, out: &mut Vec<Event>) {
        if !self.text_signatures.insert(sig.to_string()) {
            return;
        }
        if matches!(self.open, Some((OpenKind::Thinking, _))) && self.pending_signature.is_none() {
            self.pending_signature = Some(sig.to_string());
            return;
        }
        self.close_open(out);
        let index = self.alloc_index();
        out.push(Event::BlockStart {
            index,
            kind: BlockKind::Thinking,
        });
        out.push(Event::SignatureDelta {
            index,
            signature: text_signature(sig),
        });
        out.push(Event::BlockStop { index });
    }

    /// functionCall 上的签名：单独一个 RedactedThinking 块（§5.5 紧邻 tool_use 之前）
    fn call_signature_block(&mut self, sig: &str, call_id: &str, out: &mut Vec<Event>) {
        self.close_open(out);
        let index = self.alloc_index();
        out.push(Event::BlockStart {
            index,
            kind: BlockKind::RedactedThinking,
        });
        out.push(Event::SignatureDelta {
            index,
            signature: call_signature(sig, call_id),
        });
        out.push(Event::BlockStop { index });
    }

    fn function_call(
        &mut self,
        call: &Value,
        signature: Option<&str>,
        occurrences: &mut HashMap<String, usize>,
        out: &mut Vec<Event>,
    ) {
        self.saw_tool_calls = true;
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let arguments = match call.get("args") {
            Some(args) if !args.is_null() => args.to_string(),
            _ => "{}".to_string(),
        };
        let real_id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let key = match real_id {
            Some(id) => format!("id:{id}"),
            None => format!("fn:{name}:{arguments}"),
        };
        let occurrence = {
            let count = occurrences.entry(key.clone()).or_default();
            *count += 1;
            *count - 1
        };

        // 之前片段已输出过的同一调用：只补发此前缺失的签名
        let seen = self.calls.entry(key).or_default();
        if let Some(existing) = seen.get_mut(occurrence) {
            if let Some(sig) = signature {
                if !existing.signed {
                    existing.signed = true;
                    let id = existing.id.clone();
                    self.call_signature_block(sig, &id, out);
                }
            }
            return;
        }
        let id = real_id
            .map(str::to_string)
            .unwrap_or_else(generated_call_id);
        seen.push(SeenCall {
            id: id.clone(),
            signed: signature.is_some(),
        });

        self.close_open(out);
        if let Some(sig) = signature {
            self.call_signature_block(sig, &id, out);
        }
        let index = self.alloc_index();
        out.push(Event::BlockStart {
            index,
            kind: BlockKind::ToolCall { id, name },
        });
        out.push(Event::ToolArgumentsDelta {
            index,
            partial_json: arguments,
        });
        out.push(Event::BlockStop { index });
    }

    fn finalize(&mut self, out: &mut Vec<Event>) {
        if self.finished {
            return;
        }
        if !self.started {
            self.start(&Value::Null, out);
        }
        self.close_open(out);
        self.finished = true;
        let stop_reason = if self.blocked {
            StopReason::Refusal
        } else {
            stop_reason(self.finish_reason.as_deref(), self.saw_tool_calls)
        };
        out.push(Event::Finish {
            stop_reason,
            usage: self.usage.unwrap_or_default(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Effort, ServerTool};
    use crate::sse::SseParser;

    fn opts(model: &str) -> RenderOptions<'_> {
        RenderOptions {
            upstream_model: model,
            default_max_output_tokens: 16384,
        }
    }

    fn text(text: &str) -> Block {
        Block::Text { text: text.into() }
    }

    fn call(id: &str, name: &str, arguments: &str, signature: Option<Signature>) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            signature,
        }
    }

    fn result(call_id: &str, content: Vec<Block>) -> Block {
        Block::ToolResult {
            call_id: call_id.into(),
            content,
            is_error: false,
        }
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

    fn png() -> MediaSource {
        MediaSource::Base64 {
            media_type: "image/png".into(),
            data: "iVBOR".into(),
        }
    }

    fn render(request: &Request) -> Value {
        render_request(request, &opts("gemini-2.5-pro")).unwrap()
    }

    #[test]
    fn request_path_variants() {
        assert_eq!(
            request_path("gemini-2.5-pro", false),
            ("/v1beta/models/gemini-2.5-pro:generateContent".into(), None)
        );
        assert_eq!(
            request_path("models/gemini-3-pro", true),
            (
                "/v1beta/models/gemini-3-pro:streamGenerateContent".into(),
                Some("alt=sse".into())
            )
        );
    }

    #[test]
    fn system_roles_and_turn_merging() {
        let request = Request {
            model: "claude".into(),
            system: vec![
                "x-anthropic-billing-header: cch=1\n\nYou are Claude Code.".into(),
                "Rules".into(),
            ],
            messages: vec![
                user(vec![text("a")]),
                user(vec![text("b"), text("")]),
                assistant(vec![
                    Block::Thinking {
                        text: "hidden".into(),
                        signature: None,
                    },
                    text("c"),
                ]),
                assistant(vec![text("d")]),
            ],
            max_output_tokens: Some(100),
            temperature: Some(0.5),
            top_p: Some(0.9),
            top_k: Some(40),
            stop: vec!["END".into()],
            stream: true,
            ..Default::default()
        };
        let body = render(&request);
        assert!(body.get("model").is_none());
        assert_eq!(
            body["systemInstruction"],
            json!({"parts": [{"text": "You are Claude Code."}, {"text": "Rules"}]})
        );
        assert_eq!(
            body["contents"],
            json!([
                {"role": "user", "parts": [{"text": "a"}, {"text": "b"}]},
                {"role": "model", "parts": [{"text": "c"}, {"text": "d"}]},
            ])
        );
        assert_eq!(
            body["generationConfig"],
            json!({"maxOutputTokens": 100, "temperature": 0.5, "topP": 0.9, "topK": 40, "stopSequences": ["END"]})
        );
        assert!(body.get("tools").is_none() && body.get("toolConfig").is_none());
    }

    #[test]
    fn function_response_name_backfill_and_degradation() {
        let request = Request {
            messages: vec![
                user(vec![text("go")]),
                assistant(vec![
                    call("call_abc", "Bash", r#"{"command":"ls"}"#, None),
                    call("toolu_1", "Read", "", None),
                ]),
                user(vec![
                    text("and?"),
                    result("call_abc", vec![text("a.txt"), text("b.txt")]),
                    result(
                        "toolu_1",
                        vec![Block::Image { source: png() }, text("see image")],
                    ),
                    result("missing", vec![text("orphan")]),
                ]),
            ],
            ..Default::default()
        };
        let body = render(&request);
        let contents = body["contents"].as_array().unwrap();
        // 生成的 call_ id 不写回；其他 id 原样写入
        assert_eq!(
            contents[1]["parts"],
            json!([
                {"functionCall": {"name": "Bash", "args": {"command": "ls"}}},
                {"functionCall": {"name": "Read", "args": {}, "id": "toolu_1"}},
            ])
        );
        assert_eq!(
            contents[2]["parts"],
            json!([
                {"functionResponse": {"name": "Bash", "response": {"content": "a.txt\nb.txt"}}},
                {"functionResponse": {"name": "Read", "id": "toolu_1", "response": {"content": "see image"}}},
                {"inlineData": {"mimeType": "image/png", "data": "iVBOR"}},
                {"text": "[tool result missing]\norphan"},
                {"text": "and?"},
            ])
        );
    }

    #[test]
    fn media_is_inline_only() {
        let mut request = Request {
            messages: vec![user(vec![
                Block::Image { source: png() },
                Block::Document {
                    source: MediaSource::Base64 {
                        media_type: "application/pdf".into(),
                        data: "JVBER".into(),
                    },
                    name: Some("a.pdf".into()),
                },
            ])],
            ..Default::default()
        };
        assert_eq!(
            render(&request)["contents"][0]["parts"],
            json!([
                {"inlineData": {"mimeType": "image/png", "data": "iVBOR"}},
                {"inlineData": {"mimeType": "application/pdf", "data": "JVBER"}},
            ])
        );

        request.messages.push(user(vec![Block::Image {
            source: MediaSource::Url("https://x/a.png".into()),
        }]));
        assert!(render_request(&request, &opts("m")).is_err());

        let request = Request {
            messages: vec![
                assistant(vec![call("c1", "Shot", "{}", None)]),
                user(vec![result(
                    "c1",
                    vec![Block::Document {
                        source: MediaSource::Url("https://x/a.pdf".into()),
                        name: None,
                    }],
                )]),
            ],
            ..Default::default()
        };
        assert!(render_request(&request, &opts("m")).is_err());
    }

    #[test]
    fn tool_arguments_must_be_an_object() {
        for bad in ["[1]", "not json", "\"s\""] {
            let request = Request {
                messages: vec![assistant(vec![call("c", "T", bad, None)])],
                ..Default::default()
            };
            assert!(render_request(&request, &opts("m")).is_err(), "{bad}");
        }
    }

    fn tool(name: &str, parameters: Value) -> Tool {
        Tool {
            name: name.into(),
            description: None,
            parameters,
            custom: false,
        }
    }

    #[test]
    fn schema_subset_selects_parameters_or_json_schema() {
        let request = Request {
            tools: vec![
                Tool {
                    description: Some("run".into()),
                    ..tool(
                        "Simple",
                        json!({
                            "type": "object",
                            "properties": {
                                "url": {"type": "string", "format": "uri", "description": "u"},
                                "n": {"type": "integer", "minimum": 0, "maximum": 9, "default": 1},
                                "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}, "minItems": 1},
                                "v": {"anyOf": [{"type": "string"}, {"type": "number", "nullable": true}]}
                            },
                            "required": ["url"]
                        }),
                    )
                },
                tool("Empty", json!(null)),
                tool(
                    "Strict",
                    json!({
                        "$schema": "http://json-schema.org/draft-07/schema#",
                        "type": "object",
                        "properties": {"p": {"type": "string", "format": "uri"}},
                        "additionalProperties": false
                    }),
                ),
                tool(
                    "Union",
                    json!({"type": "object", "properties": {"x": {"type": ["string", "null"]}}}),
                ),
                tool(
                    "OneOf",
                    json!({"type": "object", "properties": {"x": {"oneOf": [{"type": "string"}]}}}),
                ),
            ],
            ..Default::default()
        };
        let body = render(&request);
        let declarations = body["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(declarations[0]["description"], "run");
        assert!(declarations[0].get("parametersJsonSchema").is_none());
        assert!(declarations[0]["parameters"]["properties"]["url"]
            .get("format")
            .is_none());
        assert_eq!(declarations[0]["parameters"]["required"], json!(["url"]));
        assert_eq!(
            declarations[1],
            json!({"name": "Empty", "parameters": {"type": "object", "properties": {}}})
        );
        assert_eq!(
            declarations[2],
            json!({"name": "Strict", "parametersJsonSchema": {
                "type": "object",
                "properties": {"p": {"type": "string"}},
                "additionalProperties": false
            }})
        );
        assert!(declarations[3].get("parametersJsonSchema").is_some());
        assert!(declarations[4].get("parametersJsonSchema").is_some());
    }

    #[test]
    fn tool_config_modes_and_server_tools() {
        let base = Request {
            tools: vec![tool("Bash", json!({})), tool("Read", json!({}))],
            ..Default::default()
        };
        let cases = [
            (ToolChoice::Auto, json!({"mode": "AUTO"})),
            (ToolChoice::None, json!({"mode": "NONE"})),
            (ToolChoice::Required, json!({"mode": "ANY"})),
            (
                ToolChoice::Named("Read".into()),
                json!({"mode": "ANY", "allowedFunctionNames": ["Read"]}),
            ),
            (
                ToolChoice::Named("web_search".into()),
                json!({"mode": "AUTO"}),
            ),
        ];
        for (choice, expected) in cases {
            let request = Request {
                tool_choice: choice,
                ..base.clone()
            };
            assert_eq!(
                render(&request)["toolConfig"],
                json!({ "functionCallingConfig": expected })
            );
        }

        let request = Request {
            tool_choice: ToolChoice::Required,
            server_tools: vec![
                ServerTool {
                    kind: ServerToolKind::WebSearch,
                    raw: json!({}),
                },
                ServerTool {
                    kind: ServerToolKind::Other,
                    raw: json!({}),
                },
            ],
            ..Default::default()
        };
        let body = render(&request);
        assert_eq!(body["tools"], json!([{"googleSearch": {}}]));
        assert!(
            body.get("toolConfig").is_none(),
            "no function declarations → no toolConfig"
        );
    }

    #[test]
    fn thinking_config_levels() {
        let cases = [
            (Reasoning::Unspecified, None),
            (Reasoning::Adaptive(None), None),
            (Reasoning::Disabled, Some(json!({"thinkingBudget": 0}))),
            (
                Reasoning::Effort(Effort::Minimal),
                Some(json!({"thinkingBudget": 2048, "includeThoughts": true})),
            ),
            (
                Reasoning::Effort(Effort::Low),
                Some(json!({"thinkingBudget": 2048, "includeThoughts": true})),
            ),
            (
                Reasoning::Effort(Effort::Medium),
                Some(json!({"thinkingBudget": 8192, "includeThoughts": true})),
            ),
            (
                Reasoning::Adaptive(Some(Effort::High)),
                Some(json!({"thinkingBudget": 16384, "includeThoughts": true})),
            ),
            (
                Reasoning::Effort(Effort::XHigh),
                Some(json!({"thinkingBudget": 24576, "includeThoughts": true})),
            ),
            (
                Reasoning::Budget(5000),
                Some(json!({"thinkingBudget": 5000, "includeThoughts": true})),
            ),
        ];
        for (reasoning, expected) in cases {
            let request = Request {
                reasoning,
                ..Default::default()
            };
            let config = render(&request)["generationConfig"].clone();
            assert_eq!(config["maxOutputTokens"], 16384);
            assert_eq!(
                config.get("thinkingConfig").cloned(),
                expected,
                "{reasoning:?}"
            );
        }
    }

    #[test]
    fn signatures_land_on_their_parts() {
        let request = Request {
            messages: vec![
                user(vec![text("q")]),
                assistant(vec![
                    // 其他来源的推理块丢弃
                    Block::Thinking {
                        text: "claude".into(),
                        signature: Some(Signature {
                            source: Interface::Claude,
                            value: json!({"type": "thinking", "signature": "x"}),
                        }),
                    },
                    Block::Thinking {
                        text: "gemini thought".into(),
                        signature: Some(text_signature("S_TEXT")),
                    },
                    text("answer"),
                    Block::RedactedThinking {
                        signature: call_signature("S_B", "call_b"),
                    },
                    call("call_a", "A", "{}", Some(call_signature("S_A", "call_a"))),
                    call("call_b", "B", "{}", None),
                    call("call_c", "C", "{}", None),
                ]),
                user(vec![result("call_a", vec![text("1")])]),
                assistant(vec![
                    text("done"),
                    Block::Thinking {
                        text: String::new(),
                        signature: Some(text_signature("S_END")),
                    },
                ]),
            ],
            ..Default::default()
        };
        let contents = render(&request)["contents"].clone();
        assert_eq!(
            contents[1]["parts"],
            json!([
                {"text": "answer"},
                {"functionCall": {"name": "A", "args": {}}, "thoughtSignature": "S_A"},
                {"functionCall": {"name": "B", "args": {}}, "thoughtSignature": "S_B"},
                // 最后一个 part：文本型签名
                {"functionCall": {"name": "C", "args": {}}, "thoughtSignature": "S_TEXT"},
            ])
        );
        assert_eq!(
            contents[3]["parts"],
            json!([{"text": "done", "thoughtSignature": "S_END"}])
        );
    }

    #[test]
    fn parsed_signatures_are_replayed() {
        let body = json!({
            "responseId": "r1",
            "modelVersion": "gemini-3-pro",
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "plan", "thought": true},
                {"text": "Hello", "thoughtSignature": "SIG_T"},
                {"functionCall": {"name": "Bash", "args": {"c": "ls"}}, "thoughtSignature": "SIG_C"},
            ]}, "finishReason": "STOP"}]
        });
        let response = parse_response(&body, "fallback").unwrap();
        let request = Request {
            messages: vec![user(vec![text("q")]), assistant(response.content.clone())],
            ..Default::default()
        };
        assert_eq!(
            render(&request)["contents"][1]["parts"],
            json!([
                {"text": "Hello"},
                {"functionCall": {"name": "Bash", "args": {"c": "ls"}}, "thoughtSignature": "SIG_C"},
            ])
        );

        // 文本型签名在最后一个 part 没有自带签名时落在最后一个 part
        let body = json!({"candidates": [{"content": {"parts": [
            {"text": "Hi"}, {"text": "", "thoughtSignature": "SIG_END"}
        ]}, "finishReason": "STOP"}]});
        let response = parse_response(&body, "m").unwrap();
        let request = Request {
            messages: vec![user(vec![text("q")]), assistant(response.content)],
            ..Default::default()
        };
        assert_eq!(
            render(&request)["contents"][1]["parts"],
            json!([{"text": "Hi", "thoughtSignature": "SIG_END"}])
        );
    }

    #[test]
    fn parses_text_thinking_and_calls() {
        let body = json!({
            "responseId": "r1",
            "modelVersion": "gemini-3-pro-preview",
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "think ", "thought": true},
                {"text": "more", "thought": true},
                {"text": "Hello", "thoughtSignature": "SIG_T"},
                {"text": " world"},
                {"functionCall": {"name": "Bash", "args": {"c": "ls"}}, "thoughtSignature": "SIG_C"},
                {"functionCall": {"name": "Bash", "args": {"c": "ls"}}},
                {"functionCall": {"id": "real-1", "name": "Read"}},
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 30, "candidatesTokenCount": 20, "thoughtsTokenCount": 7, "totalTokenCount": 127}
        });
        let response = parse_response(&body, "fallback").unwrap();
        assert_eq!(response.id, "r1");
        assert_eq!(response.model, "gemini-3-pro-preview");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 70,
                output_tokens: 27,
                cache_read_tokens: 30,
                cache_write_tokens: 0,
                reasoning_tokens: Some(7),
            }
        );
        let content = &response.content;
        assert_eq!(content.len(), 6, "{content:#?}");
        assert_eq!(
            content[0],
            Block::Thinking {
                text: "think more".into(),
                signature: Some(text_signature("SIG_T")),
            }
        );
        assert_eq!(content[1], text("Hello world"));
        let ids: Vec<&str> = content
            .iter()
            .filter_map(|block| match block {
                Block::ToolCall { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 3);
        // 同一片段中两个相同的并行调用各自保留，生成的 id 互不相同
        assert!(ids[0].starts_with("call_") && ids[0].len() == 37, "{ids:?}");
        assert!(ids[1].starts_with("call_") && ids[0] != ids[1]);
        assert_eq!(ids[2], "real-1");
        assert_eq!(
            content[2],
            Block::RedactedThinking {
                signature: call_signature("SIG_C", ids[0]),
            }
        );
        assert!(
            matches!(&content[3], Block::ToolCall { name, arguments, signature: None, .. } if name == "Bash" && arguments == r#"{"c":"ls"}"#)
        );
        assert!(
            matches!(&content[5], Block::ToolCall { arguments, .. } if arguments == "{}"),
            "missing args → {{}}"
        );
    }

    #[test]
    fn text_signature_without_thinking_creates_an_empty_thinking_block() {
        let body = json!({"candidates": [{"content": {"parts": [
            {"text": "A"}, {"text": "B", "thoughtSignature": "S"}
        ]}, "finishReason": "STOP"}]});
        let response = parse_response(&body, "fallback").unwrap();
        assert_eq!(response.model, "fallback");
        assert_eq!(
            response.content,
            vec![
                text("A"),
                Block::Thinking {
                    text: String::new(),
                    signature: Some(text_signature("S")),
                },
                text("B"),
            ]
        );
    }

    #[test]
    fn finish_reasons_prompt_feedback_and_errors() {
        let reason = |value: &str, parts: Value| {
            parse_response(
                &json!({"candidates": [{"content": {"parts": parts}, "finishReason": value}]}),
                "m",
            )
            .unwrap()
            .stop_reason
        };
        let text_parts = json!([{"text": "x"}]);
        assert_eq!(reason("STOP", text_parts.clone()), StopReason::EndTurn);
        assert_eq!(
            reason("FINISH_REASON_UNSPECIFIED", text_parts.clone()),
            StopReason::EndTurn
        );
        assert_eq!(
            reason("STOP", json!([{"functionCall": {"name": "f", "args": {}}}])),
            StopReason::ToolUse
        );
        assert_eq!(
            reason("MAX_TOKENS", text_parts.clone()),
            StopReason::MaxTokens
        );
        for refusal in [
            "SAFETY",
            "RECITATION",
            "SPII",
            "BLOCKLIST",
            "PROHIBITED_CONTENT",
        ] {
            assert_eq!(reason(refusal, json!([])), StopReason::Refusal, "{refusal}");
        }
        assert_eq!(
            reason("MALFORMED_FUNCTION_CALL", json!([])),
            StopReason::Other("MALFORMED_FUNCTION_CALL".into())
        );
        // 缺 finishReason 的非流式响应按 STOP
        assert_eq!(
            parse_response(
                &json!({"candidates": [{"content": {"parts": text_parts}}]}),
                "m"
            )
            .unwrap()
            .stop_reason,
            StopReason::EndTurn
        );

        let blocked = parse_response(
            &json!({"promptFeedback": {"blockReason": "SAFETY"}, "usageMetadata": {"promptTokenCount": 5, "totalTokenCount": 5}}),
            "m",
        )
        .unwrap();
        assert_eq!(blocked.stop_reason, StopReason::Refusal);
        assert!(blocked.content.is_empty());
        assert_eq!(blocked.usage.input_tokens, 5);
        assert_eq!(blocked.usage.output_tokens, 0);

        let error =
            json!({"error": {"code": 429, "message": "quota", "status": "RESOURCE_EXHAUSTED"}});
        assert_eq!(parse_response(&error, "m"), Err("quota".into()));
        assert_eq!(error_message(&json!([error])), Some("quota".into()));
        assert_eq!(error_message(&json!({"candidates": []})), None);
        assert!(parse_response(&json!({"candidates": []}), "m").is_err());
    }

    #[test]
    fn usage_falls_back_to_total_minus_prompt() {
        assert_eq!(
            parse_usage(
                &json!({"promptTokenCount": 10, "totalTokenCount": 25, "thoughtsTokenCount": 4})
            ),
            Usage {
                input_tokens: 10,
                output_tokens: 15,
                reasoning_tokens: Some(4),
                ..Default::default()
            }
        );
    }

    // ---------------------------------------------------------------- 流式

    fn decode(chunks: &[Value]) -> Vec<Event> {
        let mut decoder = StreamDecoder::new("fallback");
        let mut events: Vec<Event> = chunks
            .iter()
            .flat_map(|chunk| decoder.feed(&chunk.to_string()))
            .collect();
        events.extend(decoder.finish());
        events
    }

    fn chunk(parts: Value) -> Value {
        json!({"responseId": "r", "modelVersion": "gemini-x", "candidates": [{"content": {"role": "model", "parts": parts}}]})
    }

    fn finish_chunk(parts: Value, reason: &str) -> Value {
        json!({"candidates": [{"content": {"parts": parts}, "finishReason": reason}],
               "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}})
    }

    /// 把事件折叠为 (块类型, 内容) 便于断言；同时校验 start/stop 配对与 index 递增
    fn blocks(events: &[Event]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut open = None;
        for event in events {
            match event {
                Event::BlockStart { index, kind } => {
                    assert_eq!(*index, out.len());
                    assert!(open.is_none(), "nested block");
                    open = Some(*index);
                    let label = match kind {
                        BlockKind::Text => "text".to_string(),
                        BlockKind::Thinking => "thinking".to_string(),
                        BlockKind::RedactedThinking => "redacted".to_string(),
                        BlockKind::ToolCall { name, .. } => format!("tool:{name}"),
                        BlockKind::ServerToolUse { name, .. } => format!("server:{name}"),
                        BlockKind::ServerToolResult { .. } => "server_result".to_string(),
                    };
                    out.push((label, String::new()));
                }
                Event::TextDelta { index, text } | Event::ThinkingDelta { index, text } => {
                    assert_eq!(open, Some(*index));
                    out[*index].1.push_str(text);
                }
                Event::ToolArgumentsDelta {
                    index,
                    partial_json,
                } => {
                    assert_eq!(open, Some(*index));
                    out[*index].1.push_str(partial_json);
                }
                Event::SignatureDelta { index, signature } => {
                    assert_eq!(open, Some(*index));
                    out[*index]
                        .1
                        .push_str(&format!("<{}>", signature.value["sig"].as_str().unwrap()));
                }
                Event::BlockStop { index } => {
                    assert_eq!(open.take(), Some(*index));
                }
                Event::Finish { .. } => assert!(open.is_none()),
                _ => {}
            }
        }
        out
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn stream_incremental_and_cumulative_text() {
        let incremental = decode(&[
            chunk(json!([{"text": "Hel"}])),
            chunk(json!([{"text": "lo"}])),
            finish_chunk(json!([{"text": "!"}]), "STOP"),
        ]);
        assert_eq!(blocks(&incremental), pairs(&[("text", "Hello!")]));

        let cumulative = decode(&[
            chunk(json!([{"text": "Hel"}])),
            chunk(json!([{"text": "Hello"}])),
            finish_chunk(json!([{"text": "Hel"}, {"text": "lo!"}]), "STOP"),
        ]);
        assert_eq!(blocks(&cumulative), pairs(&[("text", "Hello!")]));
        assert_eq!(
            cumulative[0],
            Event::Start {
                id: "r".into(),
                model: "gemini-x".into(),
                usage: Usage::default()
            }
        );
        assert_eq!(
            cumulative.last(),
            Some(&Event::Finish {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                }
            })
        );

        // 确定为增量模式后不再按快照裁剪
        let sticky = decode(&[
            chunk(json!([{"text": "ab"}])),
            chunk(json!([{"text": "c"}])),
            finish_chunk(json!([{"text": "abc"}]), "STOP"),
        ]);
        assert_eq!(blocks(&sticky), pairs(&[("text", "abcabc")]));
    }

    #[test]
    fn stream_thought_to_text_and_signatures() {
        let events = decode(&[
            chunk(json!([{"text": "plan", "thought": true}])),
            chunk(json!([{"text": " more", "thought": true, "thoughtSignature": "S1"}])),
            chunk(json!([{"text": "Answer"}])),
            // 签名只在某个片段出现在文本 part 上
            chunk(json!([{"text": " done", "thoughtSignature": "S2"}])),
            finish_chunk(json!([]), "STOP"),
        ]);
        assert_eq!(
            blocks(&events),
            pairs(&[
                ("thinking", "plan more<S1>"),
                ("text", "Answer"),
                ("thinking", "<S2>"),
                ("text", " done"),
            ])
        );
        let signature = events
            .iter()
            .find_map(|event| match event {
                Event::SignatureDelta { signature, .. } => Some(signature.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(signature, text_signature("S1"));
    }

    #[test]
    fn stream_function_calls_are_deduplicated_and_late_signatures_attached() {
        let bash = json!({"functionCall": {"name": "Bash", "args": {"c": "ls"}}});
        let read_with_id = json!({"functionCall": {"id": "fc-1", "name": "Read", "args": {}}});
        let events = decode(&[
            chunk(json!([{"text": "ok"}, bash.clone()])),
            // 累计快照重复前面的调用；签名晚到
            chunk(json!([
                {"text": "ok"},
                {"functionCall": {"name": "Bash", "args": {"c": "ls"}}, "thoughtSignature": "SIG"},
                read_with_id.clone()
            ])),
            finish_chunk(json!([{"text": "ok"}, bash, read_with_id]), "STOP"),
        ]);
        assert_eq!(
            blocks(&events),
            pairs(&[
                ("text", "ok"),
                ("tool:Bash", r#"{"c":"ls"}"#),
                ("redacted", "<SIG>"),
                ("tool:Read", "{}"),
            ])
        );
        let bash_id = events
            .iter()
            .find_map(|event| match event {
                Event::BlockStart {
                    kind: BlockKind::ToolCall { id, name },
                    ..
                } if name == "Bash" => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        assert!(events.contains(&Event::SignatureDelta {
            index: 2,
            signature: call_signature("SIG", &bash_id),
        }));
        assert!(matches!(
            events.last(),
            Some(Event::Finish {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn stream_signature_on_call_precedes_the_tool_block() {
        let events = decode(&[finish_chunk(
            json!([{"functionCall": {"name": "A", "args": {"x": 1}}, "thoughtSignature": "SA"}]),
            "STOP",
        )]);
        assert_eq!(
            blocks(&events),
            pairs(&[("redacted", "<SA>"), ("tool:A", r#"{"x":1}"#)])
        );
    }

    #[test]
    fn stream_usage_after_finish_and_truncation() {
        // usageMetadata 在 finishReason 之后的片段
        let events = decode(&[
            json!({"candidates": [{"content": {"parts": [{"text": "a"}]}, "finishReason": "MAX_TOKENS"}]}),
            json!({"usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1}}),
            chunk(json!([{"text": "ignored"}])),
        ]);
        assert_eq!(blocks(&events), pairs(&[("text", "a")]));
        assert_eq!(
            events.last(),
            Some(&Event::Finish {
                stop_reason: StopReason::MaxTokens,
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 1,
                    ..Default::default()
                }
            })
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Finish { .. }))
                .count(),
            1
        );

        let events = decode(&[chunk(json!([{"text": "a"}]))]);
        assert!(matches!(events.last(), Some(Event::Error { .. })));

        let events = decode(&[json!({"error": {"code": 500, "message": "boom"}})]);
        assert_eq!(
            events,
            vec![Event::Error {
                kind: "api_error".into(),
                message: "boom".into()
            }]
        );

        let events =
            decode(&[json!({"promptFeedback": {"blockReason": "OTHER"}, "responseId": "r"})]);
        assert!(matches!(
            events.last(),
            Some(Event::Finish {
                stop_reason: StopReason::Refusal,
                ..
            })
        ));
        assert!(blocks(&events).is_empty());
    }

    /// 生成的调用 id 按首次出现顺序替换为占位符
    fn normalize_ids(events: Vec<Event>) -> Vec<Event> {
        let mut map: HashMap<String, String> = HashMap::new();
        let mut rename = |id: &str| -> String {
            if !id.starts_with(GENERATED_ID_PREFIX) {
                return id.to_string();
            }
            let next = format!("call_#{}", map.len());
            map.entry(id.to_string()).or_insert(next).clone()
        };
        events
            .into_iter()
            .map(|event| match event {
                Event::BlockStart {
                    index,
                    kind: BlockKind::ToolCall { id, name },
                } => Event::BlockStart {
                    index,
                    kind: BlockKind::ToolCall {
                        id: rename(&id),
                        name,
                    },
                },
                Event::SignatureDelta {
                    index,
                    mut signature,
                } => {
                    if let Some(call_id) = signature.value.get("call_id").and_then(Value::as_str) {
                        let renamed = rename(call_id);
                        signature.value["call_id"] = json!(renamed);
                    }
                    Event::SignatureDelta { index, signature }
                }
                other => other,
            })
            .collect()
    }

    fn decode_sse(pieces: &[&[u8]]) -> Vec<Event> {
        let mut parser = SseParser::new();
        let mut decoder = StreamDecoder::new("fallback");
        let mut events = Vec::new();
        for piece in pieces {
            for event in parser.feed(piece) {
                events.extend(decoder.feed(&event.data));
            }
        }
        for event in parser.finish() {
            events.extend(decoder.feed(&event.data));
        }
        events.extend(decoder.finish());
        normalize_ids(events)
    }

    #[test]
    fn chunking_does_not_change_events() {
        let chunks = [
            chunk(json!([{"text": "思考中", "thought": true}])),
            chunk(json!([{"text": "你好", "thoughtSignature": "T"}])),
            chunk(
                json!([{"text": "，世界"}, {"functionCall": {"name": "A", "args": {"q": "é"}}, "thoughtSignature": "C"}]),
            ),
            chunk(json!([{"functionCall": {"name": "B", "args": {}}}])),
            finish_chunk(json!([]), "STOP"),
        ];
        let sse: String = chunks
            .iter()
            .map(|c| format!("data: {c}\r\n\r\n"))
            .collect();
        let bytes = sse.as_bytes();
        let expected = decode_sse(&[bytes]);
        assert_eq!(
            blocks(&expected),
            pairs(&[
                ("thinking", "思考中<T>"),
                ("text", "你好，世界"),
                ("redacted", "<C>"),
                ("tool:A", r#"{"q":"é"}"#),
                ("tool:B", "{}"),
            ])
        );
        for split in 1..bytes.len() {
            assert_eq!(
                decode_sse(&[&bytes[..split], &bytes[split..]]),
                expected,
                "split at {split}"
            );
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(decode_sse(&singles), expected);
        for size in [3, 7, 13] {
            let pieces: Vec<&[u8]> = bytes.chunks(size).collect();
            assert_eq!(decode_sse(&pieces), expected, "chunks of {size}");
        }
    }
}
