//! OpenAI Responses 协议的上游侧（设计文档 §5、§6.2）：中转把 IR 请求发给 Responses 上游，
//! 并把其非流式响应与 SSE 事件流解析回 IR。
//!
//! Responses 作为客户端协议的编解码在其他模块实现。

use cc_proxy_core::Interface;
use serde_json::{json, Map, Value};

use crate::chat::{clean_schema, strip_leading_billing_header, RenderOptions};
use crate::ir::{
    Block, BlockKind, Event, MediaSource, Message, Reasoning, Request, Response, Role,
    ServerToolKind, Signature, StopReason, Tool, ToolChoice, Usage,
};

/// Responses 上游拒绝小于 16 的 `max_output_tokens`
const MIN_MAX_OUTPUT_TOKENS: u64 = 16;

/// 让上游返回 `encrypted_content`，否则多轮推理无法回放（§5.5）
const ENCRYPTED_REASONING_INCLUDE: &str = "reasoning.encrypted_content";

// ---------------------------------------------------------------------------
// 请求生成
// ---------------------------------------------------------------------------

fn media_url(source: &MediaSource) -> String {
    match source {
        MediaSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        MediaSource::Url(url) => url.clone(),
    }
}

fn input_image(source: &MediaSource) -> Value {
    json!({ "type": "input_image", "image_url": media_url(source) })
}

/// IR → Responses 请求。目标协议无法表达时返回 `Err(原因)`（调用方映射为 Unsupported）
pub fn render_request(request: &Request, options: &RenderOptions<'_>) -> Result<Value, String> {
    let mut input = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::User => render_user(message, &mut input)?,
            Role::Assistant => render_assistant(message, &mut input),
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(options.upstream_model));

    let instructions = request
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
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions));
    }
    body.insert("input".into(), Value::Array(input));

    // 客户端的探测请求可能只要 1 个 token，钳到上游允许的最小值而不是整请求失败
    let max_output_tokens = request
        .max_output_tokens
        .unwrap_or(options.default_max_output_tokens);
    let max_output_tokens = if (1..MIN_MAX_OUTPUT_TOKENS).contains(&max_output_tokens) {
        MIN_MAX_OUTPUT_TOKENS
    } else {
        max_output_tokens
    };
    body.insert("max_output_tokens".into(), json!(max_output_tokens));

    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if request.top_k.is_some() {
        tracing::debug!("Responses 上游不支持 top_k，已丢弃");
    }
    if !request.stop.is_empty() {
        tracing::debug!("Responses 上游不支持 stop，已丢弃");
    }
    if let Some(user) = &request.user {
        body.insert("user".into(), json!(user));
    }
    // Disabled 对应的 `effort: none` 只有部分模型支持，统一不写，由上游决定
    if !matches!(request.reasoning, Reasoning::Disabled) {
        if let Some(effort) = request.reasoning.effort() {
            body.insert(
                "reasoning".into(),
                json!({ "effort": effort.as_str(), "summary": "auto" }),
            );
        }
    }
    body.insert("stream".into(), json!(request.stream));
    // 无状态化：不在上游保存响应，并要求返回可回放的加密推理
    body.insert("store".into(), json!(false));
    body.insert("include".into(), json!([ENCRYPTED_REASONING_INCLUDE]));

    render_tools(request, &mut body);
    Ok(Value::Object(body))
}

fn render_tools(request: &Request, body: &mut Map<String, Value>) {
    let mut tools: Vec<Value> = request.tools.iter().map(tool_json).collect();
    let mut web_search = false;
    for server_tool in &request.server_tools {
        match server_tool.kind {
            ServerToolKind::WebSearch => {
                if web_search {
                    continue;
                }
                web_search = true;
                tools.push(json!({ "type": "web_search" }));
                if let Some(max_uses) = server_tool.raw.get("max_uses").and_then(Value::as_u64) {
                    body.insert("max_tool_calls".into(), json!(max_uses));
                }
            }
            ServerToolKind::Other => {
                tracing::warn!("Responses 上游不支持该内置工具，已从请求中剔除");
            }
        }
    }
    if tools.is_empty() {
        return;
    }
    let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    body.insert("tools".into(), Value::Array(tools));
    let choice = match &request.tool_choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) if names.contains(&name.as_str()) => {
            json!({ "type": "function", "name": name })
        }
        // 指向不存在或被剔除的工具时退回 auto
        ToolChoice::Named(_) => json!("auto"),
    };
    body.insert("tool_choice".into(), choice);
    if let Some(parallel) = request.parallel_tool_calls {
        body.insert("parallel_tool_calls".into(), json!(parallel));
    }
}

/// custom 工具在 IR 中已降级为参数 `{input: string}` 的普通工具，统一按 function 声明
fn tool_json(tool: &Tool) -> Value {
    let mut function = Map::new();
    function.insert("type".into(), json!("function"));
    function.insert("name".into(), json!(tool.name));
    // description 缺失时省略而不是输出 null：严格上游会拒绝 null
    if let Some(description) = &tool.description {
        function.insert("description".into(), json!(description));
    }
    function.insert("parameters".into(), clean_schema(&tool.parameters));
    function.insert("strict".into(), json!(false));
    Value::Object(function)
}

/// 工具结果 → `function_call_output.output`：纯文本为字符串，含图片时为 parts 数组
fn tool_output(content: &[Block]) -> Result<Value, String> {
    let mut parts = Vec::new();
    let mut texts = Vec::new();
    let mut has_image = false;
    for block in content {
        match block {
            Block::Text { text } => {
                texts.push(text.as_str());
                parts.push(json!({ "type": "input_text", "text": text }));
            }
            Block::Image { source } => {
                has_image = true;
                parts.push(input_image(source));
            }
            Block::Document { .. } => {
                return Err("Responses upstreams cannot accept documents in tool results".into())
            }
            _ => {}
        }
    }
    if has_image {
        Ok(Value::Array(parts))
    } else {
        Ok(json!(texts.join("\n")))
    }
}

fn render_user(message: &Message, input: &mut Vec<Value>) -> Result<(), String> {
    // 先输出全部工具结果，再输出 user 的其余内容（§5.3）
    for block in &message.content {
        if let Block::ToolResult {
            call_id, content, ..
        } = block
        {
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": tool_output(content)?,
            }));
        }
    }
    let mut parts = Vec::new();
    for block in &message.content {
        match block {
            Block::Text { text } if !text.is_empty() => {
                parts.push(json!({ "type": "input_text", "text": text }))
            }
            Block::Image { source } => parts.push(input_image(source)),
            Block::Document { .. } => {
                return Err("Responses upstreams cannot accept document blocks".into())
            }
            _ => {}
        }
    }
    if !parts.is_empty() {
        input.push(json!({ "type": "message", "role": "user", "content": parts }));
    }
    Ok(())
}

/// 同源签名还原为原 reasoning 项（去掉 `status`，上游不接受输入项带它）；其他来源丢弃
fn replay_reasoning(signature: &Signature) -> Option<Value> {
    if signature.source != Interface::OpenaiResponses {
        tracing::debug!(
            source = signature.source.as_str(),
            "跨协议的推理签名无法回放给 Responses 上游，已丢弃"
        );
        return None;
    }
    let mut item = signature.value.clone();
    let object = item.as_object_mut()?;
    if object.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    object.remove("status");
    Some(item)
}

fn flush_text(parts: &mut Vec<Value>, input: &mut Vec<Value>) {
    if !parts.is_empty() {
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": std::mem::take(parts),
        }));
    }
}

fn render_assistant(message: &Message, input: &mut Vec<Value>) {
    let start = input.len();
    // 相邻文本合并为一个 message
    let mut texts = Vec::new();
    for block in &message.content {
        match block {
            Block::Text { text } if !text.is_empty() => {
                texts.push(json!({ "type": "output_text", "text": text }))
            }
            Block::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                flush_text(&mut texts, input);
                let arguments = if arguments.trim().is_empty() {
                    "{}"
                } else {
                    arguments.as_str()
                };
                input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            Block::Thinking {
                signature: Some(signature),
                ..
            }
            | Block::RedactedThinking { signature } => {
                if let Some(item) = replay_reasoning(signature) {
                    flush_text(&mut texts, input);
                    input.push(item);
                }
            }
            // 无签名的推理文本无法回放
            _ => {}
        }
    }
    flush_text(&mut texts, input);

    // reasoning 项后面必须跟同一轮生成的 message 或 function_call，否则上游拒绝整个请求
    let mut has_follower = false;
    for index in (start..input.len()).rev() {
        if input[index].get("type").and_then(Value::as_str) == Some("reasoning") {
            if !has_follower {
                tracing::debug!("reasoning 项之后没有 message / function_call，已删除");
                input.remove(index);
            }
        } else {
            has_follower = true;
        }
    }
}

// ---------------------------------------------------------------------------
// 响应解析
// ---------------------------------------------------------------------------

/// Responses usage → IR（三桶互斥）
fn parse_usage(usage: &Value) -> Usage {
    let number = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64);
    let input = number("/input_tokens").unwrap_or(0);
    let cached = number("/input_tokens_details/cached_tokens").unwrap_or(0);
    Usage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: number("/output_tokens").unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        reasoning_tokens: number("/output_tokens_details/reasoning_tokens"),
    }
}

fn stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
    has_tool: bool,
) -> StopReason {
    match status {
        Some("incomplete") => match incomplete_reason {
            None | Some("max_output_tokens") | Some("max_tokens") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            Some(other) => StopReason::Other(other.to_string()),
        },
        _ if has_tool => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    }
}

fn response_stop_reason(response: &Value, has_tool: bool) -> StopReason {
    stop_reason(
        response.get("status").and_then(Value::as_str),
        response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str),
        has_tool,
    )
}

/// 从 Responses 错误 body 取 message（`error.message`、顶层 `message` 或字符串 `error`）
pub fn error_message(body: &Value) -> Option<String> {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| body.get("message").and_then(Value::as_str))
        .or_else(|| body.get("error").and_then(Value::as_str))
        .filter(|message| !message.trim().is_empty())
        .map(str::to_string)
}

/// 2xx 内的失败：`status` 为 failed / cancelled 或带非空 `error`
fn terminal_error(response: &Value) -> Option<String> {
    let status = response.get("status").and_then(Value::as_str);
    let has_error = response.get("error").is_some_and(|error| !error.is_null());
    let fallback = match status {
        Some("failed") => "response generation failed",
        Some("cancelled") => "response generation was cancelled",
        _ if has_error => "upstream returned an error",
        _ => return None,
    };
    Some(error_message(response).unwrap_or_else(|| fallback.to_string()))
}

/// message 项中 output_text / refusal 的文本
fn message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("output_text") => part.get("text").and_then(Value::as_str),
            Some("refusal") => part.get("refusal").and_then(Value::as_str),
            _ => None,
        })
        .collect()
}

/// reasoning 项的可见文本：summary 各段以空行分隔；没有 summary 时取原文推理
fn reasoning_text(item: &Value) -> String {
    let collect = |field: &str| {
        item.get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let summary = collect("summary");
    if summary.is_empty() {
        collect("content")
    } else {
        summary
    }
}

fn has_encrypted_content(item: &Value) -> bool {
    item.get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
}

fn reasoning_signature(item: &Value) -> Signature {
    Signature {
        source: Interface::OpenaiResponses,
        value: item.clone(),
    }
}

fn custom_arguments(input: &str) -> String {
    json!({ "input": input }).to_string()
}

fn is_json_object(text: &str) -> bool {
    serde_json::from_str::<Value>(text).is_ok_and(|value| value.is_object())
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 工具调用 id：优先 `call_id`，缺失时退回项 `id`
fn call_id(item: &Value) -> String {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Responses 非流式响应 → IR；`status` 为 failed / cancelled 或带 `error` 对象时返回 Err
pub fn parse_response(body: &Value) -> Result<Response, String> {
    if let Some(message) = terminal_error(body) {
        return Err(message);
    }
    let completed = matches!(
        body.get("status").and_then(Value::as_str),
        None | Some("completed")
    );
    let mut content = Vec::new();
    let mut has_tool = false;
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let text = message_text(item);
                if !text.is_empty() {
                    content.push(Block::Text { text });
                }
            }
            Some("reasoning") => {
                let text = reasoning_text(item);
                let signature = has_encrypted_content(item).then(|| reasoning_signature(item));
                match (text.is_empty(), signature) {
                    (false, signature) => content.push(Block::Thinking { text, signature }),
                    (true, Some(signature)) => content.push(Block::RedactedThinking { signature }),
                    (true, None) => {}
                }
            }
            Some("function_call") => {
                has_tool = true;
                let name = string_field(item, "name");
                let mut arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or("{}")
                    .to_string();
                // 被截断的响应里参数可能不完整：替换为空对象，避免客户端拿到非法 JSON
                if !completed && !is_json_object(&arguments) {
                    tracing::warn!(tool = %name, "未完成响应中的工具调用参数不是合法 JSON 对象，已替换为空对象");
                    arguments = "{}".into();
                }
                content.push(Block::ToolCall {
                    id: call_id(item),
                    name,
                    arguments,
                    signature: None,
                });
            }
            Some("custom_tool_call") => {
                has_tool = true;
                content.push(Block::ToolCall {
                    id: call_id(item),
                    name: string_field(item, "name"),
                    arguments: custom_arguments(
                        item.get("input")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    signature: None,
                });
            }
            // web_search_call 等内置工具项暂不映射
            _ => {}
        }
    }
    Ok(Response {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("resp")
            .to_string(),
        model: string_field(body, "model"),
        content,
        stop_reason: response_stop_reason(body, has_tool),
        usage: body.get("usage").map(parse_usage).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// 流式解码
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Message,
    Reasoning,
    FunctionCall,
    CustomToolCall,
    /// web_search_call 等暂不映射的项
    Ignored,
}

impl ItemKind {
    fn of(item_type: Option<&str>) -> Self {
        match item_type {
            Some("message") => ItemKind::Message,
            Some("reasoning") => ItemKind::Reasoning,
            Some("function_call") => ItemKind::FunctionCall,
            Some("custom_tool_call") => ItemKind::CustomToolCall,
            _ => ItemKind::Ignored,
        }
    }
}

/// 一个上游 output item 的解码状态
#[derive(Debug)]
struct ItemState {
    output_index: Option<u64>,
    item_id: Option<String>,
    kind: ItemKind,
    /// 开块后分配的 IR index（延迟开块：没有内容时不分配）
    ir_index: Option<usize>,
    call_id: String,
    name: String,
    /// function_call：累积的参数 delta；custom_tool_call：累积的 input
    arguments: String,
    /// function_call 已输出的参数
    emitted_arguments: String,
    /// 已输出过文本 / 推理文本
    emitted_text: bool,
    /// 最近一个推理片段（种类, 序号），用于在多个 summary part 之间补空行
    last_part: Option<(u8, u64)>,
}

impl ItemState {
    fn new(output_index: Option<u64>, item_id: Option<String>, kind: ItemKind) -> Self {
        Self {
            output_index,
            item_id,
            kind,
            ir_index: None,
            call_id: String::new(),
            name: String::new(),
            arguments: String::new(),
            emitted_arguments: String::new(),
            emitted_text: false,
            last_part: None,
        }
    }
}

/// Responses SSE 事件 → 规范化事件
#[derive(Debug, Default)]
pub struct StreamDecoder {
    started: bool,
    finished: bool,
    next_index: usize,
    /// 当前打开的块所属的项（`items` 中的位置）
    open: Option<usize>,
    items: Vec<ItemState>,
    saw_tool_call: bool,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一个 SSE 事件；类型取 data 的 `type`，缺失时用 SSE `event` 名
    pub fn feed(&mut self, event: Option<&str>, data: &str) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let Ok(data) = serde_json::from_str::<Value>(data) else {
            tracing::debug!("忽略无法解析的 Responses 流数据帧");
            return out;
        };
        let kind = data
            .get("type")
            .and_then(Value::as_str)
            .or(event)
            .unwrap_or_default()
            .to_string();
        match kind.as_str() {
            "response.created" | "response.in_progress" => {
                self.ensure_started(data.get("response"), &mut out)
            }
            "response.output_item.added" => self.item_added(&data, &mut out),
            "response.output_text.delta" | "response.refusal.delta" => {
                self.text_delta(&data, &mut out)
            }
            "response.reasoning_summary_text.delta" => {
                self.reasoning_delta(&data, (0, "summary_index"), &mut out)
            }
            "response.reasoning_text.delta" => {
                self.reasoning_delta(&data, (1, "content_index"), &mut out)
            }
            "response.function_call_arguments.delta" => self.arguments_delta(&data, &mut out),
            "response.custom_tool_call_input.delta" => {
                self.ensure_started(None, &mut out);
                let position = self.locate(&data, None, ItemKind::CustomToolCall);
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    self.items[position].arguments.push_str(delta);
                }
            }
            "response.output_item.done" => self.item_done(&data, &mut out),
            "response.completed" | "response.incomplete" => {
                let response = data.get("response").cloned().unwrap_or(Value::Null);
                self.finalize(&kind, &response, &mut out);
            }
            "response.failed" | "response.cancelled" => {
                let response = data.get("response").cloned().unwrap_or(Value::Null);
                let message = error_message(&response)
                    .or_else(|| error_message(&data))
                    .unwrap_or_else(|| "upstream response failed".into());
                self.fail(message, &mut out);
            }
            "error" => {
                let message =
                    error_message(&data).unwrap_or_else(|| "upstream stream error".into());
                self.fail(message, &mut out);
            }
            // content_part.*、*.done、reasoning_summary_part.* 等只用于校验
            _ => {}
        }
        out
    }

    /// 上游流结束：没收到 completed / incomplete 时产生 Error（已结束则为空）
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if !self.finished {
            self.fail(
                "upstream stream ended before the response completed".into(),
                &mut out,
            );
        }
        out
    }

    fn fail(&mut self, message: String, out: &mut Vec<Event>) {
        self.finished = true;
        out.push(Event::Error {
            kind: "api_error".into(),
            message,
        });
    }

    fn ensure_started(&mut self, response: Option<&Value>, out: &mut Vec<Event>) {
        if self.started {
            return;
        }
        self.started = true;
        let response = response.unwrap_or(&Value::Null);
        out.push(Event::Start {
            id: response
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("resp")
                .to_string(),
            model: string_field(response, "model"),
            usage: Usage::default(),
        });
    }

    /// 找到事件所属的项（按 output_index，其次按 item_id）；找不到时新建
    fn locate(&mut self, data: &Value, item: Option<&Value>, kind: ItemKind) -> usize {
        let output_index = data.get("output_index").and_then(Value::as_u64);
        let item_id = data
            .get("item_id")
            .or_else(|| item.and_then(|item| item.get("id")))
            .and_then(Value::as_str)
            .map(str::to_string);
        let found = self.items.iter().position(|state| {
            (output_index.is_some() && state.output_index == output_index)
                || (output_index.is_none() && item_id.is_some() && state.item_id == item_id)
        });
        if let Some(position) = found {
            let state = &mut self.items[position];
            if state.item_id.is_none() {
                state.item_id = item_id;
            }
            return position;
        }
        self.items.push(ItemState::new(output_index, item_id, kind));
        self.items.len() - 1
    }

    /// 为项打开块（已打开则沿用）。项的块已被其他块挤掉时返回 None
    fn ensure_open(
        &mut self,
        position: usize,
        kind: BlockKind,
        out: &mut Vec<Event>,
    ) -> Option<usize> {
        if let Some(index) = self.items[position].ir_index {
            if self.open == Some(position) {
                return Some(index);
            }
            tracing::warn!("Responses 流中输出项交错出现，已忽略已关闭项的后续片段");
            return None;
        }
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        self.items[position].ir_index = Some(index);
        self.open = Some(position);
        out.push(Event::BlockStart { index, kind });
        Some(index)
    }

    fn close_open(&mut self, out: &mut Vec<Event>) {
        if let Some(position) = self.open.take() {
            if let Some(index) = self.items[position].ir_index {
                out.push(Event::BlockStop { index });
            }
        }
    }

    fn close_item(&mut self, position: usize, out: &mut Vec<Event>) {
        if self.open == Some(position) {
            self.close_open(out);
        }
    }

    fn tool_kind(&self, position: usize) -> BlockKind {
        let state = &self.items[position];
        BlockKind::ToolCall {
            id: state.call_id.clone(),
            name: state.name.clone(),
        }
    }

    fn item_added(&mut self, data: &Value, out: &mut Vec<Event>) {
        self.ensure_started(None, out);
        let item = data.get("item").unwrap_or(&Value::Null);
        let kind = ItemKind::of(item.get("type").and_then(Value::as_str));
        let position = self.locate(data, Some(item), kind);
        self.items[position].kind = kind;
        if matches!(kind, ItemKind::FunctionCall | ItemKind::CustomToolCall) {
            self.saw_tool_call = true;
            let state = &mut self.items[position];
            state.call_id = call_id(item);
            state.name = string_field(item, "name");
            let block = self.tool_kind(position);
            self.ensure_open(position, block, out);
        }
        // message 等首个文本 delta 再开块；reasoning 延迟到有文本或 done 时再决定块类型
    }

    fn text_delta(&mut self, data: &Value, out: &mut Vec<Event>) {
        self.ensure_started(None, out);
        let delta = data
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return;
        }
        let position = self.locate(data, None, ItemKind::Message);
        if let Some(index) = self.ensure_open(position, BlockKind::Text, out) {
            self.items[position].emitted_text = true;
            out.push(Event::TextDelta {
                index,
                text: delta.to_string(),
            });
        }
    }

    fn reasoning_delta(&mut self, data: &Value, part: (u8, &str), out: &mut Vec<Event>) {
        self.ensure_started(None, out);
        let delta = data
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return;
        }
        let position = self.locate(data, None, ItemKind::Reasoning);
        let key = (
            part.0,
            data.get(part.1).and_then(Value::as_u64).unwrap_or(0),
        );
        let Some(index) = self.ensure_open(position, BlockKind::Thinking, out) else {
            return;
        };
        let state = &mut self.items[position];
        let mut text = String::new();
        if state.emitted_text && state.last_part != Some(key) {
            text.push_str("\n\n");
        }
        text.push_str(delta);
        state.last_part = Some(key);
        state.emitted_text = true;
        out.push(Event::ThinkingDelta { index, text });
    }

    fn arguments_delta(&mut self, data: &Value, out: &mut Vec<Event>) {
        self.ensure_started(None, out);
        let delta = data
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let position = self.locate(data, None, ItemKind::FunctionCall);
        self.items[position].arguments.push_str(delta);
        // 没收到 output_item.added（不知道名字）时先缓冲，到 done 再输出
        if delta.is_empty() || self.open != Some(position) {
            return;
        }
        if let Some(index) = self.items[position].ir_index {
            self.items[position].emitted_arguments.push_str(delta);
            out.push(Event::ToolArgumentsDelta {
                index,
                partial_json: delta.to_string(),
            });
        }
    }

    fn item_done(&mut self, data: &Value, out: &mut Vec<Event>) {
        self.ensure_started(None, out);
        let item = data.get("item").unwrap_or(&Value::Null);
        let kind = ItemKind::of(item.get("type").and_then(Value::as_str));
        let position = self.locate(data, Some(item), kind);
        self.items[position].kind = kind;
        match kind {
            ItemKind::Message => {
                // 上游没发 delta、只在 done 中给出全文
                if !self.items[position].emitted_text {
                    let text = message_text(item);
                    if !text.is_empty() {
                        if let Some(index) = self.ensure_open(position, BlockKind::Text, out) {
                            self.items[position].emitted_text = true;
                            out.push(Event::TextDelta { index, text });
                        }
                    }
                }
            }
            ItemKind::Reasoning => {
                if !self.items[position].emitted_text {
                    let text = reasoning_text(item);
                    if !text.is_empty() {
                        if let Some(index) = self.ensure_open(position, BlockKind::Thinking, out) {
                            self.items[position].emitted_text = true;
                            out.push(Event::ThinkingDelta { index, text });
                        }
                    }
                }
                if has_encrypted_content(item) {
                    let index = match self.items[position].ir_index {
                        Some(_) if self.open != Some(position) => None,
                        Some(index) => Some(index),
                        None => self.ensure_open(position, BlockKind::RedactedThinking, out),
                    };
                    if let Some(index) = index {
                        out.push(Event::SignatureDelta {
                            index,
                            signature: reasoning_signature(item),
                        });
                    }
                }
            }
            ItemKind::FunctionCall => {
                self.saw_tool_call = true;
                self.fill_tool_identity(position, item);
                let block = self.tool_kind(position);
                if let Some(index) = self.ensure_open(position, block, out) {
                    let state = &mut self.items[position];
                    let final_arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| state.arguments.clone());
                    if state.emitted_arguments.is_empty() {
                        if !final_arguments.is_empty() {
                            state.emitted_arguments = final_arguments.clone();
                            out.push(Event::ToolArgumentsDelta {
                                index,
                                partial_json: final_arguments,
                            });
                        }
                    } else if state.emitted_arguments != final_arguments {
                        tracing::warn!(
                            tool = %state.name,
                            "Responses 流中工具参数 delta 与 done 不一致，保留已输出的内容"
                        );
                    }
                }
            }
            ItemKind::CustomToolCall => {
                self.saw_tool_call = true;
                self.fill_tool_identity(position, item);
                let block = self.tool_kind(position);
                if let Some(index) = self.ensure_open(position, block, out) {
                    let state = &self.items[position];
                    let input = item
                        .get("input")
                        .and_then(Value::as_str)
                        .unwrap_or(&state.arguments);
                    out.push(Event::ToolArgumentsDelta {
                        index,
                        partial_json: custom_arguments(input),
                    });
                }
            }
            ItemKind::Ignored => {}
        }
        self.close_item(position, out);
    }

    /// 没收到 output_item.added 时用 done 项补全 id 与名字
    fn fill_tool_identity(&mut self, position: usize, item: &Value) {
        let state = &mut self.items[position];
        if state.call_id.is_empty() {
            state.call_id = call_id(item);
        }
        if state.name.is_empty() {
            state.name = string_field(item, "name");
        }
    }

    fn finalize(&mut self, kind: &str, response: &Value, out: &mut Vec<Event>) {
        self.ensure_started(Some(response), out);
        self.close_open(out);
        self.finished = true;
        let has_tool = self.saw_tool_call
            || response
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call" | "custom_tool_call")
                    )
                });
        let status = response.get("status").and_then(Value::as_str).unwrap_or(
            if kind == "response.incomplete" {
                "incomplete"
            } else {
                "completed"
            },
        );
        out.push(Event::Finish {
            stop_reason: stop_reason(
                Some(status),
                response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str),
                has_tool,
            ),
            usage: response.get("usage").map(parse_usage).unwrap_or_default(),
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

    fn text(text: &str) -> Block {
        Block::Text { text: text.into() }
    }

    fn tool_call(id: &str, name: &str, arguments: &str) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            signature: None,
        }
    }

    fn responses_sig(id: &str) -> Signature {
        Signature {
            source: Interface::OpenaiResponses,
            value: json!({"type": "reasoning", "id": id, "summary": [{"type": "summary_text", "text": "s"}], "encrypted_content": "gAAA", "status": "completed"}),
        }
    }

    fn request(messages: Vec<Message>) -> Request {
        Request {
            model: "claude-sonnet-5".into(),
            messages,
            ..Default::default()
        }
    }

    // ---- 请求生成 ----

    #[test]
    fn stateless_fields_model_and_instructions() {
        let mut req = request(vec![user(vec![text("hi")])]);
        req.system = vec![
            "x-anthropic-billing-header: cch=1\n\nYou are Claude Code.".into(),
            "Rules".into(),
        ];
        req.stream = true;
        req.user = Some("u1".into());
        req.temperature = Some(0.5);
        req.top_k = Some(3);
        req.stop = vec!["END".into()];
        let body = render_request(&req, &opts("gpt-5.1")).unwrap();
        assert_eq!(body["model"], "gpt-5.1");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "You are Claude Code.\nRules");
        assert_eq!(body["user"], "u1");
        assert_eq!(body["temperature"], 0.5);
        assert!(body.get("top_k").is_none() && body.get("stop").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
        assert_eq!(
            body["input"],
            json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
        );
    }

    #[test]
    fn max_output_tokens_is_clamped() {
        let mut req = request(vec![user(vec![text("hi")])]);
        for (given, expected) in [
            (Some(1), 16),
            (Some(15), 16),
            (Some(16), 16),
            (Some(1000), 1000),
            (Some(0), 0),
            (None, 16384),
        ] {
            req.max_output_tokens = given;
            let body = render_request(&req, &opts("m")).unwrap();
            assert_eq!(body["max_output_tokens"], expected, "{given:?}");
        }
    }

    #[test]
    fn reasoning_mapping() {
        let mut req = request(vec![user(vec![text("hi")])]);
        for (reasoning, expected) in [
            (Reasoning::Unspecified, None),
            (Reasoning::Disabled, None),
            (Reasoning::Effort(Effort::Low), Some("low")),
            (Reasoning::Budget(16000), Some("high")),
            (Reasoning::Adaptive(None), Some("medium")),
            (Reasoning::Adaptive(Some(Effort::XHigh)), Some("xhigh")),
        ] {
            req.reasoning = reasoning;
            let body = render_request(&req, &opts("gpt-5.1")).unwrap();
            match expected {
                None => assert!(body.get("reasoning").is_none(), "{reasoning:?}"),
                Some(effort) => assert_eq!(
                    body["reasoning"],
                    json!({"effort": effort, "summary": "auto"}),
                    "{reasoning:?}"
                ),
            }
        }
    }

    #[test]
    fn tool_results_first_then_user_content_with_images() {
        let req = request(vec![user(vec![
            text("and?"),
            Block::Image {
                source: MediaSource::Url("https://x/a.png".into()),
            },
            Block::ToolResult {
                call_id: "call_1".into(),
                content: vec![text("a.txt"), text("b.txt")],
                is_error: false,
            },
            Block::ToolResult {
                call_id: "call_2".into(),
                content: vec![
                    text("shot"),
                    Block::Image {
                        source: MediaSource::Base64 {
                            media_type: "image/png".into(),
                            data: "iVBOR".into(),
                        },
                    },
                ],
                is_error: false,
            },
        ])]);
        let body = render_request(&req, &opts("m")).unwrap();
        assert_eq!(
            body["input"],
            json!([
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt\nb.txt"},
                {"type": "function_call_output", "call_id": "call_2", "output": [
                    {"type": "input_text", "text": "shot"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBOR"}
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "and?"},
                    {"type": "input_image", "image_url": "https://x/a.png"}
                ]}
            ])
        );
    }

    #[test]
    fn documents_are_unsupported() {
        let document = Block::Document {
            source: MediaSource::Url("https://x/a.pdf".into()),
            name: None,
        };
        let req = request(vec![user(vec![document.clone()])]);
        assert!(render_request(&req, &opts("m")).is_err());
        let req = request(vec![user(vec![Block::ToolResult {
            call_id: "c".into(),
            content: vec![document],
            is_error: false,
        }])]);
        assert!(render_request(&req, &opts("m")).is_err());
    }

    #[test]
    fn assistant_items_merge_text_and_replay_same_source_reasoning() {
        let req = request(vec![
            user(vec![text("q")]),
            assistant(vec![
                Block::Thinking {
                    text: "s".into(),
                    signature: Some(responses_sig("rs_1")),
                },
                text("Running"),
                text(" now."),
                tool_call("call_1", "Bash", r#"{"command":"ls"}"#),
                Block::RedactedThinking {
                    signature: responses_sig("rs_2"),
                },
                tool_call("call_2", "apply_patch", ""),
            ]),
        ]);
        let body = render_request(&req, &opts("m")).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input[1],
            json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "s"}], "encrypted_content": "gAAA"}),
            "status is stripped, the rest kept"
        );
        assert_eq!(
            input[2],
            json!({"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "Running"},
                {"type": "output_text", "text": " now."}
            ]})
        );
        assert_eq!(
            input[3],
            json!({"type": "function_call", "call_id": "call_1", "name": "Bash", "arguments": "{\"command\":\"ls\"}"})
        );
        assert_eq!(input[4]["id"], "rs_2");
        assert_eq!(input[5]["arguments"], "{}");
        assert_eq!(input.len(), 6);
    }

    #[test]
    fn cross_source_or_unsigned_reasoning_is_dropped() {
        let req = request(vec![assistant(vec![
            Block::Thinking {
                text: "t".into(),
                signature: None,
            },
            Block::Thinking {
                text: "t".into(),
                signature: Some(Signature {
                    source: Interface::Claude,
                    value: json!({"type": "thinking", "thinking": "t", "signature": "x"}),
                }),
            },
            Block::RedactedThinking {
                signature: Signature {
                    source: Interface::Gemini,
                    value: json!({"sig": "x", "text": true}),
                },
            },
            text("answer"),
        ])]);
        let body = render_request(&req, &opts("m")).unwrap();
        assert_eq!(
            body["input"],
            json!([{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}])
        );
    }

    #[test]
    fn trailing_reasoning_without_follower_is_removed() {
        let req = request(vec![
            assistant(vec![
                text("a"),
                Block::RedactedThinking {
                    signature: responses_sig("rs_tail"),
                },
            ]),
            // 只有推理的 assistant 消息：后一条消息里的项不算跟随
            assistant(vec![Block::RedactedThinking {
                signature: responses_sig("rs_only"),
            }]),
            user(vec![text("next")]),
        ]);
        let body = render_request(&req, &opts("m")).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[1]["role"], "user");
        assert!(!body.to_string().contains("rs_"));
    }

    #[test]
    fn tools_web_search_and_tool_choice() {
        let mut req = request(vec![user(vec![text("hi")])]);
        req.tools = vec![
            Tool {
                name: "Bash".into(),
                description: Some("run".into()),
                parameters: json!({"properties": {"u": {"type": "string", "format": "uri"}}}),
                custom: false,
            },
            Tool {
                name: "apply_patch".into(),
                description: None,
                parameters: json!({"type": "object", "properties": {"input": {"type": "string"}}}),
                custom: true,
            },
        ];
        req.server_tools = vec![
            ServerTool {
                kind: ServerToolKind::WebSearch,
                raw: json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5}),
            },
            ServerTool {
                kind: ServerToolKind::Other,
                raw: json!({"type": "code_execution_20250522"}),
            },
        ];
        req.parallel_tool_calls = Some(false);
        req.tool_choice = ToolChoice::Named("Bash".into());
        let body = render_request(&req, &opts("m")).unwrap();
        assert_eq!(
            body["tools"],
            json!([
                {"type": "function", "name": "Bash", "description": "run",
                 "parameters": {"properties": {"u": {"type": "string"}}, "type": "object"}, "strict": false},
                {"type": "function", "name": "apply_patch",
                 "parameters": {"type": "object", "properties": {"input": {"type": "string"}}}, "strict": false},
                {"type": "web_search"}
            ])
        );
        assert_eq!(body["max_tool_calls"], 5);
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "Bash"})
        );
        assert_eq!(body["parallel_tool_calls"], false);

        for (choice, expected) in [
            (ToolChoice::Auto, json!("auto")),
            (ToolChoice::None, json!("none")),
            (ToolChoice::Required, json!("required")),
            (ToolChoice::Named("web_search".into()), json!("auto")),
        ] {
            req.tool_choice = choice;
            let body = render_request(&req, &opts("m")).unwrap();
            assert_eq!(body["tool_choice"], expected);
        }

        // 只有被剔除的内置工具：不写 tools / tool_choice
        req.tools.clear();
        req.server_tools.remove(0);
        let body = render_request(&req, &opts("m")).unwrap();
        assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
    }

    // ---- 非流式解析 ----

    #[test]
    fn parses_output_items_and_usage() {
        let body = json!({
            "id": "resp_1", "model": "gpt-5.1", "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": [
                    {"type": "summary_text", "text": "a"}, {"type": "summary_text", "text": "b"}
                ], "encrypted_content": "gAAA"},
                {"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "gBBB"},
                {"type": "reasoning", "id": "rs_3", "summary": [{"type": "summary_text", "text": "plain"}]},
                {"type": "reasoning", "id": "rs_4", "summary": []},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "Hi"}, {"type": "refusal", "refusal": " no"}
                ]},
                {"type": "web_search_call", "id": "ws_1", "status": "completed"},
                {"type": "function_call", "call_id": "call_1", "name": "Bash", "arguments": "{\"a\":1}", "status": "completed"},
                {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch"}
            ],
            "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 30},
                      "output_tokens": 20, "output_tokens_details": {"reasoning_tokens": 7}}
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.id, "resp_1");
        assert_eq!(response.model, "gpt-5.1");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        let output = body["output"].as_array().unwrap();
        assert_eq!(
            response.content,
            vec![
                Block::Thinking {
                    text: "a\n\nb".into(),
                    signature: Some(reasoning_signature(&output[0])),
                },
                Block::RedactedThinking {
                    signature: reasoning_signature(&output[1]),
                },
                Block::Thinking {
                    text: "plain".into(),
                    signature: None,
                },
                text("Hi no"),
                tool_call("call_1", "Bash", "{\"a\":1}"),
                tool_call("call_2", "apply_patch", r#"{"input":"*** Begin Patch"}"#),
            ]
        );
        let usage = response.usage;
        assert_eq!(usage.input_tokens, 70);
        assert_eq!(usage.cache_read_tokens, 30);
        assert_eq!(usage.total_input(), 100, "三桶恒等");
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.reasoning_tokens, Some(7));
    }

    #[test]
    fn stop_reasons() {
        let parse = |body: Value| parse_response(&body).unwrap().stop_reason;
        let text_output =
            json!([{"type": "message", "content": [{"type": "output_text", "text": "x"}]}]);
        assert_eq!(
            parse(json!({"status": "completed", "output": text_output})),
            StopReason::EndTurn
        );
        assert_eq!(parse(json!({"output": text_output})), StopReason::EndTurn);
        assert_eq!(
            parse(
                json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}})
            ),
            StopReason::MaxTokens
        );
        assert_eq!(
            parse(json!({"status": "incomplete"})),
            StopReason::MaxTokens
        );
        assert_eq!(
            parse(
                json!({"status": "incomplete", "incomplete_details": {"reason": "content_filter"}})
            ),
            StopReason::Refusal
        );
    }

    #[test]
    fn failures_inside_2xx_are_errors() {
        assert_eq!(
            parse_response(&json!({"status": "failed", "error": {"message": "boom"}})),
            Err("boom".into())
        );
        assert!(parse_response(&json!({"status": "cancelled"})).is_err());
        assert_eq!(
            parse_response(&json!({"status": "completed", "error": {"message": "late"}})),
            Err("late".into())
        );
        assert!(
            parse_response(&json!({"status": "completed", "error": null, "output": []})).is_ok()
        );
    }

    #[test]
    fn incomplete_function_call_arguments_are_replaced() {
        let body = json!({
            "status": "incomplete",
            "output": [
                {"type": "function_call", "call_id": "c1", "name": "A", "arguments": "{\"a\":", "status": "incomplete"},
                {"type": "function_call", "call_id": "c2", "name": "B", "arguments": "{\"b\":2}"}
            ]
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(
            response.content,
            vec![
                tool_call("c1", "A", "{}"),
                tool_call("c2", "B", "{\"b\":2}")
            ]
        );
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn error_message_extraction() {
        assert_eq!(
            error_message(&json!({"error": {"message": "bad", "type": "invalid_request_error"}})),
            Some("bad".into())
        );
        assert_eq!(error_message(&json!({"message": "m"})), Some("m".into()));
        assert_eq!(
            error_message(&json!({"error": "plain"})),
            Some("plain".into())
        );
        assert_eq!(error_message(&json!({"error": {"message": " "}})), None);
        assert_eq!(error_message(&json!({})), None);
    }

    // ---- 流式解码 ----

    /// (event, data)
    fn decode(events: &[(&str, Value)]) -> Vec<Event> {
        let mut decoder = StreamDecoder::new();
        let mut out: Vec<Event> = events
            .iter()
            .flat_map(|(name, data)| decoder.feed(Some(name), &data.to_string()))
            .collect();
        out.extend(decoder.finish());
        out
    }

    fn ev(kind: &str, mut data: Value) -> (&str, Value) {
        data["type"] = json!(kind);
        (kind, data)
    }

    fn created() -> (&'static str, Value) {
        ev(
            "response.created",
            json!({"response": {"id": "resp_1", "model": "gpt-5.1", "status": "in_progress"}}),
        )
    }

    fn start() -> Event {
        Event::Start {
            id: "resp_1".into(),
            model: "gpt-5.1".into(),
            usage: Usage::default(),
        }
    }

    fn completed(output: Value) -> (&'static str, Value) {
        ev(
            "response.completed",
            json!({"response": {"id": "resp_1", "status": "completed", "output": output,
                "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 4}, "output_tokens": 5}}}),
        )
    }

    fn finish(stop_reason: StopReason) -> Event {
        Event::Finish {
            stop_reason,
            usage: Usage {
                input_tokens: 6,
                cache_read_tokens: 4,
                output_tokens: 5,
                ..Default::default()
            },
        }
    }

    fn text_stream() -> Vec<(&'static str, Value)> {
        vec![
            created(),
            ev(
                "response.in_progress",
                json!({"response": {"id": "resp_1"}}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
            ),
            ev(
                "response.content_part.added",
                json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "part": {"type": "output_text", "text": ""}}),
            ),
            ev(
                "response.output_text.delta",
                json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "delta": "Hel"}),
            ),
            ev(
                "response.output_text.delta",
                json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "delta": "lo 你好"}),
            ),
            ev(
                "response.output_text.done",
                json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "text": "Hello 你好"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": "Hello 你好"}]}}),
            ),
            completed(json!([{"type": "message"}])),
        ]
    }

    #[test]
    fn stream_text() {
        assert_eq!(
            decode(&text_stream()),
            vec![
                start(),
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::Text
                },
                Event::TextDelta {
                    index: 0,
                    text: "Hel".into()
                },
                Event::TextDelta {
                    index: 0,
                    text: "lo 你好".into()
                },
                Event::BlockStop { index: 0 },
                finish(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn stream_empty_message_opens_no_block() {
        let events = decode(&[
            created(),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "content": []}}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "content": []}}),
            ),
            completed(json!([])),
        ]);
        assert_eq!(events, vec![start(), finish(StopReason::EndTurn)]);
    }

    fn reasoning_item(summary: Value) -> Value {
        json!({"type": "reasoning", "id": "rs_1", "summary": summary, "encrypted_content": "gAAA"})
    }

    fn tool_stream() -> Vec<(&'static str, Value)> {
        let done_reasoning = reasoning_item(json!([
            {"type": "summary_text", "text": "first"}, {"type": "summary_text", "text": "second"}
        ]));
        vec![
            created(),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
            ),
            ev(
                "response.reasoning_summary_part.added",
                json!({"output_index": 0, "item_id": "rs_1", "summary_index": 0}),
            ),
            ev(
                "response.reasoning_summary_text.delta",
                json!({"output_index": 0, "item_id": "rs_1", "summary_index": 0, "delta": "fir"}),
            ),
            ev(
                "response.reasoning_summary_text.delta",
                json!({"output_index": 0, "item_id": "rs_1", "summary_index": 0, "delta": "st"}),
            ),
            ev(
                "response.reasoning_summary_text.done",
                json!({"output_index": 0, "item_id": "rs_1", "summary_index": 0, "text": "first"}),
            ),
            ev(
                "response.reasoning_summary_text.delta",
                json!({"output_index": 0, "item_id": "rs_1", "summary_index": 1, "delta": "second"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": done_reasoning}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Bash", "arguments": ""}}),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({"output_index": 1, "item_id": "fc_1", "delta": "{\"a\""}),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({"output_index": 1, "item_id": "fc_1", "delta": ":1}"}),
            ),
            ev(
                "response.function_call_arguments.done",
                json!({"output_index": 1, "item_id": "fc_1", "arguments": "{\"a\":1}"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Bash", "arguments": "{\"a\":1}", "status": "completed"}}),
            ),
            completed(json!([{"type": "reasoning"}, {"type": "function_call"}])),
        ]
    }

    #[test]
    fn stream_reasoning_parts_and_function_call() {
        let signature = reasoning_signature(&reasoning_item(json!([
            {"type": "summary_text", "text": "first"}, {"type": "summary_text", "text": "second"}
        ])));
        assert_eq!(
            decode(&tool_stream()),
            vec![
                start(),
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::Thinking
                },
                Event::ThinkingDelta {
                    index: 0,
                    text: "fir".into()
                },
                Event::ThinkingDelta {
                    index: 0,
                    text: "st".into()
                },
                Event::ThinkingDelta {
                    index: 0,
                    text: "\n\nsecond".into()
                },
                Event::SignatureDelta {
                    index: 0,
                    signature
                },
                Event::BlockStop { index: 0 },
                Event::BlockStart {
                    index: 1,
                    kind: BlockKind::ToolCall {
                        id: "call_1".into(),
                        name: "Bash".into()
                    }
                },
                Event::ToolArgumentsDelta {
                    index: 1,
                    partial_json: "{\"a\"".into()
                },
                Event::ToolArgumentsDelta {
                    index: 1,
                    partial_json: ":1}".into()
                },
                Event::BlockStop { index: 1 },
                finish(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn stream_encrypted_only_reasoning_is_redacted() {
        let item = reasoning_item(json!([]));
        let events = decode(&[
            created(),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": item}),
            ),
            // 既无文本也无 encrypted_content：不开块
            ev(
                "response.output_item.added",
                json!({"output_index": 1, "item": {"type": "reasoning", "id": "rs_2", "summary": []}}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 1, "item": {"type": "reasoning", "id": "rs_2", "summary": []}}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index": 2, "item": {"type": "message", "id": "msg_1", "content": []}}),
            ),
            ev(
                "response.output_text.delta",
                json!({"output_index": 2, "item_id": "msg_1", "delta": "ok"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 2, "item": {"type": "message", "id": "msg_1", "content": []}}),
            ),
            completed(json!([])),
        ]);
        assert_eq!(
            events,
            vec![
                start(),
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::RedactedThinking
                },
                Event::SignatureDelta {
                    index: 0,
                    signature: reasoning_signature(&item)
                },
                Event::BlockStop { index: 0 },
                Event::BlockStart {
                    index: 1,
                    kind: BlockKind::Text
                },
                Event::TextDelta {
                    index: 1,
                    text: "ok".into()
                },
                Event::BlockStop { index: 1 },
                finish(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn stream_function_call_arguments_only_in_done() {
        let events = decode(&[
            created(),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read", "arguments": ""}}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read", "arguments": "{\"p\":\"a\"}"}}),
            ),
            completed(json!([])),
        ]);
        assert_eq!(
            events,
            vec![
                start(),
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::ToolCall {
                        id: "call_1".into(),
                        name: "Read".into()
                    }
                },
                Event::ToolArgumentsDelta {
                    index: 0,
                    partial_json: "{\"p\":\"a\"}".into()
                },
                Event::BlockStop { index: 0 },
                finish(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn stream_mismatched_done_keeps_emitted_arguments() {
        let events = decode(&[
            created(),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "A"}}),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({"output_index": 0, "delta": "{\"x\":1}"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "A", "arguments": "{\"x\":2}"}}),
            ),
            completed(json!([])),
        ]);
        let arguments: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::ToolArgumentsDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(arguments, vec!["{\"x\":1}"]);
    }

    #[test]
    fn stream_custom_tool_call_is_wrapped() {
        let events = decode(&[
            created(),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_9", "name": "apply_patch", "input": ""}}),
            ),
            ev(
                "response.custom_tool_call_input.delta",
                json!({"output_index": 0, "item_id": "ctc_1", "delta": "*** Begin"}),
            ),
            ev(
                "response.custom_tool_call_input.delta",
                json!({"output_index": 0, "item_id": "ctc_1", "delta": " Patch\n\"x\""}),
            ),
            ev(
                "response.custom_tool_call_input.done",
                json!({"output_index": 0, "item_id": "ctc_1", "input": "*** Begin Patch\n\"x\""}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_9", "name": "apply_patch"}}),
            ),
            completed(json!([])),
        ]);
        assert_eq!(
            events,
            vec![
                start(),
                Event::BlockStart {
                    index: 0,
                    kind: BlockKind::ToolCall {
                        id: "call_9".into(),
                        name: "apply_patch".into()
                    }
                },
                Event::ToolArgumentsDelta {
                    index: 0,
                    partial_json: json!({"input": "*** Begin Patch\n\"x\""}).to_string()
                },
                Event::BlockStop { index: 0 },
                finish(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn stream_incomplete_closes_open_blocks() {
        let events = decode(&[
            created(),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "message", "id": "msg_1"}}),
            ),
            ev(
                "response.output_text.delta",
                json!({"output_index": 0, "delta": "trunc"}),
            ),
            ev(
                "response.incomplete",
                json!({"response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {"input_tokens": 3, "output_tokens": 16}}}),
            ),
        ]);
        assert_eq!(
            &events[3..],
            &[
                Event::BlockStop { index: 0 },
                Event::Finish {
                    stop_reason: StopReason::MaxTokens,
                    usage: Usage {
                        input_tokens: 3,
                        output_tokens: 16,
                        ..Default::default()
                    }
                }
            ]
        );
        let filtered = decode(&[
            created(),
            ev(
                "response.incomplete",
                json!({"response": {"status": "incomplete", "incomplete_details": {"reason": "content_filter"}}}),
            ),
        ]);
        assert!(matches!(
            filtered.last(),
            Some(Event::Finish {
                stop_reason: StopReason::Refusal,
                ..
            })
        ));
    }

    #[test]
    fn stream_failed_error_and_truncation() {
        let events = decode(&[
            created(),
            ev(
                "response.failed",
                json!({"response": {"status": "failed", "error": {"code": "server_error", "message": "overloaded"}}}),
            ),
            // 结束后的事件忽略
            ev(
                "response.output_text.delta",
                json!({"output_index": 0, "delta": "x"}),
            ),
        ]);
        assert_eq!(
            events,
            vec![
                start(),
                Event::Error {
                    kind: "api_error".into(),
                    message: "overloaded".into()
                }
            ]
        );

        let events = decode(&[ev(
            "error",
            json!({"code": "rate_limit", "message": "slow down"}),
        )]);
        assert_eq!(
            events,
            vec![Event::Error {
                kind: "api_error".into(),
                message: "slow down".into()
            }]
        );

        // 没有 completed 就结束
        let mut stream = text_stream();
        stream.pop();
        let events = decode(&stream);
        assert!(matches!(events.last(), Some(Event::Error { .. })));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Error { .. }))
                .count(),
            1
        );

        // 已结束的流 finish 为空
        let mut decoder = StreamDecoder::new();
        for (name, data) in text_stream() {
            decoder.feed(Some(name), &data.to_string());
        }
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn stream_event_type_falls_back_to_sse_event_name() {
        let mut decoder = StreamDecoder::new();
        let events = decoder.feed(
            Some("response.created"),
            r#"{"response":{"id":"resp_1","model":"gpt-5.1"}}"#,
        );
        assert_eq!(events, vec![start()]);
    }

    /// 整段 SSE 以任意字节切分经 SseParser 后解码结果相同
    #[test]
    fn chunking_does_not_change_events() {
        let mut stream = tool_stream();
        stream.pop();
        stream.extend(text_stream().into_iter().skip(2).map(|(name, mut data)| {
            if let Some(index) = data.get_mut("output_index") {
                *index = json!(2);
            }
            (name, data)
        }));
        let sse: String = stream
            .iter()
            .map(|(name, data)| {
                String::from_utf8(crate::sse::frame(Some(name), &data.to_string()).to_vec())
                    .unwrap()
            })
            .collect::<String>()
            .replace("\n\n", "\r\n\r\n");
        let bytes = sse.as_bytes();

        let run = |size: usize| {
            let mut parser = SseParser::new();
            let mut decoder = StreamDecoder::new();
            let mut out = Vec::new();
            let mut sse_events = Vec::new();
            for chunk in bytes.chunks(size) {
                sse_events.extend(parser.feed(chunk));
            }
            sse_events.extend(parser.finish());
            for event in sse_events {
                out.extend(decoder.feed(event.event.as_deref(), &event.data));
            }
            out.extend(decoder.finish());
            out
        };
        let expected = run(bytes.len());
        assert!(matches!(
            expected.last(),
            Some(Event::Finish {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
        assert!(expected.contains(&Event::TextDelta {
            index: 2,
            text: "lo 你好".into()
        }));
        for size in 1..bytes.len() {
            assert_eq!(run(size), expected, "chunk size {size}");
        }
    }
}
