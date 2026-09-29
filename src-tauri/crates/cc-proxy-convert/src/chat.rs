//! OpenAI Chat Completions 协议（设计文档 §5、§6.2）：上游侧请求生成、响应与流式解析。
//!
//! Chat 作为客户端协议在后续阶段实现。

use serde_json::{json, Map, Value};

use crate::ir::{
    Block, BlockKind, Event, MediaSource, Message, Request, Response, Role, ServerToolKind,
    StopReason, Tool, ToolChoice, Usage,
};

/// Claude Code 放在 system 开头的计费行：值每次请求都变，转发给其他协议会破坏上游的前缀缓存
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// 去掉 system 开头的计费行（只处理开头，保留用户写在别处的同名文本）
pub fn strip_leading_billing_header(text: &str) -> &str {
    if !text.starts_with(BILLING_HEADER_PREFIX) {
        return text;
    }
    match text.find(['\n', '\r']) {
        None => "",
        Some(end) => {
            let rest = &text[end..];
            let rest = rest
                .strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix('\n'))
                .or_else(|| rest.strip_prefix('\r'))
                .unwrap_or(rest);
            // 计费行后通常还有一个空行
            rest.strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix('\n'))
                .or_else(|| rest.strip_prefix('\r'))
                .unwrap_or(rest)
        }
    }
}

/// 支持 `reasoning_effort` 的模型族：o 系列、gpt-5 及以上、grok-4.5 及以上。
/// 向其他模型发送该字段，严格的兼容网关会整请求拒绝。
pub fn supports_reasoning_effort(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let o_series =
        model.len() > 1 && model.starts_with('o') && model.as_bytes()[1].is_ascii_digit();
    let gpt5 = model
        .strip_prefix("gpt-")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit() && c >= '5');
    let grok = model
        .strip_prefix("grok-4.")
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|minor| minor.parse::<u32>().ok())
        .is_some_and(|minor| minor >= 5);
    o_series || gpt5 || grok || model.starts_with("grok-build-")
}

/// 工具参数 schema 清洗：根缺 `type` 时补 object；递归删除 `format: uri`
pub fn clean_schema(schema: &Value) -> Value {
    let mut schema = match schema {
        Value::Object(_) => schema.clone(),
        _ => json!({}),
    };
    strip_uri_format(&mut schema);
    if let Some(object) = schema.as_object_mut() {
        if !object.contains_key("type") {
            object.insert("type".into(), json!("object"));
        }
        if object.get("type") == Some(&json!("object")) && !object.contains_key("properties") {
            object.insert("properties".into(), json!({}));
        }
    }
    schema
}

fn strip_uri_format(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.get("format") == Some(&json!("uri")) {
                object.remove("format");
            }
            for child in object.values_mut() {
                strip_uri_format(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_uri_format),
        _ => {}
    }
}

fn media_url(source: &MediaSource) -> String {
    match source {
        MediaSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        MediaSource::Url(url) => url.clone(),
    }
}

/// 请求生成参数
pub struct RenderOptions<'a> {
    pub upstream_model: &'a str,
    pub default_max_output_tokens: u64,
}

/// IR → OpenAI Chat 请求。目标协议无法表达时返回 `Err(原因)`（上游将被跳过）。
pub fn render_request(request: &Request, options: &RenderOptions<'_>) -> Result<Value, String> {
    let mut messages = Vec::new();

    let system = request
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
        .collect::<Vec<_>>()
        .join("\n");
    if !system.is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }

    for message in &request.messages {
        match message.role {
            Role::User => render_user(message, &mut messages)?,
            Role::Assistant => render_assistant(message, &mut messages)?,
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(options.upstream_model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert(
        "max_tokens".into(),
        json!(request
            .max_output_tokens
            .unwrap_or(options.default_max_output_tokens)),
    );
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if !request.stop.is_empty() {
        body.insert("stop".into(), json!(request.stop));
    }
    if let Some(user) = &request.user {
        body.insert("user".into(), json!(user));
    }
    if request.stream {
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if supports_reasoning_effort(options.upstream_model) {
        if let Some(effort) = request.reasoning.effort() {
            body.insert("reasoning_effort".into(), json!(effort.as_str()));
        }
    }

    render_tools(request, &mut body);
    Ok(Value::Object(body))
}

fn render_tools(request: &Request, body: &mut Map<String, Value>) {
    for server_tool in &request.server_tools {
        let kind = match server_tool.kind {
            ServerToolKind::WebSearch => "web_search",
            ServerToolKind::Other => "server tool",
        };
        tracing::warn!(tool = kind, "Chat 上游不支持内置工具，已从请求中剔除");
    }
    let tools: Vec<Value> = request.tools.iter().map(tool_json).collect();
    if tools.is_empty() {
        return;
    }
    let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    body.insert("tools".into(), Value::Array(tools));
    let choice = match &request.tool_choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        // 指向被剔除的内置工具时退回 auto
        ToolChoice::Named(name) if names.contains(&name.as_str()) => {
            json!({ "type": "function", "function": { "name": name } })
        }
        ToolChoice::Named(_) => json!("auto"),
    };
    body.insert("tool_choice".into(), choice);
    if let Some(parallel) = request.parallel_tool_calls {
        body.insert("parallel_tool_calls".into(), json!(parallel));
    }
}

fn tool_json(tool: &Tool) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), json!(tool.name));
    // description 缺失时省略而不是输出 null：严格上游会拒绝 null
    if let Some(description) = &tool.description {
        function.insert("description".into(), json!(description));
    }
    function.insert("parameters".into(), clean_schema(&tool.parameters));
    json!({ "type": "function", "function": function })
}

/// 工具结果 → `role: tool` 消息的文本；图片另行返回，放到随后的 user 消息
fn tool_result_text(content: &[Block]) -> Result<(String, Vec<Value>), String> {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in content {
        match block {
            Block::Text { text } => texts.push(text.clone()),
            Block::Image { source } => images.push(json!({
                "type": "image_url",
                "image_url": { "url": media_url(source) }
            })),
            Block::Document { .. } => {
                return Err("Chat upstreams cannot accept documents in tool results".into())
            }
            _ => {}
        }
    }
    let mut text = texts.join("\n");
    if !images.is_empty() && text.is_empty() {
        text = "[image]".into();
    }
    Ok((text, images))
}

fn render_user(message: &Message, messages: &mut Vec<Value>) -> Result<(), String> {
    // Chat 要求 tool 消息紧跟带 tool_calls 的 assistant 消息：先输出全部工具结果
    let mut moved_images = Vec::new();
    for block in &message.content {
        if let Block::ToolResult {
            call_id, content, ..
        } = block
        {
            let (text, images) = tool_result_text(content)?;
            messages.push(json!({ "role": "tool", "tool_call_id": call_id, "content": text }));
            moved_images.extend(images);
        }
    }

    let mut parts = moved_images;
    for block in &message.content {
        match block {
            Block::Text { text } if !text.is_empty() => {
                parts.push(json!({ "type": "text", "text": text }))
            }
            Block::Image { source } => parts.push(json!({
                "type": "image_url",
                "image_url": { "url": media_url(source) }
            })),
            Block::Document { .. } => {
                return Err("Chat upstreams cannot accept document blocks".into())
            }
            _ => {}
        }
    }
    if let Some(content) = content_value(parts) {
        messages.push(json!({ "role": "user", "content": content }));
    }
    Ok(())
}

fn render_assistant(message: &Message, messages: &mut Vec<Value>) -> Result<(), String> {
    let mut parts = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &message.content {
        match block {
            Block::Text { text } if !text.is_empty() => {
                parts.push(json!({ "type": "text", "text": text }))
            }
            Block::ToolCall {
                id,
                name,
                arguments,
                ..
            } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            })),
            // 推理与签名：Chat 请求无法表达，丢弃
            _ => {}
        }
    }
    if parts.is_empty() && tool_calls.is_empty() {
        return Ok(());
    }
    let mut value = json!({
        "role": "assistant",
        "content": content_value(parts).unwrap_or(Value::Null),
    });
    if !tool_calls.is_empty() {
        value["tool_calls"] = Value::Array(tool_calls);
    }
    messages.push(value);
    Ok(())
}

/// 单个文本 part 简化为字符串；空则为 None
fn content_value(parts: Vec<Value>) -> Option<Value> {
    match parts.len() {
        0 => None,
        1 if parts[0]["type"] == "text" => Some(parts[0]["text"].clone()),
        _ => Some(Value::Array(parts)),
    }
}

// ---------------------------------------------------------------------------
// 响应解析
// ---------------------------------------------------------------------------

/// Chat usage → IR（三桶互斥）
pub fn parse_usage(usage: &Value) -> Usage {
    let number = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64);
    let prompt = number("/prompt_tokens").unwrap_or(0);
    let cache_read = number("/cache_read_input_tokens")
        .or_else(|| number("/prompt_tokens_details/cached_tokens"))
        .unwrap_or(0);
    let cache_write = number("/cache_creation_input_tokens").unwrap_or(0);
    Usage {
        input_tokens: prompt.saturating_sub(cache_read + cache_write),
        output_tokens: number("/completion_tokens").unwrap_or(0),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: number("/completion_tokens_details/reasoning_tokens"),
    }
}

fn stop_reason(finish_reason: Option<&str>, has_tool_calls: bool) -> StopReason {
    match finish_reason {
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("stop") | None if has_tool_calls => StopReason::ToolUse,
        Some("stop") | None => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_string()),
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 非流式 Chat 响应 → IR。2xx 内带顶层 `error` 时返回 Err
pub fn parse_response(body: &Value) -> Result<Response, String> {
    if let Some(error) = body.get("error") {
        return Err(error_message(error));
    }
    let choice = body
        .pointer("/choices/0")
        .ok_or("chat response has no choices")?;
    let message = choice.get("message").ok_or("chat choice has no message")?;
    let mut content = Vec::new();
    let reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .map(text_of)
        .unwrap_or_default();
    if !reasoning.is_empty() {
        content.push(Block::Thinking {
            text: reasoning,
            signature: None,
        });
    }
    let text = message.get("content").map(text_of).unwrap_or_default();
    if !text.is_empty() {
        content.push(Block::Text { text });
    }
    if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
        if !refusal.is_empty() {
            content.push(Block::Text {
                text: refusal.to_string(),
            });
        }
    }
    let mut tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(function_call) = message.get("function_call") {
        tool_calls.push(json!({ "id": "call_0", "function": function_call }));
    }
    let has_tool_calls = !tool_calls.is_empty();
    for (i, call) in tool_calls.iter().enumerate() {
        content.push(Block::ToolCall {
            id: call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{i}")),
            name: call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string(),
            signature: None,
        });
    }
    Ok(Response {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("chatcmpl")
            .to_string(),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content,
        stop_reason: stop_reason(
            choice.get("finish_reason").and_then(Value::as_str),
            has_tool_calls,
        ),
        usage: body.get("usage").map(parse_usage).unwrap_or_default(),
    })
}

/// 从错误对象取出可读 message
pub fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

// ---------------------------------------------------------------------------
// 流式解码
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenKind {
    Text,
    Thinking,
    Tool(usize),
}

#[derive(Debug, Default)]
struct PendingTool {
    /// 已分配的 IR index（拿到 name 后才开块）
    ir_index: Option<usize>,
    id: Option<String>,
    name: Option<String>,
    buffered_arguments: String,
}

/// Chat SSE 数据帧 → 规范化事件
#[derive(Debug, Default)]
pub struct StreamDecoder {
    started: bool,
    finished: bool,
    next_index: usize,
    open: Option<(OpenKind, usize)>,
    tools: Vec<(usize, PendingTool)>,
    finish_reason: Option<String>,
    saw_tool_calls: bool,
    usage: Option<Usage>,
    id: String,
    model: String,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一个 SSE 事件的 data
    pub fn feed(&mut self, data: &str) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if data.trim() == "[DONE]" {
            self.finalize(&mut out);
            return out;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            tracing::debug!("忽略无法解析的 Chat 流数据帧");
            return out;
        };
        if let Some(error) = chunk.get("error") {
            out.push(Event::Error {
                kind: "api_error".into(),
                message: error_message(error),
            });
            self.finished = true;
            return out;
        }
        if !self.started {
            self.started = true;
            self.id = chunk
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("chatcmpl")
                .to_string();
            self.model = chunk
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            out.push(Event::Start {
                id: self.id.clone(),
                model: self.model.clone(),
                usage: Usage::default(),
            });
        }
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(parse_usage(usage));
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return out; // usage chunk：choices 为空
        };
        if let Some(delta) = choice.get("delta") {
            let reasoning = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !reasoning.is_empty() {
                let index = self.open_block(OpenKind::Thinking, &mut out);
                out.push(Event::ThinkingDelta {
                    index,
                    text: reasoning.to_string(),
                });
            }
            let text = delta.get("content").map(text_of).unwrap_or_default();
            if !text.is_empty() {
                let index = self.open_block(OpenKind::Text, &mut out);
                out.push(Event::TextDelta { index, text });
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.tool_delta(call, &mut out);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            // 不立即结束：usage chunk 通常在 finish_reason 之后，且可能重复发送
            self.finish_reason.get_or_insert_with(|| reason.to_string());
        }
        out
    }

    /// 上游流结束（无论有没有 `[DONE]`）
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if self.finish_reason.is_none() {
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

    fn close_open(&mut self, out: &mut Vec<Event>) {
        if let Some((_, index)) = self.open.take() {
            out.push(Event::BlockStop { index });
        }
    }

    /// 打开（或沿用）一个文本 / 推理块，返回其 index
    fn open_block(&mut self, kind: OpenKind, out: &mut Vec<Event>) -> usize {
        if let Some((open_kind, index)) = self.open {
            if open_kind == kind {
                return index;
            }
        }
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        out.push(Event::BlockStart {
            index,
            kind: match kind {
                OpenKind::Text => BlockKind::Text,
                _ => BlockKind::Thinking,
            },
        });
        self.open = Some((kind, index));
        index
    }

    fn tool_delta(&mut self, call: &Value, out: &mut Vec<Event>) {
        self.saw_tool_calls = true;
        let chat_index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let position = match self.tools.iter().position(|(i, _)| *i == chat_index) {
            Some(position) => position,
            None => {
                self.tools.push((chat_index, PendingTool::default()));
                self.tools.len() - 1
            }
        };
        {
            let pending = &mut self.tools[position].1;
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                pending.id.get_or_insert_with(|| id.to_string());
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                if !name.is_empty() {
                    pending.name.get_or_insert_with(|| name.to_string());
                }
            }
        }
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // 已开块：参数直接输出（若它不是当前块，先切换）
        if let Some(index) = self.tools[position].1.ir_index {
            if self.open != Some((OpenKind::Tool(chat_index), index)) {
                self.close_open(out);
                // 回到已关闭的工具块不合法：按新参数继续追加到该块已无法表达，记录并忽略
                tracing::warn!("Chat 流中工具调用参数交错出现，已忽略乱序片段");
                return;
            }
            if !arguments.is_empty() {
                out.push(Event::ToolArgumentsDelta {
                    index,
                    partial_json: arguments,
                });
            }
            return;
        }

        // 未开块：参数可能先于 id / name 到达，先缓冲。另一个工具块正在输出时也只缓冲：
        // 兼容网关可能交错发送多个工具的参数，而已关闭的块无法再追加，这些工具在
        // 流结束时按顺序整体输出
        self.tools[position]
            .1
            .buffered_arguments
            .push_str(&arguments);
        let tool_open = matches!(self.open, Some((OpenKind::Tool(_), _)));
        if self.tools[position].1.name.is_some() && !tool_open {
            self.start_tool(position, out);
        }
    }

    fn start_tool(&mut self, position: usize, out: &mut Vec<Event>) {
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        let chat_index = self.tools[position].0;
        let pending = &mut self.tools[position].1;
        pending.ir_index = Some(index);
        let id = pending
            .id
            .clone()
            .unwrap_or_else(|| format!("call_{chat_index}"));
        let name = pending.name.clone().unwrap_or_default();
        out.push(Event::BlockStart {
            index,
            kind: BlockKind::ToolCall { id, name },
        });
        let buffered = std::mem::take(&mut pending.buffered_arguments);
        if !buffered.is_empty() {
            out.push(Event::ToolArgumentsDelta {
                index,
                partial_json: buffered,
            });
        }
        self.open = Some((OpenKind::Tool(chat_index), index));
    }

    fn finalize(&mut self, out: &mut Vec<Event>) {
        if self.finished {
            return;
        }
        if !self.started {
            self.started = true;
            out.push(Event::Start {
                id: "chatcmpl".into(),
                model: String::new(),
                usage: Usage::default(),
            });
        }
        // 一直没拿到 name 的工具调用：仍然输出，避免丢失参数
        for position in 0..self.tools.len() {
            if self.tools[position].1.ir_index.is_none() {
                self.start_tool(position, out);
            }
        }
        self.close_open(out);
        self.finished = true;
        out.push(Event::Finish {
            stop_reason: stop_reason(self.finish_reason.as_deref(), self.saw_tool_calls),
            usage: self.usage.unwrap_or_default(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Effort, Reasoning, ServerTool};

    fn opts(model: &str) -> RenderOptions<'_> {
        RenderOptions {
            upstream_model: model,
            default_max_output_tokens: 16384,
        }
    }

    #[test]
    fn billing_header_is_stripped_only_at_the_start() {
        assert_eq!(
            strip_leading_billing_header("x-anthropic-billing-header: cch=1\n\nYou are"),
            "You are"
        );
        assert_eq!(
            strip_leading_billing_header("x-anthropic-billing-header: x"),
            ""
        );
        assert_eq!(
            strip_leading_billing_header("Hi\nx-anthropic-billing-header: x"),
            "Hi\nx-anthropic-billing-header: x"
        );
    }

    #[test]
    fn reasoning_effort_gate() {
        for yes in [
            "o3",
            "o4-mini",
            "gpt-5",
            "gpt-5.1-codex",
            "GPT-6-sol",
            "grok-4.5",
            "grok-4.10",
            "grok-build-1",
        ] {
            assert!(supports_reasoning_effort(yes), "{yes}");
        }
        for no in [
            "deepseek-chat",
            "gpt-4o",
            "grok-4.1",
            "kimi-k2",
            "o",
            "qwen3",
        ] {
            assert!(!supports_reasoning_effort(no), "{no}");
        }
    }

    #[test]
    fn schema_cleaning() {
        assert_eq!(
            clean_schema(&json!(null)),
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            clean_schema(&json!({"properties": {"u": {"type": "string", "format": "uri"}}})),
            json!({"properties": {"u": {"type": "string"}}, "type": "object"})
        );
        assert_eq!(
            clean_schema(
                &json!({"type": "object", "properties": {"d": {"type": "string", "format": "date"}}})
            ),
            json!({"type": "object", "properties": {"d": {"type": "string", "format": "date"}}})
        );
    }

    fn sample_request() -> Request {
        Request {
            model: "claude-sonnet-5".into(),
            system: vec![
                "x-anthropic-billing-header: cch=1\n\nYou are Claude Code.".into(),
                "Rules".into(),
            ],
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Block::Text {
                        text: "list".into(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Block::Thinking {
                            text: "use ls".into(),
                            signature: None,
                        },
                        Block::Text {
                            text: "Running.".into(),
                        },
                        Block::ToolCall {
                            id: "toolu_1".into(),
                            name: "Bash".into(),
                            arguments: r#"{"command":"ls"}"#.into(),
                            signature: None,
                        },
                        Block::ToolCall {
                            id: "toolu_2".into(),
                            name: "Read".into(),
                            arguments: r#"{"p":"a"}"#.into(),
                            signature: None,
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        Block::Text {
                            text: "and?".into(),
                        },
                        Block::ToolResult {
                            call_id: "toolu_1".into(),
                            content: vec![Block::Text {
                                text: "a.txt".into(),
                            }],
                            is_error: false,
                        },
                        Block::ToolResult {
                            call_id: "toolu_2".into(),
                            content: vec![Block::Image {
                                source: MediaSource::Base64 {
                                    media_type: "image/png".into(),
                                    data: "iVBOR".into(),
                                },
                            }],
                            is_error: false,
                        },
                    ],
                },
            ],
            tools: vec![Tool {
                name: "Bash".into(),
                description: None,
                parameters: json!({"properties": {}}),
                custom: false,
            }],
            tool_choice: ToolChoice::Named("web_search".into()),
            parallel_tool_calls: Some(false),
            stream: true,
            reasoning: Reasoning::Adaptive(Some(Effort::High)),
            server_tools: vec![ServerTool {
                kind: ServerToolKind::WebSearch,
                raw: json!({}),
            }],
            top_k: Some(5),
            ..Default::default()
        }
    }

    #[test]
    fn renders_a_chat_request() {
        let body = render_request(&sample_request(), &opts("deepseek-chat")).unwrap();
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["max_tokens"], 16384);
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert!(
            body.get("reasoning_effort").is_none(),
            "deepseek-chat does not take reasoning_effort"
        );
        assert!(body.get("top_k").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "You are Claude Code.\nRules"})
        );
        assert_eq!(messages[1], json!({"role": "user", "content": "list"}));
        // assistant：推理丢弃，文本 + tool_calls
        assert_eq!(messages[2]["content"], "Running.");
        assert_eq!(
            messages[2]["tool_calls"][1]["function"]["arguments"],
            r#"{"p":"a"}"#
        );
        // 工具结果在前，图片移到随后的 user 消息
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "toolu_1", "content": "a.txt"})
        );
        assert_eq!(
            messages[4],
            json!({"role": "tool", "tool_call_id": "toolu_2", "content": "[image]"})
        );
        assert_eq!(messages[5]["role"], "user");
        assert_eq!(messages[5]["content"][0]["type"], "image_url");
        assert_eq!(
            messages[5]["content"][0]["image_url"]["url"],
            "data:image/png;base64,iVBOR"
        );
        assert_eq!(
            messages[5]["content"][1],
            json!({"type": "text", "text": "and?"})
        );
        // 工具：description 省略、schema 补 type；tool_choice 指向被剔除的 web_search → auto
        assert_eq!(
            body["tools"][0],
            json!({"type": "function", "function": {"name": "Bash", "parameters": {"properties": {}, "type": "object"}}})
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn reasoning_effort_for_supported_models() {
        let body = render_request(&sample_request(), &opts("gpt-5.1")).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn no_tools_means_no_tool_choice_and_documents_are_unsupported() {
        let mut request = sample_request();
        request.tools.clear();
        let body = render_request(&request, &opts("m")).unwrap();
        assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());

        request.messages.push(Message {
            role: Role::User,
            content: vec![Block::Document {
                source: MediaSource::Url("https://x/a.pdf".into()),
                name: None,
            }],
        });
        assert!(render_request(&request, &opts("m")).is_err());
    }

    #[test]
    fn parses_a_chat_response() {
        let body = json!({
            "id": "chatcmpl-1", "model": "deepseek-chat",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant", "content": "hi", "reasoning_content": "think",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "Bash", "arguments": "{\"a\":1}"}}]
            }}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 30}}
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(
            response.stop_reason,
            StopReason::ToolUse,
            "stop + tool_calls counts as tool use"
        );
        assert_eq!(response.content.len(), 3);
        assert!(matches!(&response.content[0], Block::Thinking { text, .. } if text == "think"));
        assert_eq!(response.usage.input_tokens, 70);
        assert_eq!(response.usage.cache_read_tokens, 30);
        assert_eq!(response.usage.output_tokens, 20);
        assert!(parse_response(&json!({"error": {"message": "boom"}})).is_err());
    }

    fn decode(chunks: &[&str]) -> Vec<Event> {
        let mut decoder = StreamDecoder::new();
        let mut events: Vec<Event> = chunks.iter().flat_map(|c| decoder.feed(c)).collect();
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn stream_text_reasoning_tools_and_late_usage() {
        let events = decode(&[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":""}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":"think"}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            // 参数先于 id / name 到达
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\""}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":":1}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"Read","arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            "[DONE]",
        ]);
        assert_eq!(
            events,
            vec![
                Event::Start {
                    id: "c1".into(),
                    model: "m".into(),
                    usage: Usage::default()
                },
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::Thinking
                },
                Event::ThinkingDelta {
                    index: 0,
                    text: "think".into()
                },
                Event::BlockStop { index: 0 },
                Event::BlockStart {
                    index: 1,
                    kind: BlockKind::Text
                },
                Event::TextDelta {
                    index: 1,
                    text: "Hi".into()
                },
                Event::BlockStop { index: 1 },
                Event::BlockStart {
                    index: 2,
                    kind: BlockKind::ToolCall {
                        id: "call_1".into(),
                        name: "Bash".into()
                    }
                },
                // 开块前缓冲的参数与开块时这一段合并输出
                Event::ToolArgumentsDelta {
                    index: 2,
                    partial_json: "{\"a\":1}".into()
                },
                Event::BlockStop { index: 2 },
                Event::BlockStart {
                    index: 3,
                    kind: BlockKind::ToolCall {
                        id: "call_2".into(),
                        name: "Read".into()
                    }
                },
                Event::ToolArgumentsDelta {
                    index: 3,
                    partial_json: "{}".into()
                },
                Event::BlockStop { index: 3 },
                Event::Finish {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    }
                },
            ]
        );
    }

    #[test]
    fn interleaved_tool_arguments_are_not_lost() {
        let events = decode(&[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"A","arguments":"{\"x"}},{"index":1,"id":"b","function":{"name":"B","arguments":"{\"y"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":1}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\":2}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        // 按块重建：每个块 start/stop 成对、参数完整
        let mut blocks: Vec<(String, String, bool)> = Vec::new();
        for event in &events {
            match event {
                Event::BlockStart {
                    index,
                    kind: BlockKind::ToolCall { name, .. },
                } => {
                    assert_eq!(*index, blocks.len());
                    blocks.push((name.clone(), String::new(), false));
                }
                Event::ToolArgumentsDelta {
                    index,
                    partial_json,
                } => {
                    assert!(!blocks[*index].2, "delta after stop");
                    blocks[*index].1.push_str(partial_json);
                }
                Event::BlockStop { index } => blocks[*index].2 = true,
                _ => {}
            }
        }
        assert_eq!(
            blocks,
            vec![
                ("A".into(), "{\"x\":1}".into(), true),
                ("B".into(), "{\"y\":2}".into(), true),
            ]
        );
    }

    #[test]
    fn stream_without_done_still_finishes_and_truncation_is_an_error() {
        let events = decode(&[
            r#"{"id":"c","choices":[{"index":0,"delta":{"content":"a"}}]}"#,
            r#"{"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
        ]);
        assert!(matches!(
            events.last(),
            Some(Event::Finish {
                stop_reason: StopReason::MaxTokens,
                ..
            })
        ));

        let events = decode(&[r#"{"id":"c","choices":[{"index":0,"delta":{"content":"a"}}]}"#]);
        assert!(matches!(events.last(), Some(Event::Error { .. })));

        let events = decode(&[r#"{"error":{"message":"rate limited"}}"#]);
        assert_eq!(
            events,
            vec![Event::Error {
                kind: "api_error".into(),
                message: "rate limited".into()
            }]
        );
    }
}
