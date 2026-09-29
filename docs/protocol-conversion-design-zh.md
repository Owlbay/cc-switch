# 协议转换组件设计方案

> 状态：草案 v3（已按 Fable 二次审查修订）· 基线：`refactor/standalone-proxy-core@885140d9e`
> 关联：[passthrough-relay-design-zh.md](passthrough-relay-design-zh.md)（透传 core，本文实现其 §7 预留的改写组件中的"协议转换"一类）
>
> 文中 `文件:行号` 均指本仓库 `src-tauri/src/proxy/providers/` 下桌面端现有转换代码（协议细节的对照），或 `src-tauri/crates/cc-proxy-core/src/` 下的透传 core。

## 1. 目标与边界

让客户端用一种协议访问、上游用另一种协议提供服务，例如 Claude Code（Anthropic Messages）接一个只提供 OpenAI Chat Completions 的上游。转换通过统一中间表示（IR）完成：每种协议只需实现"解析为 IR"与"从 IR 生成"两个方向，任意两种协议即可互转。

| 原则 | 说明 |
|---|---|
| core 默认透传不变 | 没有配置 `protocol` / `model_map` 的上游仍然逐字节透传；透传 core 的 T1–T18 回归全部保持 |
| 转换在 core 之外 | 新 crate `cc-proxy-convert` 实现 IR 与四种协议的编解码；core 只提供钩子（trait），不依赖转换 crate |
| 按上游开启 | 转换是上游的属性：上游声明自己说的协议或模型映射，与所在接口不同（或有映射）时才转换 |
| 有损要显式 | IR 只建模跨协议有意义的字段；每对协议的丢弃字段在 §5 逐一列出，不静默发明语义 |
| 不因可降级的内容拒绝请求 | 客户端常规请求（Claude Code、Codex CLI 的默认请求形态）必须能通过转换；只有目标协议**无法表达且降级会改变结果**的内容才拒绝（§5.4） |
| 失败可诊断 | 无法表达的请求返回中转自身错误，不发往上游；转换失败不计入上游熔断 |

### 1.1 本期范围

| 项 | 本期 |
|---|---|
| 客户端协议（接口） | `claude`（Claude Code）、`openai_responses`（Codex CLI） |
| 上游协议 | `claude`、`openai_chat`、`openai_responses`、`gemini` 四种任意 |
| 转换方向 | 2 × 4 = 8 个（含 2 个同协议恒等转换） |
| IR 与 trait | 按四种协议完整设计；以后补客户端方向只增加编解码器，不改结构 |

### 1.2 后续项（本期不做）

- Gemini、OpenAI Chat 作为**客户端**协议（需要 Gemini 请求解析 / 响应生成、Chat 请求解析 / 响应生成）；
- `GET /v1/models` 列表格式转换（本期非 Anthropic 上游对 Claude 客户端的 `/v1/models` 走透传规则，见 §9）；
- Chat / Responses 的 `response_format` → Gemini `responseSchema`；
- 文档块（PDF）在 Chat / Responses 方向的支持（本期只做 Anthropic ↔ Gemini）；
- Anthropic 其他内置工具（`code_execution`、`text_editor`、`bash`、`computer` 等）历史消息的降级；
- 封套 HMAC 防伪（`ccsw1h`，§5.5 只保留版本位）；
- 厂商方言（Chat 的 `reasoning_content`、`enable_thinking`、`max_completion_tokens`、schema 限制差异等）作为可选配置。

非目标：模型能力探测、prompt 改写、用量计费。

## 2. 总体结构

```
客户端（协议 A ∈ {claude, openai_responses}）
   │
   ▼
cc-proxy-core：鉴权 → 识别接口 A → 读 body → 尝试循环（逐上游）
   │                                  │
   │  上游协议 == A 且无 model_map：    │  上游协议 B ≠ A，或有 model_map：
   │  原样透传（现状）                  ▼
   │                        Converter::convert_request(A→B)
   │                          A 解析为 IR →（model_map）→ 生成 B 请求（path / 头 / body）
   │                                  │
   │                         发往上游，收到 B 响应
   │                                  ▼
   │                        ResponseConverter(B→A)
   │                          非流式：B JSON → IR 结果 → A JSON
   │                          流式：B SSE 解码为 IR 事件 → 编码为 A SSE
   ▼
客户端
```

crate 依赖：

```
cc-proxy（bin） ──► cc-proxy-convert ──► cc-proxy-core
        └──────────────────────────────►┘
```

- `cc-proxy-core` 定义钩子 trait（§8），不依赖 `cc-proxy-convert`。
- `cc-proxy-convert` 实现 trait，依赖 core 只为使用 `Interface` 等公共类型。
- `cc-proxy` 在构造 `Relay` 时注入转换器；配置中出现需要转换的上游但未注入转换器时，`Relay::with_converter` / `Relay::new` 报配置错误。

## 3. 配置

上游新增三个可选字段：

```toml
[[upstreams.claude]]              # Claude Code 走 claude 接口
id = "deepseek"
base_url = "https://api.deepseek.com"
api_key = "${DEEPSEEK_API_KEY}"
protocol = "openai_chat"          # 上游说的协议；缺省等于所在接口
# 模型名映射：客户端请求的模型 → 上游模型；"*" 为兜底。缺省不改模型名
model_map = { "claude-sonnet-5" = "deepseek-chat", "*" = "deepseek-chat" }
# 客户端没带输出上限时写入上游请求的值；缺省 16384。只在转换路径生效
default_max_output_tokens = 16384

[[upstreams.claude]]              # 同协议 + model_map：恒等转换，只改模型名
id = "anthropic-alias"
base_url = "https://api.anthropic.com"
api_key = "${ANTHROPIC_API_KEY}"
model_map = { "claude-sonnet-5" = "claude-sonnet-4-5" }
```

### 3.1 语义

| `protocol` | `model_map` | 路径 |
|---|---|---|
| 缺省或等于接口 | 空 | **透传**：逐字节不变（现状） |
| 缺省或等于接口 | 非空 | **恒等转换**：请求解析为 IR、改模型名、再生成同协议请求；响应也经过 IR 重新生成。**不再逐字节透传**：IR 未建模的字段（如 `cache_control` 之外的厂商扩展、未知顶层字段）会丢失，`metadata`、`anthropic-beta` 等按 §5 处理。文档与 `check` 输出都要把这一点讲清楚，用户只有在接受这种代价时才应配置同协议 `model_map` |
| 不等于接口 | 任意 | **协议转换** |

- `protocol` 取值同接口名：`claude`、`openai_chat`、`openai_responses`、`gemini`。
- `model_map` 匹配顺序：精确匹配 → `*`；都不匹配时保留原模型名。键与值都不能为空字符串。
- `auth` 缺省值按**上游协议**取（`openai_chat` 上游默认 Bearer，`gemini` 上游默认 `x-goog-api-key`），而不是按所在接口。core 现状是按接口取（`upstream.rs:250` 的 `auth: upstream.auth_scheme(interface)`，`config.rs:57-63` 的 `AuthScheme::default_for`），**C1 需改为按上游协议取**；显式设置 `auth` 时不变。`protocol` 缺省等于接口时行为与现状完全一致。
- `default_max_output_tokens`：客户端请求没有输出上限时（Codex CLI 不发 `max_output_tokens`）写入上游请求的值，缺省 16384。它决定 Anthropic 的 `max_tokens`，进而决定 thinking 预算的钳位上限（§5.2 第 1 条）；`check` 输出每个转换上游的该值。透传路径不使用。
- 同一接口的上游列表可以混合透传与转换，故障转移按顺序进行（§8.3）。

### 3.2 校验

- `protocol` 必须是四个取值之一。
- 需要转换（协议转换或恒等转换）的上游所在接口本期只能是 `claude` 或 `openai_responses`；`openai_chat` / `gemini` 接口下出现 `protocol ≠ 接口` 或非空 `model_map` → 配置错误"该接口的转换本期不支持"（§1.2）。
- 需要转换的上游存在而 `Relay` 没有注入转换器 → `Relay::new` 报错；注入的转换器不支持该方向（`Converter::supports(client, upstream)` 返回 false）→ 同样在构造时报错，而不是运行时 400。
- `model_map` 的键、值不能为空；`"*"` 只能出现一次。
- `default_max_output_tokens` 必须大于 0；透传上游（无 `protocol`、无 `model_map`）设置了它 → `check` 给出警告"该字段只在转换路径生效"。
- 校验位置：以上取值检查放在 `RelayConfig::validate`；`Converter::supports` 检查放在 `Relay::with_converter`。

## 4. IR 数据模型

IR 以"内容块序列"为中心，覆盖四种协议的公共语义。以下为 Rust 类型草案（`cc-proxy-convert::ir`）。

### 4.1 请求

```rust
pub struct Request {
    pub model: String,
    pub system: Vec<Block>,            // 仅 Text
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub tool_choice: ToolChoice,       // Auto | None | Required | Named(String)
    pub parallel_tool_calls: Option<bool>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop: Vec<String>,
    pub stream: bool,
    pub reasoning: Reasoning,          // Unspecified | Disabled | Effort(Effort) | Budget(u64) | Adaptive(Option<Effort>)
    pub user: Option<String>,          // metadata.user_id / user / safety_identifier
}

pub enum Effort { Minimal, Low, Medium, High, XHigh }

pub struct Message { pub role: Role, pub content: Vec<Block> }   // Role: User | Assistant

pub enum Block {
    Text { text: String },
    Image { source: MediaSource },      // Base64 { media_type, data } | Url(String)
    Document { source: MediaSource, name: Option<String> },   // PDF 等
    ToolCall { id: String, name: String, arguments: String, signature: Option<Signature> },
    // arguments 为 JSON 文本；custom 工具（§5.2）降级后为 {"input": "<文本>"}
    ToolResult { call_id: String, content: Vec<Block>, is_error: bool },
    Thinking { text: String, signature: Option<Signature> },
    RedactedThinking { signature: Signature },   // 只有不透明数据、没有可见文本
}

/// 上游返回的、要求在后续请求中原样回放的不透明值，及其来源协议（§5.5）
pub struct Signature { pub source: Interface, pub value: String }

pub struct Tool { pub name: String, pub description: Option<String>, pub parameters: serde_json::Value }
```

约定：

- `ToolCall.arguments` 保存为 JSON **文本**，不解析再序列化，避免数字精度与键序变化；生成需要对象的协议（Gemini `args`、Anthropic `input`）时才解析，解析失败按 §5.4 处理。
- `ToolResult` 作为独立块出现在 User 消息中；生成各协议时的拆分与顺序见 §5.3。
- `ToolCall.signature` 承载 Gemini 附在 `functionCall` part 上的 `thoughtSignature`（Gemini 3 要求后续请求回放，否则上游可能拒绝：`transform_gemini.rs:684-691`）；`Thinking.signature` 承载 Anthropic thinking 块的 `signature`、Responses `reasoning` 项（整项）以及 Gemini 附在文本 part 上的 `thoughtSignature`（`streaming_gemini.rs:83-95`）。承载与回放规则见 §5.5。
- 工具调用 id：Gemini 的 `functionCall` 可能没有 `id`，解析时生成 `call_<uuid>`（全局唯一，不能按序号 `call_{n}`，否则跨轮会重复；对照 `transform_gemini.rs:35-43`），生成 Gemini 请求时不回传生成的 id，只回填函数名（依据同一请求内 `ToolCall.id → name` 的对应关系）。

### 4.2 非流式响应

```rust
pub struct Response {
    pub id: String,
    pub model: String,
    pub content: Vec<Block>,            // Text / Thinking / RedactedThinking / ToolCall
    pub stop_reason: StopReason,        // EndTurn | MaxTokens | ToolUse | StopSequence | Refusal | Other(String)
    pub usage: Usage,
}

pub struct Usage {
    pub input_tokens: u64,              // 不含缓存读写（Anthropic 语义）
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
}
```

usage 归一（三桶互斥，恒等 `input + cache_read + cache_write == 上游含缓存的输入数`，对照 `transform.rs:668-683`）：

| 协议 | 解析 | 生成 |
|---|---|---|
| Anthropic | `input_tokens` 即 IR `input_tokens`；`cache_read_input_tokens` / `cache_creation_input_tokens` | 同名 |
| Chat | `prompt_tokens − prompt_tokens_details.cached_tokens − cache_write_tokens`（有些上游直接给 `cache_read_input_tokens`，优先取直传字段） | `prompt_tokens = input + cache_read + cache_write`，`prompt_tokens_details.cached_tokens = cache_read`，`completion_tokens_details.reasoning_tokens` |
| Responses | `input_tokens − input_tokens_details.cached_tokens`（`transform_responses.rs:2267-2280`） | `input_tokens = input + cache_read + cache_write`，`input_tokens_details.cached_tokens = cache_read`，`output_tokens_details.reasoning_tokens` |
| Gemini | `promptTokenCount − cachedContentTokenCount`；输出取 `candidatesTokenCount`（缺失时退化为 `totalTokenCount − promptTokenCount`，`transform_gemini.rs:1227`），`thoughtsTokenCount` 记为 `reasoning_tokens` | `promptTokenCount = input + cache_read`，`cachedContentTokenCount = cache_read` |

### 4.3 流式事件

```rust
pub enum Event {
    Start { id: String, model: String, usage: Usage },          // usage 可能只有输入侧
    BlockStart { index: usize, kind: BlockKind },               // Text | Thinking | RedactedThinking | ToolCall { id, name }
    TextDelta { index: usize, text: String },
    ThinkingDelta { index: usize, text: String },
    SignatureDelta { index: usize, signature: Signature },      // 块结束前至多一次
    ToolArgumentsDelta { index: usize, partial_json: String },
    BlockStop { index: usize },
    Finish { stop_reason: StopReason, usage: Usage },           // 完整 usage（输入 + 输出）
    Error { kind: String, message: String },
}
```

- 解码器负责把源协议的流**规范化**为上述事件：保证每个块有 `BlockStart` / `BlockStop` 配对、`index` 从 0 递增、`Finish` 恰好一次并且在所有 `BlockStop` 之后。
- 编码器只依赖规范化后的事件，按目标协议输出 SSE 帧。
- 解码器各自的边界情况见 §6.2。

## 5. 字段映射与有损清单

### 5.1 请求字段

| IR | Anthropic Messages | OpenAI Chat | OpenAI Responses | Gemini |
|---|---|---|---|---|
| `system` | `system`（字符串或 text 块数组，`cache_control` 丢弃） | 首条 `role: system`（解析时 `developer` 同） | `instructions`；解析时 `input[]` 里 `role: system / developer` 的 message 项也并入（`transform_codex_anthropic.rs:247-263`） | `systemInstruction.parts` |
| `messages` | `messages[].content` 块 | `messages[]`（content 字符串或 parts；`role: tool` 为工具结果） | `input[]` 项，见 §5.2 | `contents[]`（role `user` / `model`；`functionResponse` 放在 `user` 轮） |
| `tools` | `tools[]{name, description, input_schema}` | `tools[]{type:function, function{name, description, parameters}}` | `tools[]{type:function, name, description, parameters}` | `tools[].functionDeclarations[]`，schema 处理见 §5.2 |
| `tool_choice` | `auto` / `any` / `none` / `{type:tool,name}`；`disable_parallel_tool_use` → `parallel_tool_calls` | `auto` / `required` / `none` / `{type:function,function:{name}}` | `auto` / `required` / `none` / `{type:function,name}` | `toolConfig.functionCallingConfig.mode`（`AUTO` / `ANY` / `NONE`）+ `allowedFunctionNames` |
| `max_output_tokens` | `max_tokens`（**必填**；客户端未带时取上游 `default_max_output_tokens`，§3.1） | `max_tokens`（现有代码只对 o 系模型用 `max_completion_tokens`，`transform.rs:214-221`；大量兼容网关不识别后者，本期固定 `max_tokens`，方言选项见 §1.2） | `max_output_tokens`（**≥ 16**，小于 16 的正整数钳到 16；Claude 桌面端探测请求用 `max_tokens: 1`，`transform_responses.rs:1813-1826`） | `generationConfig.maxOutputTokens` |
| `temperature` / `top_p` / `top_k` / `stop` | 同名，`stop_sequences` | 同名（无 `top_k`），`stop` | 同名（无 `top_k` / `stop`，丢弃并记 debug） | `generationConfig.temperature / topP / topK / stopSequences` |
| `reasoning` | `thinking{type:enabled, budget_tokens}` / `{type:disabled}` / `{type:adaptive}` + `output_config{effort}`（Claude Code 对 4.6 系模型的默认形态，`transform_codex_anthropic.rs:301-341`） | `reasoning_effort` | `reasoning{effort}` | `generationConfig.thinkingConfig{thinkingBudget}`（`0` 表示关闭；Gemini 2.5 Pro 不接受 `0`，最小 128，`Disabled` 对它会被上游 400，写入使用说明） |
| `stream` | `stream` | `stream` + `stream_options.include_usage=true`（否则多数上游流式不回 usage，`transform.rs:276-300`） | `stream` | path `:streamGenerateContent?alt=sse` vs `:generateContent` |
| 无状态化 | — | — | 生成时**固定写** `store: false`，并保证 `include` 含 `reasoning.encrypted_content`（`transform_responses.rs:2003-2027`）：否则上游不返回 `encrypted_content`，§5.5 的封套永远为空、多轮 thinking 退化，且 OpenAI 默认会保存响应 | — |
| `user` | `metadata.user_id` | `user` | `user` | — |
| `model` | body `model` | body `model` | body `model` | path 中的 `models/{model}`，body 无 `model` |

reasoning 换算（对照 `transform_codex_anthropic.rs:36-46`，官方要求 Anthropic `budget_tokens ≥ 1024`）：

| Effort | budget_tokens | Gemini thinkingBudget |
|---|---|---|
| `minimal` / `low` | 2048 | 2048 |
| `medium` | 8192 | 8192 |
| `high` | 16384 | 16384 |
| `xhigh` | 24576 | 24576 |
| `Disabled`（Chat / Responses 的 `none`） | `thinking: {type: disabled}` | `thinkingBudget: 0` |

budget → effort 取最接近档；目标协议不认识的档位（如给 Chat 上游 `xhigh`）降到 `high`。解析 Chat / Responses 时 `max` / `ultra` 视为 `xhigh`（`transform_codex_anthropic.rs:36-46`）。

`Adaptive(effort)` 的换算：

- 解析 Anthropic：`thinking.type = adaptive` → `Adaptive(output_config.effort)`，`effort` 缺省为 `None`。
- 生成 Anthropic：`Adaptive(e)` → `thinking: {type: adaptive}` + `output_config: {effort: e}`（`e` 为 `None` 时不写 `output_config`），**原样回放**，恒等转换不得改成 `enabled` / `budget_tokens`（X13）；`Effort(e)` 给 adaptive 模型时也生成 adaptive 形态（`transform_codex_anthropic.rs:333-341`），模型是否为 adaptive 由模型名前缀判定。
- 生成 Chat / Responses：`Adaptive(Some(e))` → `e`；`Adaptive(None)` → `medium`。
- 生成 Gemini：`Adaptive(Some(e))` → 按上表取 `thinkingBudget`；`Adaptive(None)` → 不写 `thinkingConfig`（由上游决定）。

### 5.2 各协议的结构细节

**Anthropic 请求生成的合法性规范化**（不做会被 Anthropic 400，规则来自 `transform_codex_anthropic.rs`）：

1. `budget_tokens < max_tokens`：预算钳到 `max_tokens / 2`，钳后 `< 1024` 则改为不开 thinking（`:346-360`）。不抬高调用方的 `max_tokens`；调用方没带时用上游 `default_max_output_tokens`（缺省 16384，使 `high` 的 16384 预算钳到 8192 而不是 4096 下的 2048）。
2. thinking 开启时不发 `temperature` / `top_p` / `top_k`（`:369-376`）。
3. thinking 开启时不能强制工具（`tool_choice` 为 `any` / `tool`）：保留调用方的工具约束、改为关闭 thinking（`:412-424`）。
4. `tool_choice` 只在 `tools` 非空时发送（`:394-397`）。
5. 空或纯空白的 text 块丢弃；内容为空的消息丢弃；首条消息必须是 user，不是则补一条占位 user 文本（`:904-925`、`:1102-1120`）；末尾 assistant 消息的尾随空白裁掉（`:1075-1100`）。
6. tool_result 块排在同一条 user 消息的其他块之前（`:1139-1158`）；thinking / redacted_thinking 块排在 assistant 消息最前（`:1160-1180`）。
7. 有 tool_use 而下一条 user 消息里没有对应 tool_result 的"未完成工具轮"，整段丢弃（`:921-989`）。
8. **无同源签名的 thinking 整块丢弃**：`Thinking.signature` 为 `None`（如来自 Chat `reasoning_content` 的推理文本）、封套 `src` 不是 `claude`、封套解码失败，或 `RedactedThinking` 同理 → 该块不生成（Anthropic 对缺 `signature` 的 thinking 块 400；校验做法对照 `:71-99`）。这条适用于**所有**历史轮次，§5.5 的"降级关 thinking"只处理最后一轮 tool_use 之前没有签名 thinking 的情况。

**工具 schema 清洗**（对照 `transform.rs:499-530`、`gemini_schema.rs:18-215`）：

- 所有目标：根 schema 缺 `type` 时补 `type: object` 与 `properties: {}`；`description` 缺失或 null 时**省略**字段而不是输出 null（严格上游会 400，`transform.rs:252-256`）；递归删除 `format: uri`。
- Gemini：schema 只含 OpenAPI 子集（type / properties / required / items / enum / description / nullable / format / default / 数值与长度范围，以及成员都属于该子集的 `anyOf`）时写 `parameters`；含 `additionalProperties`、`oneOf` / `allOf`、`$ref` / `$defs`、`$schema`、`patternProperties`、`const`、`type` 为数组，或任何未知关键字时改写 `parametersJsonSchema`（`gemini_schema.rs:100-165`，未知关键字按保守处理）。
- Anthropic：`input_schema` 必须是 `type: object`（`transform_codex_anthropic.rs:441-462`）。

**Responses `input[]` 项**（对照 `transform_codex_anthropic.rs:507-700`、`transform_codex_chat.rs:118-240`）：

| 项 | IR |
|---|---|
| `message`（`role` user / assistant / system / developer；content 为字符串或 `input_text` / `output_text` / `refusal` / `input_image` / `input_file` parts） | User / Assistant 消息；system / developer 并入 `system` |
| `function_call{call_id, name, arguments}` | Assistant 消息中的 `ToolCall`（合并进紧邻的 assistant 消息） |
| `function_call_output{call_id, output}`（output 为字符串或 `input_text` / `input_image` 数组） | User 消息中的 `ToolResult`（连续项合并进同一条 user 消息） |
| `reasoning{id, summary[], encrypted_content}` | `Thinking` / `RedactedThinking`（§5.5） |
| `custom_tool_call{call_id, name, input}` / `custom_tool_call_output` | 降级为 `ToolCall{arguments: {"input": <string>}}` / `ToolResult`；对应工具定义 `type: custom` 降级为参数 `{input: string}` 的 function 工具（Codex CLI 对部分模型用 custom 工具发 `apply_patch`，`transform_codex_anthropic.rs:568-590`、`:484`） |
| `item_reference`、`web_search_call`、`local_shell_call` 等 | 见 §5.4 |
| `status: incomplete` 的历史工具调用项 | 丢弃（`:515-529`） |

**custom 工具的响应方向**：请求中 `type: custom` 的工具名记入 `OutboundMeta.custom_tool_names`。上游返回名字在该集合内的 `ToolCall` 时，生成 Responses 响应必须还原为 `custom_tool_call{call_id, name, input: arguments["input"]}`（`input` 取不到字符串时用整段 `arguments` 文本），流式用 `response.custom_tool_call_input.delta / done`（`streaming_codex_anthropic.rs:391-397`），否则 Codex 不会执行 `apply_patch`。

生成 Responses 请求时：`reasoning` 项后面必须跟同一生成轮的 message 或 function_call，否则删掉该 reasoning 项（`transform_responses.rs:2478-2500`）；`input_image` 用 `image_url`（data URL 或 http URL）；`input_file` 用 `file_data` + `filename`。

**Gemini**：`functionResponse.response` 必须是对象，文本结果包成 `{"content": <text>}`（`transform_gemini.rs:758-776`）；`functionResponse.name` 由同一请求内的 `ToolCall.id → name` 回填，回填不到时该 ToolResult 降级为 user 文本块并记 warn（不 400）。

**Chat**：assistant 消息只有 tool_calls 时 `content` 为 null；`tool_calls[].function.arguments` 为 JSON 文本。

### 5.3 多个工具结果与工具结果中的媒体

- 生成 Chat：一条 user 消息里的每个 `ToolResult` 各拆成一条 `role: tool` 消息，**先输出全部 tool 消息，再把剩余 user 内容作为一条 user 消息**（Chat 要求 tool 消息紧跟带 tool_calls 的 assistant 消息）。`role: tool` 不能带图片：图片移到随后的 user 消息，tool 消息里留一段文本占位（`transform.rs:399-420`、`:452-455`；测试 `:1233-1326`）。
- 生成 Responses：每个 `ToolResult` 一个 `function_call_output` 项，顺序同上；图片放进 `output` 数组的 `input_image`。
- 生成 Gemini：每个 `ToolResult` 一个 `functionResponse` part，同一 `user` 轮；图片作为 `functionResponse.parts`（Gemini 3）或紧随的 `inlineData` part（`transform_gemini.rs:735-757`）。
- 生成 Anthropic：全部 `tool_result` 块在前，其余块在后（§5.2 第 6 条）。

### 5.4 丢弃或降级的字段（跨协议时）

| 内容 | 处理 |
|---|---|
| Anthropic `cache_control` | 丢弃（目标协议无对应语义），记 debug |
| Anthropic `metadata` 除 `user_id` 外 | 丢弃 |
| Anthropic server tools（`web_search_20250305` 等 `type` 以 `web_search` 开头的工具；Claude Code 开启 WebSearch 时默认携带，`transform.rs:252-256`、`transform_responses.rs:300-315`） | 目标为 Responses：映射为内置 `{type: "web_search"}`（`max_uses` → `max_tool_calls`），响应中的 `web_search_call` 映射回 `server_tool_use` + `web_search_tool_result`（`transform_responses.rs:373-430`、`:2739-2780`）。目标为 Chat / Gemini：**剔除该工具并记 warn，不 400**；剔除后 `tool_choice` 指向它则改为 `auto`，`tools` 变空则一并删掉 `tool_choice`（`transform_codex_anthropic.rs:394-400`） |
| Anthropic `code_execution`、`text_editor`、`bash`、`computer` 等其他内置工具 | 同上剔除工具定义；历史消息中对应的 `server_tool_use` / 结果块本期直接丢弃并记 warn（降级为文本列为后续项，§1.2） |
| Responses `web_search` 工具 | 目标为 Anthropic：映射为 `web_search_20250305`；目标为 Chat / Gemini：剔除，同上 |
| Responses `file_search`、`computer_use_preview`、`local_shell`、`image_generation`、`mcp` 等内置工具；`item_reference`、`local_shell_call` 等输入项 | 剔除工具并记 warn；输入项降级为文本（`item_reference` 无法解引用，丢弃并记 warn） |
| Responses `previous_response_id`、`conversation` | **400** `relay_conversion_unsupported`：服务端状态无法在别的上游重建，静默丢弃会丢历史 |
| Responses `store`、`include`、`prompt_cache_key`、`text.verbosity`、`reasoning.summary`、`service_tier`、`truncation`、`safety_identifier`（Codex CLI 默认发 `store: false` 与 `include: ["reasoning.encrypted_content"]`，`transform_codex_anthropic.rs:2420-2433`） | 静默忽略；`store: true` 记 warn（上游不会保存） |
| Chat / Responses `n > 1`、`logprobs` | **400**（改变返回结果的形态） |
| Chat / Responses `response_format` | 目标为 Anthropic / Gemini 时 **400**（本期不做 `responseSchema` 映射，§1.2）；目标为 Chat ↔ Responses 时直译 |
| Gemini `safetySettings`、`cachedContent`（只在 Gemini 作客户端时出现，本期不涉及） | 丢弃 |
| 图片 URL | 按 URL 原样传递；目标为 Gemini 时 **400**（Gemini API Key 模式只接受 `inlineData`，不在中转内下载） |
| 文档块（PDF） | 本期只支持 Anthropic ↔ Gemini；目标为 Chat / Responses 时 **400**，Responses 客户端的 `input_file` 解析为 `Unsupported`（§1.2） |
| 工具调用 `arguments` 不是合法 JSON 对象 | 请求方向 **400**（`transform_codex_anthropic.rs:541-556`）；响应方向且上游 `status ≠ completed` 时替换为 `{}` 并记 warn（`transform_responses.rs:2685-2735`） |

原则：**目标协议无法表达且降级会改变结果的返回 400**（历史引用、多候选、结构化输出）；**可以在中转内完成的降级静默或 warn 后进行**（缓存标记、内置工具剔除、顺序调整、schema 清洗）。判断标准是"客户端常规请求必须能通过"：Claude Code 默认带 WebSearch、Codex CLI 默认带 `store` / `include` / `prompt_cache_key` / `reasoning.summary`，这些都不能导致 400。

### 5.5 签名与推理内容的承载

三种上游会返回要求原样回放的不透明值：Anthropic thinking 块的 `signature`（及 `redacted_thinking.data`）、Responses `reasoning` 项的 `encrypted_content`、Gemini part 上的 `thoughtSignature`。它们只对产生它们的上游有意义；中转不保存状态，因此必须通过**客户端协议**带回客户端、由客户端在下一轮原样回放。

**回放规则**

| 上游 | 客户端 Claude（Anthropic Messages） | 客户端 Codex（Responses） |
|---|---|---|
| 同协议（恒等转换） | thinking / redacted_thinking 块原样保留 | `reasoning` 项原样保留 |
| Anthropic | 原样 | `reasoning{summary: [thinking 文本], encrypted_content: 封套}`，封套内是整个 thinking / redacted_thinking 块（现有做法 `transform_codex_anthropic.rs:69-100`） |
| Responses | `thinking{thinking: summary 文本, signature: 封套}`；summary 为空则 `redacted_thinking{data: 封套}`；封套内是整个 `reasoning` 项（现有做法 `reasoning_bridge.rs:11-96`） | 原样 |
| Gemini（`functionCall` 上的签名） | 紧邻 tool_use **之前**插入 `redacted_thinking{data: 封套}`，封套带 `call_id`（thinking 块必须在 assistant 消息最前，§5.2 第 6 条，故插在所有 tool_use 之前、按 tool_use 顺序排列） | 紧邻 `function_call` 项之前插入 `reasoning{summary: [], encrypted_content: 封套}`，封套带 `call_id` |
| Gemini（文本 part / `thought` part 上的签名） | `thinking{thinking: 思考文本, signature: 封套}` 或 `redacted_thinking` | `reasoning{summary, encrypted_content: 封套}` |

解析客户端请求时的对应逆过程：遇到封套则解码；封套 `src` 与本次上游协议相同 → 还原为该协议的原始值（Gemini 的 `call_id` 型封套附到同一 assistant 消息中 id 相同的 `ToolCall.signature`；`text` 型封套附到该 assistant 消息最后一个 `Thinking` / `Text` 块的 `signature`）；不同 → 丢弃（§5.4 的 debug 日志）。非封套的原始值（客户端直接从同协议上游拿到的）视为来源等于客户端协议，只在上游协议与其相同时回放。

生成 Gemini 时签名的落点：`ToolCall.signature` 写到对应 `functionCall` part 的 `thoughtSignature`（并行调用时通常只有第一个调用带签名，有则写、无则不写）；`text` 型签名写到该 `model` 轮**最后一个** part 的 `thoughtSignature`（Gemini 要求文本签名位于上一 model 轮的末尾 part）。

**降级**：Anthropic 要求"thinking 开启时，跟在 tool_result 之后的那条 assistant 消息必须以签名 thinking 块开头"（`transform_codex_anthropic.rs:1010-1013`、`:323-333`）。生成 Anthropic 请求时，若最后一轮 assistant 含 tool_use 而没有可回放的签名 thinking（跨协议、封套解码失败、或客户端没带），本次请求改为 `thinking: {type: disabled}`，记 warn，**不返回 400**。生成 Gemini 时缺签名则不带 `thoughtSignature`（Gemini 3 可能拒绝，作为已知限制记录）；生成 Responses 时缺签名则不发 reasoning 项。

**封套格式**（版本化）

```
ccsw1.<src>.<base64url(JSON)>
```

- `ccsw1`：格式版本；不认识的版本 → 视为无效封套，按"缺签名"降级。
- `src`：`claude` / `openai_responses` / `gemini`。
- JSON 载荷：
  - `claude`：`{"block": <原始 thinking 或 redacted_thinking 块>}`
  - `openai_responses`：`{"item": <原始 reasoning 项>}`
  - `gemini`：`{"sig": "<thoughtSignature>", "call_id": "<对应 ToolCall.id>"}` 或 `{"sig": "...", "text": true}`
- 解码只接受结构正确的载荷（与编码同一校验，防止旧格式或残缺封套回放，`transform_codex_anthropic.rs:92-99` 的做法）。

**防伪取舍**：封套只在客户端与上游之间往返，最终由上游校验（Anthropic 校验 `signature`，Gemini 校验 `thoughtSignature`，Responses 校验 `encrypted_content`）。客户端伪造封套最多让**自己的**请求被上游拒绝，不能借中转获得额外权限，也不能影响其他客户端。因此**本期不做 HMAC**（§1.2）：随机密钥会在中转重启后让所有在途会话的签名失效（只能降级关 thinking），配置密钥又增加一项必须保密的配置，而收益只是"更早地拒绝伪造"。格式中的版本位为以后加签预留（如 `ccsw1h`），本期解码只认 `ccsw1`，其他版本按"缺签名"降级。

### 5.6 响应字段

| IR | Anthropic | Chat | Responses | Gemini |
|---|---|---|---|---|
| 文本 | `content[].text` | `choices[0].message.content`（字符串或 parts；`refusal` 作为文本） | `output[]` message → `output_text` / `refusal` | `candidates[0].content.parts[].text`（`thought ≠ true`） |
| 推理 | `thinking` 块 | `reasoning_content` / `reasoning`（非标准；解析时都识别，空字符串不建块；生成时不输出） | `reasoning` 项 `summary[]` | `parts[].thought = true` |
| 工具调用 | `tool_use` 块 | `tool_calls[]`（也识别旧式 `function_call`） | `function_call` 项 | `functionCall` part |
| id / model | `id` / `model` | `id` / `model` | `id` / `model` | `responseId` / `modelVersion`；缺失时用请求的模型名 |

stop reason（对照 `transform.rs:650-660`、`transform_gemini.rs:1241-1263`、`transform_responses.rs:2091-2108`；与现有代码的差别：Chat `content_filter` 现有代码映射为 `end_turn`，本文**有意**改为 `Refusal`，与 Gemini 安全拦截一致）：

| IR | Anthropic | Chat `finish_reason` | Responses | Gemini `finishReason` |
|---|---|---|---|---|
| `EndTurn` | `end_turn` | `stop` | `status: completed` | `STOP` / `FINISH_REASON_UNSPECIFIED` / 缺失 |
| `ToolUse` | `tool_use` | `tool_calls`（**`stop` 但有 tool_calls 也算**） | `completed` 且 output 含 function_call | **`STOP` 且 parts 含 functionCall**（Gemini 没有工具专用的 finishReason） |
| `MaxTokens` | `max_tokens` | `length` | `incomplete` + `incomplete_details.reason = max_output_tokens`（reason 缺失也按此） | `MAX_TOKENS` |
| `StopSequence` | `stop_sequence` | `stop`（无法区分，生成时用 `stop`） | `completed` | `STOP` |
| `Refusal` | `refusal` | `content_filter` | `incomplete` + `reason = content_filter` | `SAFETY` / `RECITATION` / `SPII` / `BLOCKLIST` / `PROHIBITED_CONTENT`；`promptFeedback.blockReason` 存在时整个响应视为 Refusal（`transform_gemini.rs:150-172`） |
| `Other` | `end_turn` + warn | `stop` + warn | `completed` | `STOP` + warn |

- 2xx 响应体内的失败：Responses `status: failed / cancelled` 或带 `error` 对象（`transform_responses.rs:2121-2140`）、Chat 顶层 `error`、Gemini 顶层 `error` → 按 §7 的错误响应处理（非流式返回 502 `relay_conversion_error` 并附上游 message；流式按 §6.4）。
- 生成 Anthropic 时 `stop_sequence` 字段固定 null（跨协议拿不到命中的序列）。

## 6. 流式转换

### 6.1 SSE 解析

- 解析器按 SSE 规范处理：`\n` / `\r\n` / `\r` 行结束，`event:`、`data:`（多行拼接）、`id:`、`retry:`，以 `:` 开头的注释行，空行分隔事件；跨网络分片的半行保留到下一块；UTF-8 多字节序列被切开时保留到下一块再解码。
- 只有进入转换路径的响应才经过解析器；透传路径不解析（保持逐字节透传）。

### 6.2 各协议解码

| 源协议 | 流的形态 | 规范化要点 |
|---|---|---|
| Anthropic | `message_start` / `content_block_*` / `message_delta` / `message_stop` / `ping` / `error` | 已有块边界；`ping` 丢弃；`signature_delta` → `SignatureDelta`；`redacted_thinking` 的 `content_block_start.data` → `RedactedThinking` 块；`message_delta.usage` 与 `message_start.usage` 合并为 `Finish.usage` |
| OpenAI Chat | `data: {chunk}`，以 `data: [DONE]` 结束 | 由 `delta.content` / `delta.reasoning_content`（或 `delta.reasoning`）/ `delta.tool_calls[i]` 推断块开始与切换；**`reasoning_content: ""` 不开块**（`streaming.rs:269-275`）；tool_calls 按 `index` 归属，**`arguments` 可能先于 `id` / `name` 到达**，块在拿到 name 前先缓冲（`streaming.rs:170-171`、测试 `:883-890`）；`finish_reason` **不立即产生 `Finish`**：usage chunk 通常在 finish_reason 之后、`choices: []`，且部分上游重复发 finish_reason（`streaming.rs:160-165`、测试 `:997-1060`），故 `Finish` 在 `[DONE]` 或流结束时发出并合并最后的 usage；没有 `[DONE]` 而流正常结束也发 `Finish`；既无 finish_reason 也无 `[DONE]` 就结束 → `Error` |
| OpenAI Responses | `response.created` / `response.output_item.added` / `response.output_text.delta` / `response.function_call_arguments.delta` / `response.reasoning_summary_text.delta` / `response.reasoning_text.delta`（原文推理，与 summary 同样进 `ThinkingDelta`） / `response.custom_tool_call_input.delta` / `response.output_item.done` / `response.completed` / `response.incomplete` / `response.failed` / `error`；`content_part.*`、`*.done`、`reasoning_summary_part.*` 只用于校验、不产生事件 | 以 output item 为块；`output_item.done` 的 `reasoning` 项带 `encrypted_content` → `SignatureDelta`；`function_call` 的 `output_item.done` 带完整 `arguments`，与已收到的 delta 不一致时以 done 为准；`response.completed` / `response.incomplete` → `Finish`；`response.failed` / `error` → `Error` |
| Gemini | 每个 `data:` 是一个 `GenerateContentResponse` 片段 | 按 part 类型推断块；文本 part 可能是**增量**也可能是**累计快照**（部分网关），用"新文本以已累计文本为前缀"判定并只发增量（`streaming_gemini.rs:356-364`）；`functionCall` 一次给出完整参数（一条 `ToolArgumentsDelta`），同一调用可能在后续片段重复出现（按 id 或位置去重，`streaming_gemini.rs:100-135`）；`thoughtSignature` 可能只出现在某个片段（`streaming_gemini.rs:170-180`）；最后一个带 `finishReason` 的片段 → `Finish`（stop reason 规则同 §5.6：`STOP` + functionCall ⇒ `ToolUse`）；`promptFeedback.blockReason` → `Refusal` |

### 6.3 各协议编码

- Anthropic：`message_start`（usage 只有输入侧时 `output_tokens: 0`）→ `content_block_start` / `content_block_delta` / `content_block_stop` → `message_delta{stop_reason, usage}` → `message_stop`。`message_delta.usage` 写**完整** usage（`input_tokens`、`output_tokens`、缓存桶），因为 `message_start` 时 Chat / Gemini 还没有输入 token 数（`streaming.rs:104-131`）。tool_use 的 `content_block_start` 带 `input: {}`，参数走 `input_json_delta`。
- OpenAI Responses：事件清单（与现有 `streaming_codex_anthropic.rs` / `codex_responses_sse.rs` 一致）：
  - 开头 `response.created` → `response.in_progress`；
  - Text 块：`output_item.added`（`message`，`content: []`）→ `content_part.added`（`output_text`）→ `output_text.delta`… → `output_text.done` → `content_part.done` → `output_item.done`；
  - Thinking / RedactedThinking 块：`output_item.added`（`reasoning`，`summary: []`）→ `reasoning_summary_part.added` → `reasoning_summary_text.delta`… → `reasoning_summary_text.done` → `reasoning_summary_part.done` → `output_item.done`（此时带 `encrypted_content` 封套；RedactedThinking 不发 summary 事件）。Codex 依赖 `reasoning_summary_part.added` 分段；
  - ToolCall 块：`output_item.added`（`function_call`，`arguments: ""`）→ `function_call_arguments.delta`… → `function_call_arguments.done` → `output_item.done`；名字在 `custom_tool_names` 内时改为 `custom_tool_call` 项与 `custom_tool_call_input.delta / done`（§5.2）；
  - 结尾 `response.completed`（`response.output` 含全部完整项与 usage）；`Refusal` / `MaxTokens` 用 `response.incomplete`（带 `incomplete_details.reason`）。
  - 每帧带递增的 `sequence_number`，`output_index` 为块序号，每个 item 有 `id`（`msg_` / `fc_` / `rs_` / `ctc_` 前缀 + 随机）。
- OpenAI Chat、Gemini：本期不作为客户端协议，编码器推迟（§1.2）；IR 事件模型已覆盖其需要（Chat 的 `tool_calls[index]` 首帧带 id / name、`Finish` 后发 usage chunk 与 `[DONE]`；Gemini 每个增量一个片段、`Finish` 带 `finishReason` 与 `usageMetadata`）。

### 6.4 异常流

- 上游流在 `Finish` 之前结束：编码器补发目标协议的错误事件（Anthropic `error`、Responses `response.failed`），并以错误结束 body，使客户端连接中断（与透传 core 的提交点语义一致）。
- 上游 `Error` 事件：编码为目标协议的错误事件。
- 空闲超时、访问日志沿用 core 现有包装，包装顺序与 `ConvertingBody` 的要求见 §8.4。

## 7. 非流式与错误响应

- 上游 2xx：读完整 body（受 `idle_timeout` 约束，且以 `server.max_body_bytes` 封顶，超出返回 502 `relay_conversion_error`）解析为 `ir::Response`，生成客户端协议 JSON；`content-type: application/json`，`content-length` 由新 body 决定。
- 上游非 2xx（**含不可重试的状态码**）：先以 `failover.retry_body_bytes` 有界缓冲 body（超出 → 502 `relay_conversion_error`），状态码保持，body 尽力解析为错误（`{error:{type,message}}` / Gemini `{error:{code,status,message}}`），按客户端协议格式重写（Anthropic `{type:"error", error:{type, message}}`；Responses `{error:{type, message, code}}`）；解析失败时把原始 body 作为 message 文本包装。转换上游的非 2xx 响应**不走流式透传**（§8.3）。
- `retry-after` 等语义头保留；上游的 `content-length`、`content-type`、`content-encoding` 与协议专有头（`anthropic-*`、`openai-*`、`x-goog-*`）不转发给客户端，由转换结果重新生成。

## 8. 与 core 的集成

### 8.1 钩子

```rust
// cc-proxy-core::convert
pub trait Converter: Send + Sync {
    /// 构造期检查：该方向是否可转换（配置校验用，§3.2）
    fn supports(&self, client: Interface, upstream: Interface) -> bool;
    /// 把客户端协议的请求转为上游协议；每次尝试调用一次（不同上游可能协议不同）。
    fn convert_request(&self, ctx: &ConversionContext, request: &InboundRequest)
        -> Result<OutboundRequest, ConvertError>;
    /// 为一次最终响应创建响应转换器（每个请求至多一个）。
    fn response_converter(&self, ctx: &ConversionContext, outbound: &OutboundMeta)
        -> Box<dyn ResponseConverter>;
}

pub struct ConversionContext<'a> {
    pub client: Interface,               // 客户端协议（接口）
    pub upstream: Interface,             // 上游协议
    pub model_map: &'a ModelMap,
    pub default_max_output_tokens: u64,  // §3.1，客户端未带输出上限时使用
}

pub struct InboundRequest<'a> { pub method: &'a Method, pub path: &'a str, pub query: Option<&'a str>, pub headers: &'a HeaderMap, pub body: &'a Bytes }

pub struct OutboundRequest { pub method: Method, pub path_and_query: String, pub headers: HeaderMap, pub body: Bytes, pub meta: OutboundMeta }

/// 响应转换需要的请求侧信息（由 convert_request 填好）
pub struct OutboundMeta {
    pub stream: bool,
    pub client_model: String,            // 回填到客户端响应的 model 字段
    pub upstream_model: String,
    pub tool_names: Vec<String>,         // Gemini 等需要按名字回填时使用
    pub custom_tool_names: Vec<String>,  // Responses `type: custom` 工具名，响应时还原为 custom_tool_call（§5.2）
    pub hosted_web_search: Option<String>, // 请求中被映射的 server tool 名，响应映射回去时用
}

pub trait ResponseConverter: Send {
    fn convert_head(&mut self, status: StatusCode, headers: &HeaderMap) -> HeaderMap;
    /// 流式：喂入上游 body 字节，返回要发给客户端的字节（可为空）
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ConvertError>;
    /// 上游 body 结束（含错误结束：`error` 为 Some 时补发错误帧）
    fn finish(&mut self, error: Option<&dyn std::error::Error>) -> Vec<Bytes>;
    /// 非流式：整段上游 body → 整段客户端 body
    fn convert_full(&mut self, status: StatusCode, body: &[u8]) -> Result<Bytes, ConvertError>;
}

pub enum ConvertError {
    /// 目标协议无法表达（§5.4 的 400 类）：跳过该上游，不计熔断
    Unsupported(String),
    /// 转换器自身错误：立即 500，不再尝试
    Internal(String),
    /// 响应阶段：上游 body 不可解析 / 压缩 / 2xx 内的失败
    Response(String),
}
```

- `ConversionContext.stream` 已移除：以 `&` 传入无法回填，改由 `OutboundMeta.stream` 携带。`ResponseConverter::convert_full` 同时用于 2xx 非流式响应与所有非 2xx 错误响应的重写（§7）。
- core 的 `ResolvedUpstream` 增加 `protocol: Interface`、`model_map: ModelMap`、`default_max_output_tokens: u64`、`converts: bool`（`protocol ≠ interface || !model_map.is_empty()`）；`Relay::new(config)` 保持不变（无转换器，配置中有需转换的上游则报错），新增 `Relay::with_converter(config, Arc<dyn Converter>)`。`ConversionContext` 增加 `default_max_output_tokens`。

### 8.2 请求

- 转换请求的头：以入站头为基础，按透传规则（hop-by-hop、`expect`、入站凭据）处理后，再由转换器调整：删除 `accept-encoding`（上游必须返回未压缩内容，否则无法解析）、`content-length`（重算）与源协议专有头（`anthropic-version`、`anthropic-beta`、`x-stainless-*`、`openai-*`、`x-goog-*` 中不属于目标协议的）；补充目标协议必需头（目标为 Anthropic 时缺省 `anthropic-version: 2023-06-01`；目标为 Gemini 且流式时 `accept: text/event-stream` 可选）。上游鉴权按上游协议写入（§3.1）。
- `preserve_header_case`：入站 `extensions` 里的 `HeaderCaseMap` 只覆盖从入站复制的头；转换器新增或改写的头不在其中，按小写发出。恒等转换与协议转换都如此，文档与 `check` 输出注明。
- path：由转换器按目标协议生成（Chat `/v1/chat/completions`、Responses `/v1/responses`、Anthropic `/v1/messages`、Gemini `/v1beta/models/{upstream_model}:{generateContent|streamGenerateContent}?alt=sse`），再经 core 的 `base_url` / `strip_prefix` 拼接。
- 同一请求中，转换结果在尝试间按 **`(上游协议, model_map 映射结果)`** 复用；只按协议复用会让不同 `model_map` 的上游拿到错误的模型名。

### 8.3 故障转移

- 每次尝试按该上游的 `converts` 决定透传或转换；可重试判定、熔断不变。`converts == false` 的分支**原样调用现有的 `build_upstream_request` / `relay_response`**，不经过任何转换代码。
- `convert_request` 返回 `Unsupported`：记 warn、**释放该上游的熔断许可（`AttemptGuard` drop，不计成功也不计失败）、继续下一个上游**（同接口可混合透传上游，它们可能能服务这个请求）。`Unsupported` **不计入 `attempts`、不调用 `log.start_attempt`**：现状在构造请求前就 `attempts += 1`（`forward.rs:161`），`max_attempts = 1` 时跳过后将无法转到透传上游，故转换调用要移到计数之前。全部上游都跳过或失败且没有可回放的响应时，返回 400 `relay_conversion_unsupported`，message 取第一个 `Unsupported` 的原因；有其他失败时按现有优先级（回放 > 502 > 503）。
- `convert_request` 返回 `Internal`：立即 500 `relay_conversion_error`。
- 转换上游的非 2xx 响应（无论是否可重试，包括现状直接 `relay_response` 的两处：不可重试状态 `forward.rs:217-221`、"最后一次尝试"`:238-241`）一律经 `buffer_response` 有界缓冲，**缓冲时即调用 `convert_full` 重写为客户端协议**，结果直接放入 `LastFailure.response`（可重试）或直接返回（不可重试）。这样 `LastFailure` 无需记录上游协议，回放逻辑不变；缓冲超限按现状记 `last.error`。

### 8.4 响应体

- 转换响应体：`LoggedBody<ConvertingBody<IdleTimeoutBody<Incoming>>>`：空闲超时在最内层监视上游；`ConvertingBody` 在 `relay_response` 内、`IdleTimeoutBody` 外构造，在 `poll_frame` 中调用 `feed` / `finish`；访问日志由 `AccessLog::stream` 在最外层包装（现状即如此），统计发给客户端的字节。
- `ConvertingBody` 的 `size_hint()` 返回 `SizeHint::default()`（不能转发内层的 `content-length`，输出长度已变）；`is_end_stream()` 恒返回 false，直到自身输出缓冲排空且内层已返回 `None`：`LoggedBody` 读到最后一帧后用 `inner.is_end_stream()` 判定结束（`access_log.rs:189-195`），hyper 也据此决定是否继续 poll；`finish()` 补发的帧否则会丢。内层的 trailers 帧丢弃。
- 内层错误（空闲超时、上游断连）与 `feed` 返回 `Err`（流不可解析）：`ConvertingBody` 先把 `finish(Some(err))` 产生的错误帧作为数据帧发出，再返回 `Err`，与 §6.4 一致；`LoggedBody` 随之记 `outcome = error`。
- 转换响应的头：删除上游 `content-length`、`content-encoding`，`content-type` 由转换器重写（§7）。
- 上游响应带 `content-encoding`（非 identity）时判定为转换失败：非流式返回 502 `relay_conversion_error`，流式按 §6.4 结束。
- 访问日志新增字段 `convert`（如 `claude->openai_chat`、恒等转换 `claude->claude`，透传时为 `-`）；其余字段（`attempts`、`upstream`、`status`）语义不变。
- `preserve_header_case`：转换请求仍附带入站 `Extensions`（含 `HeaderCaseMap`）；hyper 写头时对不在映射中的名字回退为小写，转换器新增或改写的头因此按小写发出，无需额外处理；响应侧同理。

## 9. 特殊端点

| 端点 | 处理 |
|---|---|
| `POST /v1/messages/count_tokens` → 非 Anthropic 上游 | `Unsupported`（按 §8.3 跳到下一个上游；没有可用上游则 400） |
| `GET /v1/models`、`GET /v1/models/{id}` | 本期不做列表格式转换（§1.2）。转换上游收到这类请求时返回 `Unsupported`（跳过），让同接口的透传上游服务；同协议恒等转换上游按透传处理 |
| `GET/DELETE /v1/responses/{id}`、`POST /v1/responses/compact` → 非 Responses 上游 | `Unsupported` |
| Gemini `:countTokens`、`:embedContent`、`:batchEmbedContents` | Gemini 不是本期客户端协议，不涉及 |

## 10. 测试

| 编号 | 验收 |
|---|---|
| X1 | IR 单元测试：每种协议的请求 / 非流式响应 / 流式事件解析与生成（本期实现的方向），覆盖文本、多模态、多个工具调用与结果、tool_result 内图片、thinking / redacted_thinking、每个 stop reason、usage 归一（三桶恒等） |
| X2 | 往返测试：协议 A → IR → A 在 **IR 覆盖字段**上等价（字段集合与值相同，忽略键序；明确列出不参与比较的丢弃字段），对 Anthropic 与 Responses 各一组 |
| X3 | 流式分片鲁棒性：同一段上游 SSE 以任意字节边界切分（含把 `\r\n` 与多字节 UTF-8 切开），输出事件序列相同 |
| X4 | 录制回放：各上游协议真实流样本（仓库测试夹具，脱敏）经转换后，用桌面端现有解码器的语义（`streaming.rs` / `streaming_gemini.rs` / `streaming_responses.rs` 的测试夹具与断言）作对照，客户端侧能完整重建文本、工具调用与 usage |
| X5 | 集成：mock 上游说协议 B，客户端以协议 A 请求，流式与非流式各一组，覆盖本期全部 8 个方向 |
| X6 | 有损策略：含 `web_search_20250305` 的 Claude 请求对 Chat / Gemini 上游被剔除工具后成功、对 Responses 上游映射为内置工具；Codex 默认字段（`store: false`、`include`、`prompt_cache_key`、`reasoning.summary`）被忽略且成功；`previous_response_id` / `n>1` 返回 400 且上游未收到请求 |
| X7 | 故障转移：转换上游失败后换透传上游，第二个上游收到的是**原始**请求；`Unsupported` 跳过第一个上游、第二个透传上游成功、熔断计数不变、`attempts` 不含被跳过的上游（`max_attempts = 1` 时仍能转到第二个）；两个上游 `model_map` 不同时各自收到正确模型名；转换上游返回不可重试 4xx 与可重试 5xx 时客户端都收到按客户端协议重写的错误体 |
| X8 | 透传回归：（a）`Relay::new` 无转换器、未配置转换时透传 core 全部 T1–T18 通过；（b）注入一个**任何方法被调用即 panic** 的测试转换器、配置中只有透传上游，再跑一遍全部 T1–T18。两者共同证明透传路径不经过任何转换代码 |
| X9 | 压缩：转换路径上游请求不带 `accept-encoding`；上游仍返回压缩内容时按 §8.4 报错 |
| X10 | Anthropic 合法性规范化（§5.2）：预算钳位、thinking 与 temperature / 强制 tool_choice 互斥、空文本 / 空消息 / 首条非 user / 尾随空白、tool_result 顺序、未完成工具轮 |
| X11 | 签名承载（§5.5）：Anthropic 上游 → Codex 客户端 → 再回 Anthropic 上游，thinking 块原样回放；Responses 上游 → Claude 客户端 → 再回 Responses，reasoning 项原样（请求带 `store: false` 与 `include`）；Gemini 上游 → 两种客户端 → 回 Gemini，`thoughtSignature` 附回 functionCall、文本签名附到末尾 part；封套版本 / 结构错误、跨协议回放时降级为关 thinking 而不是 400；Chat 上游产生的无签名 thinking 再发往 Anthropic 时整块丢弃 |
| X12 | 流式边界（§6.2）：Chat usage chunk 在 finish_reason 之后、重复 finish_reason、arguments 先于 id、`reasoning_content: ""`；Gemini 累计快照、functionCall 重复片段、签名只在某片段；`ConvertingBody` 在内层结束后仍能发出 `finish()` 的帧，`size_hint` 未知，内层 trailers 被丢弃，`feed` 出错时先发错误帧再 `Err` |
| X13 | 恒等转换：同协议 + `model_map` 只改模型名，其余 IR 覆盖字段不变（含 `thinking: {type: adaptive}` + `output_config` 原样回放）；未配置 `model_map` 时仍走透传（X8） |
| X14 | Codex 常见请求形态：`apply_patch` custom 工具 + `reasoning` + 并行调用的多轮对话分别对 Anthropic / Chat / Gemini 上游成功，响应中的 `apply_patch` 调用还原为 `custom_tool_call`；未带 `max_output_tokens` 时上游收到 `default_max_output_tokens` |

## 11. 分阶段实施

每个阶段结束时工作区可独立编译、全部测试通过；未实现的方向由 `Converter::supports` 返回 false，在配置校验阶段拒绝，不会在运行时暴露半成品。代码量估计含测试。

| 步骤 | 内容 | 估计 | 验收 |
|---|---|---|---|
| C1 | core 集成，子步骤见下 | ~0.9k 行 | X7（恒等版）、X8、X12 的 `ConvertingBody` 项、X13 的透传项 |
| C2 | `cc-proxy-convert` crate 基础：IR 类型、SSE 解析器、签名封套（仅 `ccsw1`）、model_map、Anthropic Messages 编解码（请求解析 / 生成、响应生成 / 解析、流式解码 / 编码，含 §5.2 规范化与 adaptive thinking）、OpenAI Chat 上游侧编解码（请求生成、响应与流式解析）、错误响应转换；**`cc-proxy` 在本步注入转换器**（`Relay::with_converter`），示例配置加一个 `claude→openai_chat` 上游，便于用真实 Claude Code 端到端验证。方向：`claude→claude`（恒等）、`claude→openai_chat` | ~3.5k 行 | X1–X3（两种协议）、X5（两个方向）、X6 的 Claude 部分、X10、X12 的 Chat 项、X13，真实客户端手动验证 |
| C3 | OpenAI Responses 编解码（客户端侧请求解析与响应 / 流式编码含 §6.3 完整事件清单与 custom 工具还原，上游侧请求生成含 `store: false` / `include` 与响应 / 流式解码）、Responses ↔ Anthropic 的签名封套、server tool ↔ `web_search` 映射。方向：`openai_responses→openai_responses`（恒等）、`openai_responses→claude`、`openai_responses→openai_chat`、`claude→openai_responses` | ~4k 行以上（现有对应代码含测试约 15k 行，IR 化后仍是最大的一步） | X1–X3、X5（四个方向）、X6 的 Codex 部分、X11（Anthropic / Responses 互回放）、X14（Anthropic / Chat） |
| C4 | Gemini 上游侧编解码（请求生成含 path 中的模型与流式标志、schema 子集判定、调用 id 生成与函数名回填、签名落点、响应与流式解码含累计快照与签名）、Gemini 签名封套。方向：`claude→gemini`、`openai_responses→gemini` | ~2.5k 行 | X1–X3、X5（两个方向）、X11（Gemini 回放）、X12 的 Gemini 项、X14（Gemini） |
| C5 | 特殊端点（§9）、使用说明（恒等转换的代价、`default_max_output_tokens`、Gemini 2.5 Pro 不可关 thinking、Gemini 3 缺签名限制）、录制夹具 | ~0.6k 行 | X4、X6、X9，端到端 |

合计约 11.5k 行。

**C1 子步骤**（每一步结束时全部测试通过）：

1. 透传保证先行：`converts == false` 分支原样调用现有 `build_upstream_request` / `relay_response`；加入一个"任何方法被调用即 panic"的测试转换器，`Relay::with_converter` 注入后跑全量 T1–T18，与 `Relay::new` 无转换器路径一起构成 X8。
2. 配置：`protocol` / `model_map` / `default_max_output_tokens` 字段与 `RelayConfig::validate` 校验（协议取值、接口限制、`model_map` 空键 / 重复 `*`、`default_max_output_tokens > 0`）；`Converter::supports` 检查放在 `Relay::with_converter`；`auth` 缺省改为 `AuthScheme::default_for(protocol)`，`protocol` 缺省等于接口时现有 `auth_scheme_defaults_and_override` 测试不变。
3. 尝试循环：转换调用移到 `attempts += 1` / `start_attempt` 之前，`Unsupported` 释放 `AttemptGuard`、不计 attempts；`Internal` 立即 500；转换结果按 `(上游协议, 映射结果)` 复用；转换上游的所有非 2xx（含不可重试）走 `buffer_response` 有界缓冲 → `convert_full` 重写，不走流式透传。
4. `ConvertingBody`：放在 `relay_response` 内、`IdleTimeoutBody` 外；`size_hint` 返回 `SizeHint::default()`；`is_end_stream` 恒 false 直到输出缓冲排空且内层 `None`；丢弃内层 trailers；内层 `Err` 或 `feed` 返回 `Err` 时先吐 `finish(Some(err))` 的帧再返回 `Err`；响应头删 `content-length` / `content-encoding`。
5. `preserve_header_case`：转换请求仍附入站 `Extensions`，不做额外处理（§8.4）。
6. 访问日志加 `convert` 字段（透传 `-`）；恒等转换器测试同时断言 `attempts`、`upstream`、`status` 不变。
7. `check` 输出：每个转换上游的方向、`default_max_output_tokens`，以及恒等转换"不再逐字节透传"的提示。

## 12. 风险

| 风险 | 应对 |
|---|---|
| 各厂商对同一协议的实现差异（Chat 的 `reasoning_content`、`enable_thinking`、工具 schema 限制） | 本期只按官方协议实现，解析侧对已知别名宽容（§5.6）；厂商方言作为后续可选配置（§1.2） |
| 流式状态机的边界情况（并行工具调用、推理与文本交错、空块、累计快照） | §6.2 列出的已知情况进入 X12；X3 分片测试 + X4 回放测试；解码器规范化后编码器只处理规范事件 |
| 签名跨协议无法回放 | §5.5 的封套承载；不可用时降级关 thinking 并记 warn，不 400 |
| Gemini 3 缺 `thoughtSignature` 时拒绝请求 | 封套承载覆盖本中转产生的会话；客户端从别处带来的历史无签名时可能被 Gemini 拒绝，作为已知限制写入使用说明。Gemini 官方为"注入的历史 functionCall"提供占位签名 `skip_thought_signature_validator`（写在缺签名的 `functionCall` part 上以跳过校验），可作为后续上游级可选项（如 `gemini_fill_missing_signature`）；本期不做 |
| Gemini 2.5 Pro 不能关闭 thinking | `Disabled` 生成 `thinkingBudget: 0` 会被该模型 400；写入使用说明，后续方言配置可改为最小值 |
| 厂商对 Chat `max_tokens` / `max_completion_tokens` 的取舍 | 本期固定 `max_tokens`；OpenAI o 系模型需要 `max_completion_tokens`，作为方言选项后续处理（§1.2） |
| 客户端把封套带到别的透传上游 | 恒等 / 透传上游收到 `ccsw1.*` 值会被上游拒绝（签名无效）。使用说明建议同一接口下不要混用"转换上游 + 同协议透传上游"承接带 thinking 的会话；后续可考虑在透传路径识别并剥离封套（会破坏透传承诺，本期不做） |
| 恒等转换改变了"透传"承诺 | 只在显式配置 `model_map` 时发生；`check` 输出与文档明确其代价；X8 守护透传路径 |
| 协议版本演进 | 解析时忽略未知字段；生成时只输出已建模字段 |

## 13. 修订记录

**v3（已按 Fable 二次审查修订）**

- §1.2：Chat / Responses 的文档块、其他 server tools 历史降级、HMAC 封套（`ccsw1h`）移到后续项；`max_completion_tokens` 列为方言选项。
- §3：新增上游级 `default_max_output_tokens`（缺省 16384，`check` 输出）；校验位置明确（`validate` 与 `with_converter` 分工）。
- §4.1 / §5.1：`Reasoning` 增加 `Adaptive(Option<Effort>)`，建模 Claude Code 的 `thinking: {type: adaptive}` + `output_config`，恒等转换原样回放；Chat 改为默认 `max_tokens`；Responses 上游固定 `store: false` 与 `include: ["reasoning.encrypted_content"]`；Gemini 2.5 Pro 不接受 `thinkingBudget: 0` 的说明。
- §5.2：新增第 8 条"无同源签名的 thinking 整块丢弃"；custom 工具补响应方向（`OutboundMeta.custom_tool_names`、`custom_tool_call` 还原）；Gemini schema 子集判定按代码修正（`anyOf` 可留在 `parameters`）；修正 `transform_codex_anthropic.rs` 的几处行号。
- §5.4：其他 server tools 历史块改为丢弃；文档块仅 Anthropic ↔ Gemini。
- §5.5：文本型 Gemini 签名的解析归属与生成落点；HMAC 改为只保留版本位。
- §5.6：注明 `content_filter → Refusal` 与现有代码的有意差别。
- §6.2 / §6.3：Responses 解码补 `reasoning_text.delta` / `custom_tool_call_input.delta`；编码给出完整事件清单（`content_part.*`、`*.done`、`reasoning_summary_part.*`、custom 工具事件）。
- §7 / §8.3 / §8.4：非流式 2xx 以 `max_body_bytes` 封顶；转换上游的所有非 2xx 有界缓冲后 `convert_full` 重写，`LastFailure` 不再需要记录上游协议；`Unsupported` 不计 attempts；`ConvertingBody` 的 trailers、`feed` 出错、响应头处理；`preserve_header_case` 的说明。
- §10：X7 / X8 / X11 / X12 / X13 更新，新增 X14（Codex 常见请求形态）。
- §11：转换器注入提前到 C2；C3 估计上调到 4k 行以上；C1 拆为 7 个子步骤；合计约 11.5k 行。
- §12：补 `skip_thought_signature_validator` 可选项、Gemini 2.5 Pro 关 thinking、Chat `max_tokens` 取舍三项。

**v2（已按 Fable 审查修订）**

- §1.1/§1.2：本期客户端协议限定为 `claude` 与 `openai_responses`，上游四种任意；Gemini / Chat 作客户端、`/v1/models` 转换、`response_format → responseSchema` 列为后续项。IR 与 trait 仍按四种协议设计。
- §3：允许"同协议 + `model_map`"走恒等转换并说明其不再逐字节透传；`auth` 缺省改按上游协议取，标注 C1 需改 `upstream.rs:250`；补充校验规则（构造期 `supports` 检查）。
- §4：`ToolCall` / `Thinking` 增加 `signature`，新增 `RedactedThinking` 块；Gemini 调用 id 改为全局唯一；usage 归一改为四协议逐项表并要求三桶恒等；`Reasoning` 增加 `Disabled` 与 `minimal` / `xhigh` 档。
- §5.1：`max_output_tokens` 补 Responses ≥ 16 钳位；effort ↔ budget 档位与现有代码统一。
- §5.2（新）：Anthropic 请求合法性规范化七条；工具 schema 清洗；Responses `input[]` 项完整清单（含 custom 工具降级）；Gemini `functionResponse` 结构。
- §5.3（新）：多个工具结果与工具结果内媒体在各协议的拆分与顺序。
- §5.4：server tools / Responses 内置工具改为映射或剔除，不再 400；Codex 默认字段明确为静默忽略；400 的判定标准改为"无法表达且降级会改变结果"。
- §5.5（新）：签名承载规则、降级策略、版本化封套格式与防伪取舍。
- §5.6：stop reason 表补 Gemini `STOP` + functionCall、Chat `stop` + tool_calls、Responses 2xx 内失败；响应 `model` / `id` 来源。
- §6.2/§6.3：补 Chat usage chunk 与重复 finish_reason、arguments 先于 id、空 `reasoning_content`、Gemini 累计快照与重复 functionCall、`message_delta` 完整 usage、Responses `sequence_number` / item id / `response.incomplete`。
- §8：trait 增加 `supports` 与 `OutboundMeta`，去掉不可回填的 `ConversionContext.stream`；`Unsupported` 改为跳过上游而非终止；转换结果按 `(协议, 映射结果)` 复用；`LastFailure` 记录上游协议；`ConvertingBody` 的 `size_hint` / `is_end_stream` 规则与 `LoggedBody` 的结束判定对齐；`preserve_header_case` 下新增头按小写发出。
- §9：`/v1/models` 与 `count_tokens` 改为 `Unsupported` 跳过，让同接口透传上游服务。
- §10/§11：验收项重写为 X1–X13，分阶段计划按 8 个方向重排并给出代码量估计。
- §12：补 Gemini 3 缺签名、封套流入透传上游两项风险。
