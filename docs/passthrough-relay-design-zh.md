# 透传中转 core 设计方案

> 状态：草案 v3.1（已按 R1/R2 实施结果同步）· 基线：`refactor/standalone-proxy-core@5e6abca5f`
> 实现位置：`src-tauri/crates/cc-proxy-core/`（R1、R2 已实施；R3–R5 待做，见 §12）。
> 取代：`docs/standalone-proxy-core-design-zh.md` 中 P2 及之后"搬迁现有代理"的路线。该文档的调研结论、P0（workspace 骨架）与已完成的 P1 前两步仍然有效。
> 库行为引用的源码版本以 `src-tauri/Cargo.lock` 锁定版本为准：hyper 1.8.1、hyper-util 0.1.20、hyper-rustls 0.27.7、rustls 0.23.37、axum 0.7.9（§9）。
> v3 中标注"原型实测"的结论来自 `/tmp/relay-proto`（锁定版本搭建的最小原型：自建 http1 accept loop + axum `fallback` + legacy Client + 原始 socket mock 上游）。R2 实际实现改用 hyper `service_fn`、不引入 axum（§9），原型中与 axum 无关的结论仍然有效，并已由集成测试重新验证。

## 1. 目标与原则

实现一个独立的模型接口中转：客户端把 Claude Code、Codex、Gemini CLI 等工具指向中转，中转把请求**原样**转发给配置的上游，再把响应**原样**流回客户端。

已确认的决策（2026-09-29）：

| 决策 | 结论 |
|---|---|
| 接口范围 | 四个：Anthropic Messages、OpenAI Chat Completions、OpenAI Responses、Gemini generateContent / streamGenerateContent |
| 凭据 | 客户端用中转自己的 token 鉴权；中转只把鉴权头替换成配置的上游 Key |
| 路由与故障转移 | 属于 core：按接口选上游，失败时切到下一个上游，但每次都原样重发同一请求 |
| 与现有代理的关系 | 新写透传 core，桌面端现有代理（`src-tauri/src/proxy`）不动 |
| HTTP 版本 | 本期入站、出站都只用 HTTP/1.1；HTTP/2 列为后续（§2.3） |

核心原则：**core 不修改任何模型语义内容。** 请求 body、响应 body、SSE 流逐字节不变；需要改写（协议互转、模型映射、thinking 整流、缓存注入等）时，由独立的改写组件通过 §7 的钩子接入。core 默认不安装任何改写组件。

## 2. 透传边界：哪些字节可以变

### 2.1 "逐字节相同"的定义域

- **body**：去掉 HTTP/1.1 分帧（chunked 的 chunk 边界、chunk-size 行）之后的字节序列。chunk 边界与 TCP 分片边界不保证一致，这是传输层的事，不属于内容。
- **头**：头名（含原始大小写，§2.4）、头值、重复头的条数与同名头之间的相对顺序。不同名头之间的相对顺序按各自**首次出现**的位置保持；交错出现的同名头会被聚合到首次出现的位置（`http::HeaderMap` 的固有行为，§2.4）。例：入站 `Anthropic-Beta: one` / `User-Agent: ua` / `anthropic-beta: two`，上游收到 `Anthropic-Beta: one` / `anthropic-beta: two` / `User-Agent: ua`（原型实测）。这是本设计对"顺序"的定义，T2 按此断言。
- **起始行**：method、path、query（除 §6 移除的 `key`）、状态码、reason phrase 不保证（hyper 只按状态码生成标准 reason）。

### 2.2 允许变化的字节

除下表外一律不变。

| 对象 | 处理 | 原因 / 依据 |
|---|---|---|
| 请求方法、path、query | 原样。仅 Gemini 的 `key` query 参数（入站鉴权）被移除，移除方式见 §6.3（字节级过滤，其它参数不重新编码） | query 中的 `key` 是凭据，不能转发给上游 |
| 请求 body | 逐字节原样 | — |
| 入站鉴权头 `authorization`、`x-api-key`、`x-goog-api-key` | 无论鉴权是否通过，全部移除（含非 Bearer scheme 的 `authorization`） | 中转自己的 token 不能泄露给上游 |
| 上游鉴权头 | 按上游配置写入（§5.3），每次尝试各自写入 | 唯一允许的"注入" |
| `host` | 移除入站 `host`，在**原位置**写入上游 host（含非默认端口）。core 自己写，不依赖 hyper-util 的 `set_host`（它只在缺失时补，`hyper-util-0.1.20/src/client/legacy/client.rs:298`） | HTTP 要求 |
| hop-by-hop 头：`connection`、`keep-alive`、`proxy-connection`、`proxy-authenticate`、`proxy-authorization`、`te`、`trailer`、`transfer-encoding`、`upgrade`，以及 `connection` 头值中列出的字段 | 请求、响应两个方向都移除 | RFC 9110 §7.6.1；`proxy-connection` 非标准但现有代理同样处理（`src-tauri/src/proxy/response_processor.rs:35-46`） |
| `expect` | 请求侧移除 | core 已完整缓冲 body，RFC 9110 §10.1.1 允许代理在能自行确定结果时不转发期望。hyper 服务端在 core 读 body 时会自动向客户端回 `100 Continue`（`hyper-1.8.1/src/proto/h1/conn.rs:311`）；hyper 客户端本身不等待 100（`proto/h1/role.rs:1148` 固定 `expect_continue: false`）并跳过上游的 1xx（`role.rs:1251`），转发该头只会让上游多发一个无人等待的 `100 Continue` |
| `content-length`（请求侧） | 客户端提供时原样；客户端以 `transfer-encoding: chunked` 上传时，core 缓冲后由 hyper 客户端补齐 `content-length`（`role.rs:1483 set_content_length`） | 分帧变化，body 不变 |
| `content-length`（响应侧） | 原样 | — |
| 其它请求头（`anthropic-version`、`anthropic-beta`、`openai-organization`、`openai-beta`、`chatgpt-account-id`、`session_id`、`originator`、`x-goog-api-client`、`user-agent`、`accept-encoding`、`x-forwarded-*`、`traceparent` 等） | 原样，包括现有代理会剥离的追踪/CDN 类头 | 透传优先；hyper 客户端不会自动增加 `accept-encoding` 或任何其它头 |
| `via` | **有意不添加** | RFC 9110 §7.6.3 要求代理发送 `Via`，本设计以透传优先，作为已知偏离记录在此 |
| 响应状态码、响应头 | 原样（除 hop-by-hop） | — |
| 响应 body / SSE | 逐字节原样，**不解压、不解析、不重新分块语义** | `content-encoding` 原样转发，客户端自行解压 |
| 响应 trailer | 丢弃 | `trailer` 属于 hop-by-hop；四个接口都不使用 trailer |

### 2.3 hyper 自动产生的分帧头（不属于内容）

以下由 hyper 在 HTTP/1.1 分帧层产生，T2/T3 的断言要按此排除：

| 方向 | hyper 行为 | 依据 |
|---|---|---|
| 客户端 ← 中转 | 响应缺少 `date` 时自动补 `date`；已有 `date` 则不动。上游响应都带 `date`，所以只影响中转自身响应（401/404/502/503 等） | `hyper-1.8.1/src/server/conn/http1.rs:246`（默认开启）、`proto/h1/role.rs:948`（仅缺失时写） |
| 客户端 ← 中转 | 无 `content-length` 的流式响应补 `transfer-encoding: chunked`；空 body 补 `content-length: 0` | `role.rs` Server `set_length` |
| 中转 → 上游 | `Full<Bytes>` body 且无 `content-length` 时补 `content-length` | `role.rs:1483` |
| 中转 → 上游 | 不自动加 `connection`、`accept-encoding`、`user-agent`；`host` 仅缺失时补（core 已先写） | `role.rs` Client 编码；`client.rs:298` |

本期只用 HTTP/1.1：入站用 `hyper::server::conn::http1`，出站 `hyper-rustls` 只开 `http1` feature、不做 ALPN。原因：与 app 现状一致（`src-tauri/Cargo.toml:61-63`，hyper-util / hyper-rustls 只开 `http1`），Claude Code / Gemini CLI（Node fetch）本来就是 HTTP/1.1，而 HTTP/2 下 hyper 会自动剥离 connection 类头与非 `trailers` 的 `te`（`hyper-1.8.1/src/proto/h2/mod.rs:43`），透传边界需要另行定义。后续启用 HTTP/2 时需要 `hyper-util`/`hyper-rustls` 的 `http2` feature 与 ALPN，并补 §2 的 h2 规则。

### 2.4 头名大小写与顺序

- **默认保留原始大小写**（`server.preserve_header_case = true`，可关闭）。实现只用 hyper 内建机制，不采用现有代理的 raw peek 方案：
  - 入站：`hyper::server::conn::http1::Builder::preserve_header_case(true)`（`server/conn/http1.rs:312`），hyper 把原始大小写存入请求 `extensions` 的 `HeaderCaseMap`（`proto/h1/role.rs:233`）。
  - 出站：把入站请求的 `extensions` 复制到上游请求，客户端 Builder 开 `http1_preserve_header_case(true)`（`hyper-util-0.1.20/src/client/legacy/client.rs:1275`），hyper 编码时按 `HeaderCaseMap` 还原（`role.rs:1200`）。
  - `HeaderCaseMap` 按**头名**索引、按位置配对拼写（`hyper-1.8.1/src/ext/mod.rs:161-199`，`write_headers_original_case`）。因此 core 移除后重写的头（`host`、上游鉴权头）只要入站出现过同名头，就沿用入站拼写：入站 `Host: relay.local` → core 写小写 `host` → 上游收到 `Host: …`；入站 `Authorization`/`X-Api-Key` 被移除、core 写小写 `authorization`/`x-api-key` → 上游收到 `Authorization: Bearer <上游 key>`、`X-Api-Key: …`（原型实测）。只有入站**不存在**同名头时才按 core 写入的小写发出。
  - 响应方向同理：客户端 `http1_preserve_header_case(true)` 记录上游响应头大小写，服务端编码时读响应 `extensions` 中的 `HeaderCaseMap` 回写（`role.rs:440`，不受服务端 `preserve_header_case` 开关影响）。hyper 自己补的分帧头也遵守这一规则：core 删掉上游的 `Transfer-Encoding` 后，hyper 补的 `transfer-encoding: chunked` 会写成 `Transfer-Encoding`（原型实测）。
  - core 不需要也不能引用 `HeaderCaseMap` 类型（`pub(crate)`），只做 `extensions` 的整体搬运（`std::mem::take` 入站 `extensions` 赋给出站请求；响应方向相同）。
- 关闭时统一小写（HTTP/1.1 头名大小写不敏感，RFC 9110 §5.1）。
- **顺序**：hyper 解析时按 wire 顺序 `append` 进 `HeaderMap`（`role.rs:326`），`HeaderMap` 迭代按首次插入顺序，同名多值聚在首次出现的位置（定义见 §2.1）。core 重建头时按入站迭代顺序逐个复制，`host` 在原位置替换，移除的头直接跳过，新增的上游鉴权头追加在末尾。

## 3. 接口识别

按 method + path + 少量只读头识别接口类型，path 和 query 原样拼到上游地址后面。识别函数签名固定为：

```rust
pub fn identify(method: &Method, path: &str, headers: &HeaderMap) -> Option<Interface>;
```

`headers` 只读，仅用于下表中标注"按头分流"的规则；识别不修改任何头，属于路由判断。匹配顺序自上而下，先匹配 `/_relay/*`（§8），再匹配下表：

| 接口 | 匹配规则 | 典型客户端 |
|---|---|---|
| `claude` | `POST /v1/messages`、`POST /v1/messages/count_tokens`；**按头分流**：`GET /v1/models`、`GET /v1/models/{id}`（`{id}` 不含 `:`）且请求带 `anthropic-version` 头 | Claude Code |
| `openai_chat` | `POST /v1/chat/completions` | 各类 OpenAI 兼容客户端 |
| `openai_responses` | `POST /v1/responses`、`/v1/responses/*`（含 `GET`、`DELETE` 与 `compact` 等子路径）；`GET /v1/models`、`GET /v1/models/{id}`（`{id}` 不含 `:`）且**不带** `anthropic-version` 头 | Codex CLI |
| `gemini` | `/v1beta/*`（任意 method）；**`POST`** `/v1/models/{name}:{action}`（path 在 `/v1/models/` 之后含 `:`，如 `:generateContent`、`:streamGenerateContent`、`:countTokens`） | Gemini CLI（API Key 模式） |

- `GET /v1/models` 两个客户端都会请求，path 相同，按 `anthropic-version` 头分流（实证见 §14 C1）：
  - Claude Code 2.1.260 在 `ANTHROPIC_BASE_URL` 指向非第一方 host、有凭据且设置了 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` 时，发 `GET {base}/v1/models?limit=1000`，头固定含 `anthropic-version: 2023-06-01`（以及 `Authorization: Bearer` / `x-api-key` / `User-Agent`）。非 2xx 只记日志、不影响会话。
  - Codex CLI 0.155.1 启动时探测 `GET /models`（现有代理 `src-tauri/src/proxy/handlers.rs:78-84`、`server.rs:323-324`），不发 `anthropic-version`。
  - 两者都透传给对应接口的上游，上游返回什么就给什么；不合成响应。
- Gemini GA 版 `/v1/models/...` 只在 **method 为 `POST` 且** path 含 `:` 时识别：OpenAI 微调模型 id 本身含冒号（如 `ft:gpt-4o-mini:org::id`），查询模型用 `GET`，而 Gemini 的 `generateContent`、`countTokens` 等动作全是 `POST`。因此 `GET /v1/models/ft:gpt-4o-mini:org::id` 按 `/v1/models/{id}` 规则分流（`interface.rs` 单测 `fine_tuned_model_id_with_colons_is_not_gemini`）。现有代理为避免冲突只把 `/v1beta/*` 无前缀暴露给 Gemini（`server.rs:397-400`）。
- `identify` 对 `/_relay/*` 总是返回 `None`；该前缀由 `Relay::handle` 在调用 `identify` 之前处理（§8）。
- **已实施（R2）**：入站服务不用 axum。`server.rs` 用 `hyper::service::service_fn` 直接调用 `Relay::handle`，所有路由判断在 `interface::identify` 内完成，core 不依赖 axum / tower（§9）。
- Codex CLI 还会调用 `/v1/alpha/search`（web search）、`/v1/images/generations`、`/v1/images/edits`（现有代理 `server.rs:359-392`）。**本期明确排除**，返回 404；以后如需纳入，作为 `openai_responses` 接口的附加路径加入。
- Gemini CLI 的 OAuth（Code Assist）模式走 `cloudcode-pa.googleapis.com/v1internal`，不属于四接口，不在范围内；只有 `GEMINI_API_KEY` 模式能指向中转。
- 不匹配的请求（含 `/`）返回 `404`，body 为 `{"error":{"type":"relay_not_found","message":"..."}}`。这是中转自身的响应，不属于透传内容。中转自身错误的 `type` 统一加 `relay_` 前缀以免与上游错误类型混淆，完整取值见 §5.4。
- 不做模型名解析、不按模型路由。

## 4. 上游与路由

每个接口配置一个**有序**上游列表。

```rust
pub struct Upstream {
    pub id: String,
    pub base_url: Url,                 // 例：https://api.anthropic.com、https://open.bigmodel.cn/api/anthropic、
                                       //     https://x.openai.azure.com/openai?api-version=2024-10-21
    pub api_key: Secret,               // config::Secret，Debug 不打印内容
    pub auth: Option<AuthScheme>,      // 缺省按接口取默认值，见 §5.3
    pub strip_prefix: Option<String>,  // 例："/v1"；用于 base_url 本身已含版本段的上游
    pub proxy_url: Option<String>,     // 覆盖 [server].proxy_url；空字符串表示直连；见 §5.6
}
```

以上为 `config::UpstreamConfig`（纯配置）；启动时经 `UpstreamSet::build` 校验并解析为 `upstream::ResolvedUpstream`（含已解析的 `Url`、生效的 `AuthScheme` 与共享的 `Arc<UpstreamClient>`）。

**URL 拼接**：

```
上游 URL = base_url.scheme://base_url.authority
        + (base_url.path 去掉末尾 "/") + (入站 path 去掉 strip_prefix)
        + query
query   = 合并(base_url.query, 过滤后的入站 query)   // 用 "&" 连接，base 在前；两者皆空则不带 "?"
```

- 例：`base_url = https://api.deepseek.com`，入站 `/v1/chat/completions` → `https://api.deepseek.com/v1/chat/completions`。
- 例：`base_url = https://ark.example.com/api/v3`，`strip_prefix = "/v1"`，入站 `/v1/chat/completions` → `https://ark.example.com/api/v3/chat/completions`。
- 例：`base_url = https://x.openai.azure.com/openai?api-version=2024-10-21`，入站 `/v1/chat/completions?x=1` → `https://x.openai.azure.com/openai/v1/chat/completions?api-version=2024-10-21&x=1`。
- `strip_prefix` 必须以 `/` 开头、不以 `/` 结尾；只有入站 path 等于 `strip_prefix` 或以 `strip_prefix + "/"` 开头时才剥离，否则原样。剥离在接口识别**之后**进行，识别始终用入站原始 path。
- `base_url` 不允许带 fragment；`check` 对 path 以 `/messages`、`/chat/completions`、`/responses`、`:generateContent`、`:streamGenerateContent` 结尾的 `base_url` 报错并提示"base_url 不应包含端点路径"（用户粘贴完整 URL 是现有代理反复处理的常见错误，`server.rs:519-566`）。`check` 同时对每个上游用该接口的典型 path 做一次 dry-run，打印最终 URL。
- `strip_prefix`、query 合并属于路由配置（选择上游地址），不改动 body 与语义，归入 core。

**选择顺序**：按列表顺序，跳过熔断器处于 open 的上游；全部 open 时，按顺序尝试第一个 half-open 可用的，仍无则返回 `503`（中转自身响应）。

## 5. 转发流程

### 5.1 请求

0. `/_relay/*` 前缀先于一切处理（§8）。
1. 入站鉴权（§6），失败返回 `401`（`relay_unauthorized`）；无论成败都移除入站凭据。
2. 识别接口（§3）；不匹配返回 `404`（`relay_not_found`）。识别在读 body **之前**，不匹配的请求不消耗 body（已实施顺序，与 v3 的"先读 body 再识别"不同）。
3. 读取完整请求 body 到内存（`http_body_util::Limited`，上限 `max_body_bytes`，默认 200 MiB，与 app 的 `DefaultBodyLimit` 对齐，`server.rs:404`；超过返回 `413`（`relay_payload_too_large`），读取出错返回 `400`（`relay_bad_request`））。故障转移需要重放同一请求，所以不做流式上传。读 body 期间客户端断开则直接结束，不进入上游。
4. 构造 `RelayRequest`，调用拦截器 `on_request`（§7），**每请求一次**，不随重试重复（R4）。
5. 进入 §5.2 的尝试循环。每次尝试：选上游（§4）→ 拼 URL → 按 §2 重建头、写入上游鉴权头（§5.3）→ 若 body 被拦截器改过则重算 `content-length` → 发送。**R2 已实施**单上游版本：取该接口列表中的第一个上游，没有则 `503`（`relay_no_upstream`），上游请求失败则 `502`（`relay_bad_gateway`）；循环与重试在 R3。

整个流程在 `Relay::handle` 这一个 future 内完成，**不 `tokio::spawn`**（§5.4）。

### 5.2 故障转移

**提交点**定义为 `Relay::handle` 把 `Response` 返回给 hyper 的那一刻；此前的重试都在 handle 内的循环完成，此后不再重试。

| 情况 | 行为 |
|---|---|
| 连接失败、TLS 失败、代理失败 | 记一次失败，换下一个上游 |
| 在 `response_head_timeout` 内没有收到响应头 | 记一次失败，换下一个上游 |
| 上游返回状态码属于 `retry_on_status`（默认 `429, 500, 502, 503, 504, 529`） | 读取 body（上限 `retry_body_bytes`，默认 1 MiB）保存为 `last_response`，记一次失败，换下一个上游 |
| 其它状态码（含 2xx、4xx） | 原样转发给客户端，结束 |
| 提交点之后，上游中断 | 直接中断客户端连接，**不重试**（否则会拼接出两段不同的响应）。hyper 对流式响应错误直接断开、不写终止 chunk |

- 总尝试次数上限 `max_attempts`（默认等于该接口的上游数量）。
- 所有上游都失败时：若 `last_response` 存在且 body 未截断，把它的状态码、头（去 hop-by-hop）、body 原样转发（`retry-after` 等头一并保留）；若 body 被截断或最后一次是网络错误没有响应，返回 `502`，body 为中转自身的 JSON 错误。
- `401`、`403` 默认不重试（多半是 Key 配置错误，换上游会掩盖问题），可通过 `retry_on_status` 配置。
- `529` 是 Anthropic 的 `overloaded_error` 状态码（非标准），OpenAI / Gemini 不会返回；`429` 带 `retry-after` 时中转不等待，直接换上游。
- 被丢弃的重试响应：hyper-util 的 HTTP/1.1 连接不可共享（`client.rs:879`）。body 未读完就 drop `Incoming` 时，hyper 先尝试排空剩余 body（`proto/h1/conn.rs` `poll_drain_or_close_read`），排空不了则关闭连接不回池；读完（≤ `retry_body_bytes`）则可回池。这是可接受的代价，不做额外优化。
- **R3 实现时处理**（R2 未动此项，仍为默认 `true`）：hyper-util legacy Client 自带一次内部重试——`retry_canceled_requests` 默认 `true`（`client.rs:252-259, 1034`），当复用的池连接在请求**尚未开始发送**时被对端关闭，Client 会在同一上游重发一次。这不违反"每次原样重发同一请求"，上游也只会收到一次完整请求；R2 记录该行为，或显式 `retry_canceled_requests(false)` 让所有重试都由 §5.2 循环统一计数。
- **已知限制**：OpenAI Responses 的有状态用法（`previous_response_id`、`conversation`、`GET/DELETE /v1/responses/{id}`）引用的 id 是上游私有的，切换上游后会得到上游的 404，且这是一次"成功"透传、不再重试。Codex CLI 当前用 `store: false` 全量历史，不受影响。对 `GET/DELETE /v1/responses/*` core 只发第一个可用上游、不做状态码重试。

### 5.3 上游鉴权写入

| 接口 | 默认 `AuthScheme` | 写入的头 |
|---|---|---|
| `claude` | `XApiKey` | `x-api-key: <key>` |
| `openai_chat`、`openai_responses` | `Bearer` | `authorization: Bearer <key>` |
| `gemini` | `XGoogApiKey` | `x-goog-api-key: <key>` |

`AuthScheme` 可在上游级别覆盖为 `Bearer` / `XApiKey` / `XGoogApiKey` / `None`（例如部分 Anthropic 兼容上游要求 Bearer）。写入发生在拦截器之后、每次尝试各自进行（不同上游不同 Key），拦截器拿不到上游 Key。

### 5.4 响应、超时与取消

- 状态码、响应头原样（去掉 hop-by-hop），body 以流的形式边收边发，不缓冲、不解压、不解析。hyper 服务端每收到一个 Data frame 就写出一个 chunk 并 flush，客户端在上游写出第一段后即可收到。
- 超时（均只影响连接，不改动内容）：
  - `connect_timeout`（**已实施，R2**）：`HttpConnector::set_connect_timeout`（`hyper-util-0.1.20/src/client/legacy/connect/http.rs:347`）；连接器同时 `set_nodelay(true)`，SSE 小块写入不受 Nagle 影响，不改内容。
  - `response_head_timeout`（原 `first_byte_timeout`，改名以免与 SSE 首个数据帧混淆；**R3**）：`tokio::time::timeout` 包住 `client.request(req)`，覆盖建连、TLS、发送与等响应头。
  - `idle_timeout`（默认 300 秒；**R3**）：逐帧 timeout 包装上游 `Incoming` 流；超时则结束流，客户端连接被中断。
- **中转自身响应**（`error.rs`，已实施）：body 固定为 `{"error":{"type":"<type>","message":"..."}}`，`content-type: application/json`，`type` 统一带 `relay_` 前缀以免与上游错误类型混淆：

  | `RelayErrorKind` | 状态码 | `type` | 触发点 |
  |---|---|---|---|
  | `Unauthorized` | 401 | `relay_unauthorized` | §6 鉴权失败 |
  | `NotFound` | 404 | `relay_not_found` | §3 不匹配、`/_relay/*` 未定义路径 |
  | `PayloadTooLarge` | 413 | `relay_payload_too_large` | body 超过 `max_body_bytes` |
  | `BadRequest` | 400 | `relay_bad_request` | 读取入站 body 出错 |
  | `NoUpstream` | 503 | `relay_no_upstream` | 该接口无上游（R3 起含全部熔断） |
  | `BadGateway` | 502 | `relay_bad_gateway` | 上游连接 / TLS / 代理失败（R3 起为全部上游失败且无 `last_response`） |
  | `Internal` | 500 | `relay_internal_error` | 构造上游请求失败（如 api_key 不是合法头值） |
- **客户端断开即取消上游**：依赖 drop 传播。hyper 服务端在客户端断开时 drop 正在执行的 service future，上游的 `client.request()` future 或 `Incoming` 随之 drop，hyper-util 关闭该上游连接。前提是转发路径不 `tokio::spawn`；违反这一条取消语义就丢失了，这是实现硬约束（T12 验证）。机制：hyper 服务端在半关闭关闭（默认）时对繁忙连接检测到 EOF 即返回 `IncompleteMessage`（`proto/h1/conn.rs` `mid_message_detect_eof`），连接 future 结束、在途 service future 被 drop；hyper 客户端 dispatcher 在响应回调被 drop 时 `close()`（`proto/h1/dispatch.rs` `Client::poll_ready`）。原型实测：客户端在响应头到达前断开，handler future ≈1 ms 内被 drop，上游 socket ≈1.5 ms 内收到 EOF；上传 body 中途断开时上游收到 0 字节；流式响应上游分两段写，客户端分别在 3 ms / 206 ms 收到，未整体缓冲。

### 5.5 熔断器

core 自带一个精简熔断器，不复用 `src/proxy/circuit_breaker.rs`（它依赖现有代理的 `log_codes` 与 `types`，搬出来会改动现有代理）。语义与现有实现一致：

- `closed`：连续失败达到 `failure_threshold`（默认 5）→ `open`。
- `open`：经过 `open_duration`（默认 60 秒）→ `half_open`。
- `half_open`：只放行一个探测请求（原子许可，并发安全）；成功 → `closed`，失败 → `open`。
- 按"上游"维度计数，只统计 §5.2 表中的失败情况。

### 5.6 出站代理与 TLS

- 出站代理：`[server].proxy_url` 全局配置，`Upstream.proxy_url` 按上游覆盖（空字符串表示该上游直连）。支持 `http://`、`https://`、`socks5://`、`socks5h://`，与 app 一致（`src-tauri/src/proxy/http_client.rs:236`）。实现用 hyper-util 自带的连接器（`client-proxy` feature）：
  - `http`：`Tunnel::new(proxy_uri, HttpConnector)`（`connect/proxy/tunnel.rs:68`），HTTP CONNECT 隧道；`https`：内层改为 `HttpsConnector<HttpConnector>` 先与代理建 TLS，再 CONNECT。
  - `socks5`：`SocksV5::new(proxy_uri, HttpConnector).local_dns(true)`（本地解析）；`socks5h`：`local_dns(false)`（代理端解析，默认值，`connect/proxy/socks/v5/mod.rs:57,79`）。
  - 代理 URL 中的 `user:pass@` 通过 `with_auth` 传入。**已实施（R2）**：两者签名不同——`Tunnel::with_auth(HeaderValue)` 接收完整的 `proxy-authorization` 值，core 用 `percent-encoding` 解码 userinfo 后自行拼 `Basic base64(user:pass)`（`base64` crate，`HeaderValue` 标记 `set_sensitive`）；`SocksV5::with_auth(user: String, pass: String)` 接收解码后的明文。代理连接目标只保留 scheme、host、端口（socks 缺省 1080，http 缺省 80）。
  - 外层统一再包一层 `hyper-rustls` 的 `HttpsConnector` 负责到上游的 TLS。相同出站代理设置的上游共享一个 `Client` 实例（`UpstreamSet::build` 按生效代理地址去重；连接池按 `Client` 隔离）。
  - **已实施（R2）**：四种连接器组合是四个不同的 `Client<C, B>` 类型（`Tunnel<C>::Response = C::Response`，HTTPS 代理时为 `MaybeHttpsStream<MaybeHttpsStream<…>>`），`upstream.rs` 用 `enum UpstreamClient { Direct, HttpProxy, HttpsProxy, Socks }` 统一 `request()`。
  - **已实施（R2）**：`Tunnel` 发 CONNECT 时端口取 `dst.port()`，缺省固定 **443**、不看 scheme（`connect/proxy/tunnel.rs` `Service::call`）。`upstream_url::upstream_uri` 生成的 authority **总是带显式端口**（http→80、https→443，`url::Url::port_or_known_default`），由 `upstream_url` 单元测试 `http_upstream_gets_explicit_port` 覆盖；T14 的 CONNECT 用例使用带显式端口的上游。
  - `connect_timeout` 只覆盖到代理的 TCP 建连，不覆盖 CONNECT / SOCKS 握手；握手卡住由 `response_head_timeout` 兜底。
- TLS 根证书：默认同时加载 `webpki-roots`（`webpki_roots::TLS_SERVER_ROOTS`）与系统证书（`rustls_native_certs::load_native_certs()`，忽略单条解析错误、只在完全加载失败时告警），合并进一个 `RootCertStore` 后用 `HttpsConnectorBuilder::new().with_tls_config(config)`（`hyper-rustls-0.27.7/src/connector/builder.rs:60`）。`[tls].extra_ca_file` 可追加 PEM 文件（企业 MITM / 自签上游），`[tls].native_roots = false` 可关闭系统证书。
- CryptoProvider 安装方式见 §9。

## 6. 入站鉴权

### 6.1 规则

- 配置 `auth_tokens`（可多个）。
- 从四处收集候选值：`x-api-key`、`authorization`（仅取 `Bearer ` 前缀之后的值，其它 scheme 不算候选）、`x-goog-api-key`、query `key`。**任一**候选与任一 `auth_tokens` 相等即通过；不做"第一个匹配即停"，避免依赖头的顺序。Claude Code 同时设置 `ANTHROPIC_API_KEY` 与 `ANTHROPIC_AUTH_TOKEN` 时会同时发 `x-api-key` 与 `Authorization: Bearer`（Claude Code 2.1.260 自身构造请求头的代码：`{...f&&{Authorization:\`Bearer ${f}\`},..._&&{"x-api-key":_},"anthropic-version":"2023-06-01"}`，两者都有时同时发），Codex 发 `authorization: Bearer` 并附带 `chatgpt-account-id`，Gemini CLI 发 `x-goog-api-key`。
- 比较用 `subtle::ConstantTimeEq`（`[u8]::ct_eq` 在长度不等时自行短路判否，`subtle-2.6.1/src/lib.rs:289-296`，不必额外加长度分支）。
- 无论通过与否，四处全部移除（含非 Bearer 的 `authorization`，它是给中转的，不能到上游），再进入 §5。

### 6.2 监听与匿名模式

- 默认只监听 `127.0.0.1`。
- **默认拒绝匿名**：`auth_tokens` 为空时拒绝启动，除非显式配置 `allow_anonymous = true`；且 `allow_anonymous = true` 只允许与回环地址组合，监听非回环地址时无论如何都要求 `auth_tokens` 非空。
- 匿名模式风险（写入使用说明）：回环监听下任何本机进程、同机其它用户，以及浏览器页面向 `http://127.0.0.1:15721` 发起的 simple request（如 `Content-Type: text/plain` 的 POST，不触发 CORS 预检）都能免费使用上游 Key。匿名模式下请求带 `origin` 头时记一条 warn 日志。

### 6.3 Gemini `key` 的移除

字节级过滤，不用 `url::Url::query_pairs_mut()`（它按 form-urlencoded 重新序列化，空格/保留字符的编码可能改变）：

- 把原始 query 字符串按 `&` 切分；去掉名字精确等于 `key`（`key=...` 或单独的 `key`）的段，不解码；其余段按原顺序用 `&` 拼回；结果为空则不带 `?`。
- 例：`?alt=sse&key=abc&x=a%20b+c` → `?alt=sse&x=a%20b+c`。Gemini CLI 流式请求的 `alt=sse` 必须逐字节保留（`src-tauri/src/proxy/gemini_url.rs:315`）。
- 匿名模式下同样移除。

## 7. 改写组件的接入点（本期只定义，不实现任何改写）

core 暴露一个可选的拦截器接口，默认列表为空，即纯透传：

```rust
#[async_trait]
pub trait Interceptor: Send + Sync {
    /// 入站鉴权、body 读取、接口识别之后、选上游之前调用，每请求一次。
    /// 可修改 interface、path、query、headers、body，也可以直接短路返回响应。
    async fn on_request(&self, ctx: &RequestContext, req: &mut RelayRequest) -> InterceptOutcome {
        InterceptOutcome::Continue
    }
    /// 收到最终要发给客户端的上游响应头后调用一次；可修改状态码与头，也可以包装 body 流。
    async fn on_response(&self, ctx: &RequestContext, resp: &mut RelayResponse) {}
}

pub struct RelayRequest {
    pub interface: Interface,          // 可改，改后按新接口的上游列表路由
    pub method: Method,
    pub path: String,
    pub query: Option<String>,         // 已去掉 key
    pub headers: HeaderMap,            // 已去掉入站凭据与 hop-by-hop；extensions 里的 HeaderCaseMap 一并携带
    pub body: Bytes,
}

pub struct RelayResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BoxBody<Bytes, RelayError>, // 可用 wrapper 做 SSE 逐帧改写
}
```

- 执行顺序（与 §5.1 对应）：入站鉴权并移除凭据 → 接口识别 → 读 body → `on_request`（按注册顺序，一次）→ 每次尝试：选上游、拼 URL、写上游鉴权头、按需重算 `content-length`、发送 → 收到**最终**上游响应头（不属于 `retry_on_status`）→ `on_response`（按注册的逆序，一次）→ 提交点（handler 返回 `Response`，§5.2）。`on_response` 必须在提交点之前执行，否则无法修改状态码与头。
- `on_response` 只对最终要发给客户端的上游响应调用，不对被丢弃的重试响应调用；中转自身生成的 401/404/413/502/503 不经过拦截器。
- 拦截器改了 `body` 时 core 删除并重算 `content-length`；改了 `headers` 时 `HeaderCaseMap` 里没有的头按小写发出。
- 拦截器看到的 body 可能带 `content-encoding`；要改写内容的拦截器需自行解压，或在 `on_request` 中改 `accept-encoding`。core 不代劳。
- `Vec<Box<dyn Interceptor>>` 需要 `async-trait`（原生 async fn in trait 不 dyn-safe）。
- 本期交付：trait 定义 + 空列表 + 一个测试用拦截器（只在测试中注册），用来证明钩子生效；生产配置不能启用任何拦截器。
- 现有代理中的协议转换、thinking 整流、模型映射、缓存注入等，以后可以作为拦截器迁移，不在本期范围。

## 8. 可观测性

- 每个请求输出一行结构化日志：request_id、接口类型、上游 id、尝试次数、状态码、响应头耗时、总耗时、请求与响应字节数。
- **不记录**请求或响应 body，不记录任何 Key 或 token；日志中的 URL 去掉 query。
- 不解析 token 用量（解析需要理解响应内容，本期不做；以后可以用只读的旁路观察者实现，同样不改动流）。
- `GET /_relay/health` 返回 `{"status":"ok"}`（**R4 实现**）。路由顺序：`/_relay/*` 先于鉴权与 §3 的接口匹配（`Relay::handle` 第一步）；`/_relay/` 下未定义的路径 404（`relay_not_found`）；其它路径（含 `/`）按 §3 处理。**R2 阶段 `/_relay/*` 一律 404**。

## 9. crate 结构与依赖

```
src-tauri/crates/
├── cc-switch-domain/      # 已有，本期不再扩展
├── cc-proxy-core/         # 透传引擎 lib
│   └── src/
│       ├── lib.rs           # 对外 API：RelayConfig、Relay、identify、serve；所有模块 pub mod 导出
│       ├── config.rs        # 配置类型与校验（纯数据，不读文件）；Secret、AuthScheme、validate_*
│       ├── interface.rs     # §3 接口识别：identify(method, path, headers)
│       ├── headers.rs       # §2 头处理：hop-by-hop、凭据移除、原位 host 替换、上游鉴权头
│       ├── upstream_url.rs  # §4 URL 拼接、§6.3 query 过滤（原 url.rs，改名避免与外部 crate `url` 在 crate 根产生路径歧义）
│       ├── auth.rs          # §6 入站鉴权
│       ├── breaker.rs       # §5.5 熔断器（时间由调用方传入，R3 接入）
│       ├── tls.rs           # §5.6 根证书合并、CryptoProvider
│       ├── upstream.rs      # UpstreamClient enum、UpstreamSet、出站代理连接器
│       ├── forward.rs       # §5 Relay::handle：转发（故障转移 R3）
│       ├── server.rs        # 入站 accept loop（hyper http1 Builder + service_fn，preserve_header_case）
│       ├── intercept.rs     # §7 拦截器 trait（R4）
│       └── error.rs         # 中转自身错误响应（RelayErrorKind、relay_error）
│   └── tests/
│       ├── support/mod.rs   # 原始 socket 测试基建：mock 上游、原始客户端、relay 启动
│       ├── passthrough.rs   # T1–T5、T9（鉴权部分）、T13、URL 拼接、中转错误
│       ├── outbound_proxy.rs# T14：mock CONNECT / SOCKS5 代理
│       ├── tls_upstream.rs  # T15：自签 HTTPS 上游 + extra_ca_file
│       └── fixtures/tls/    # 预生成的测试 CA 与 localhost 证书（README.md 记录重新生成方法）
└── cc-proxy/                # 可执行程序（R5）
    └── src/
        ├── main.rs          # CLI：serve / check
        └── config_file.rs   # TOML 读取、${ENV} 展开
```

入站不用 `axum::serve`：它内部用 `hyper_util::server::conn::auto::Builder`（`axum-0.7.9/src/serve.rs:19,254`）且不暴露 `preserve_header_case`/`auto_date_header`。**已实施（R2）**：core 自己写 accept loop，用 `hyper::server::conn::http1::Builder`（`timer(TokioTimer)`、`preserve_header_case`）+ `hyper::service::service_fn` 直接调用 `Relay::handle`，**不用 axum / tower**；所有路由判断在 `interface::identify` 内完成。每个连接一个 `tokio::spawn`，单个请求的转发路径上没有任何 spawn，取消语义（§5.4）依然靠 drop 传播。`serve(listener, relay, shutdown)` 在 `shutdown` 完成后停止接受新连接，已建立的连接处理完当前请求。`HeaderCaseMap` 经 `extensions` 在 server → client、client → server 两个方向都能传递（T2、T3 集成测试守护）。

### 9.1 依赖表

按 `src-tauri/crates/cc-proxy-core/Cargo.toml` 实际内容列出（`^` 语义版本写入 `Cargo.toml`，锁定版本以 `src-tauri/Cargo.lock` 为准，列在此处便于核对）。

| crate | Cargo.toml 版本 | 锁定版本 | features | default-features | 用途 |
|---|---|---|---|---|---|
| `cc-switch-domain` | path | — | — | — | 已有 domain crate（`test-util` feature 透传） |
| `tokio` | `1` | 1.50.0 | `macros`, `rt`, `net`, `time`, `sync`, `io-util` | 默认 | 运行时（`rt-multi-thread` 由可执行程序 / dev-dependencies 开启） |
| `hyper` | `1` | 1.8.1 | `http1`, `client`, `server` | 默认 | HTTP/1.1 |
| `hyper-util` | `0.1` | 0.1.20 | `tokio`, `http1`, `client-legacy`, `client-proxy` | 默认（空） | legacy Client、`Tunnel`/`SocksV5`、`TokioIo`/`TokioTimer`/`TokioExecutor`（不需要 `server`/`service`） |
| `hyper-rustls` | `0.27` | 0.27.7 | `http1`, `tls12`, `ring` | **`false`**（默认 feature 含 `aws-lc-rs`；见 §9.2）。不开 `webpki-tokio`/`native-tokio`：core 自己合并 `RootCertStore`，这两个 feature 只会多拉一份 `webpki-roots 1.x` | TLS 连接器（只用 `with_tls_config`） |
| `rustls` | `0.23` | 0.23.37 | `ring`, `tls12`, `logging`, `std` | **`false`** | `ClientConfig`、`RootCertStore`；PEM 解析走 `rustls::pki_types`（不单独依赖 `rustls-pki-types`） |
| `rustls-native-certs` | `0.8` | 0.8.3 | — | 默认 | 系统证书 |
| `webpki-roots` | `0.26` | 0.26.11 | — | 默认 | 内置根证书 |
| `http` | `1` | 1.4.0 | — | 默认 | 类型 |
| `http-body` | `1` | 1.0.1 | — | 默认 | `Body` trait（`Relay::handle` 的 body 泛型约束） |
| `http-body-util` | `0.1` | 0.1.3 | — | 默认 | `Full`、`BodyExt`、`BoxBody`、`Limited` |
| `bytes` | `1` | 1.11.1 | — | 默认 | — |
| `base64` | `0.22` | 0.22.1 | — | 默认 | 出站代理 `Proxy-Authorization: Basic` |
| `percent-encoding` | `2.3` | 2.3.2 | — | 默认 | 代理 URL 中 `user:pass@` 的百分号解码 |
| `url` | `2.5` | 2.5.8 | — | 默认 | `base_url` / `proxy_url` 解析（不用于 query 改写） |
| `serde` | `1.0` | 1.0.228 | `derive` | 默认 | 配置 |
| `serde_json` | `1.0` | 1.0.149 | — | 默认 | 中转自身错误 body |
| `thiserror` | `2.0` | 2.0.18 | — | 默认 | 错误类型 |
| `tracing` | `0.1` | 0.1.44 | — | 默认 | 日志 |
| `subtle` | `2.6` | 2.6.1 | — | 默认 | 常量时间比较 |
| `httparse` | `1.10` | 1.10.1 | — | 默认 | **dev-dependency**：原始 socket mock 上游 |
| `tokio-rustls` | `0.26` | 0.26.4 | `ring`, `tls12`, `logging` | **`false`**（dev） | **dev-dependency**：T15 的自签 HTTPS mock 上游 |
| `tokio`（dev） | `1` | 1.50.0 | `macros`, `rt-multi-thread` | 默认 | **dev-dependency**：集成测试多线程运行时 |

未纳入（较 v3 删除或推迟）：`axum`、`tower`、`futures-util`（入站不用 axum，流适配用 `http-body-util`）；`rustls-pki-types`（经 `rustls::pki_types` 使用）；`async-trait`（R4 引入拦截器时再加）；`rcgen`（不在锁文件里，T15 改用预生成证书）；`toml`、`clap`、`tracing-subscriber`（属 `cc-proxy` 可执行程序，R5）。

- `cc-proxy-core` 不依赖 `cc-switch-domain` 以外的 app 代码，也不依赖 `tauri`、`rusqlite`（`scripts/check-proxy-core-deps.sh` 已在 CI 中守护）。
- HTTP 客户端选 hyper 而不是 reqwest：reqwest 会自行补充部分默认头，并在启用相应 feature 时自动解压；hyper 对头和 body 的控制更直接，便于保证 §2 的透传边界。

### 9.2 rustls CryptoProvider

`rustls 0.23` 在未安装进程级 provider 且 feature 二义时，`ClientConfig::builder()` 会 panic。app 是在 `src-tauri/src/lib.rs:465` 手动 `rustls::crypto::ring::default_provider().install_default()`。core 的做法：

- `tls.rs` 构造 `ClientConfig` 时用 `ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions()` 显式传 provider，不依赖进程级安装；这样嵌入 app 时也不会与 app 的 `install_default()` 冲突。**这是唯一有效的保护**。
- core 内**禁止**调用 `ClientConfig::builder()`、`HttpsConnectorBuilder::with_native_roots()` / `with_webpki_roots()` / `with_platform_verifier()`（它们内部走 `builder()`，`hyper-rustls-0.27.7/src/connector/builder.rs:76-`），只允许 `with_tls_config`。
- `default-features = false` + `ring` 只在单独构建 `-p cc-proxy-core` 时成立。CI 用 `cargo test --workspace`、`cargo clippy --workspace`（`.github/workflows/ci.yml:153,160`），resolver 2 会把 app 的 `hyper-rustls`（未关默认 feature，含 `aws-lc-rs`，`src-tauri/Cargo.toml:63`）与 `rustls = "0.23"`（默认含 `aws_lc_rs`，`:69`）统一到同一份 rustls 上，二义在 workspace 构建中始终存在。保留 `default-features = false` 是为了让 core 单独构建时依赖图最小，不是为了避免二义。
- 原型实测：上表组合（含 `webpki-roots 0.26`、`rustls-native-certs 0.8`）`cargo build --offline` 通过，`builder_with_provider` 路径可用。

## 10. 配置示例（`cc-proxy`）

```toml
[server]
listen = "127.0.0.1:15721"
auth_tokens = ["${CC_PROXY_TOKEN}"]
# allow_anonymous = false        # 仅回环地址可设为 true
max_body_bytes = 209715200        # 200 MiB
preserve_header_case = true
# proxy_url = "socks5h://127.0.0.1:1080"   # 全局出站代理，可被上游覆盖

[tls]
native_roots = true
# extra_ca_file = "/etc/ssl/corp-ca.pem"

[timeouts]
connect_secs = 10
response_head_secs = 120
idle_secs = 300

[failover]
retry_on_status = [429, 500, 502, 503, 504, 529]
retry_body_bytes = 1048576
# max_attempts 缺省为上游数量

[breaker]
failure_threshold = 5
open_secs = 60

[[upstreams.claude]]
id = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "${ANTHROPIC_API_KEY}"

[[upstreams.openai_responses]]
id = "openai"
base_url = "https://api.openai.com"
api_key = "${OPENAI_API_KEY}"
proxy_url = "http://127.0.0.1:7890"

[[upstreams.openai_chat]]
id = "deepseek"
base_url = "https://api.deepseek.com"
api_key = "${DEEPSEEK_API_KEY}"
proxy_url = ""                    # 空字符串：该上游直连

[[upstreams.gemini]]
id = "google"
base_url = "https://generativelanguage.googleapis.com"
api_key = "${GEMINI_API_KEY}"
```

- `${VAR}` 在加载时展开；变量未定义时报错退出。
- 某个接口没有配置上游时，该接口的请求返回 `503`。
- `cc-proxy check --config <path>` 只校验配置、打印每个上游的 dry-run URL，不发请求。

## 11. 测试与验收

透传正确性是本方案的核心，用"上游镜像"集成测试逐项证明。**mock 上游用原始 `TcpListener` + `httparse`**，记录收到的原始字节（头的大小写、顺序、分帧都可见），并按测试用例手写响应字节（含手写 chunked 分帧）；不用 hyper 做 mock 服务端，它会重新解析和分帧。客户端侧同样用原始 socket 发请求、收原始响应字节后自行去分帧再比较。**已实施（R2）**：基建在 `tests/support/mod.rs`（mock 上游按 `Segment` 序列分段写出响应、记录 `RecordedRequest`；原始客户端；`relay_with` 用 `RelayConfig` 启动中转），`passthrough.rs`、`outbound_proxy.rs`（含 mock CONNECT / SOCKS5 代理）、`tls_upstream.rs` 三个文件共 27 个集成测试。

| 编号 | 验收项 |
|---|---|
| T1 | 四个接口各一组：上游收到的 method、path、query（去掉 `key`，其它参数逐字节不变，含 `alt=sse&x=a%20b+c` 用例）、body 与客户端发出的**逐字节相同** |
| T2 | 上游收到的头 = 客户端头 − 入站鉴权头（含非 Bearer 的 `authorization`）− hop-by-hop（含 `proxy-connection`）− `expect` + 上游鉴权头 + 上游 host；头名大小写、顺序（按 §2.1 定义：不同名头按首次出现位置、交错同名头聚合，含一条 `Anthropic-Beta` / `User-Agent` / `anthropic-beta` 交错用例）、重复头条数、非 UTF-8 头值不变；`host` 在原位置且沿用入站拼写（入站 `Host` → 上游 `Host`）；上游鉴权头沿用入站同名头拼写（入站 `Authorization` / `X-Api-Key` → 上游 `Authorization` / `X-Api-Key`），入站无同名头时为小写；`preserve_header_case = false` 时全小写 |
| T3 | 客户端收到的状态码、响应头（去掉 hop-by-hop）、body 与上游返回的相同（按 §2.1 定义域；上游不带 `date` 时允许中转补 `date`；hyper 补的 `transfer-encoding` / `content-length` 在上游存在同名头时沿用其拼写） |
| T4 | SSE 流：上游用原始 socket 分多次、以任意 chunk 边界写出（含把一个事件拆在两个 chunk 里），客户端去分帧后字节相同，且在上游写出第一段后即可收到（不整体缓冲） |
| T5 | `content-encoding: gzip` / `br` 的响应原样转发、不被解压；请求 `accept-encoding` 原样到上游 |
| T6 | 故障转移：首个上游返回 `503` / 连接失败 / 响应头超时时，第二个上游收到的 body 与原请求逐字节相同；`400` 不触发切换；全部失败时客户端收到最后一个上游的 `503` 响应（含其 `retry-after`）；最后一次为网络错误时收到中转的 `502` |
| T7 | 提交点之后上游中断：客户端连接中断，第二个上游**没有**收到请求 |
| T8 | 熔断：连续失败达到阈值后跳过该上游；`open_secs` 后放行且只放行一个探测请求（并发验证） |
| T9 | 入站鉴权：四种携带方式均可通过；同时带正确 `x-api-key` 与错误 `authorization` 通过、反之亦通过；错误 token 返回 `401`；token 不出现在上游请求（头与 query）中；非回环地址且无 token 时拒绝启动；无 token 且未设 `allow_anonymous` 时拒绝启动 |
| T10 | 未安装拦截器时 T1–T5 成立；安装测试拦截器后其修改（改 path、改 body 且 `content-length` 重算、包装 SSE 流、短路返回）生效；被丢弃的重试响应不触发 `on_response` |
| T11 | 日志中不出现 body、Key、token、query |
| T12 | 客户端在响应头到达前断开：上游连接被关闭（mock 上游观察到 EOF），第二个上游不被调用；客户端在上传 body 中途断开：上游未收到请求 |
| T13 | 边界：客户端 chunked 上传 → 上游收到 `content-length` 且 body 相同；客户端带 `expect: 100-continue` → 客户端收到 `100 Continue`、上游未收到 `expect`；`HEAD` 与 `204` 无 body；上游响应 trailer 被丢弃；`GET /v1/models` 不带 `anthropic-version` 透传到 openai_responses 上游、带 `anthropic-version` 透传到 claude 上游；`/v1/models/gemini-2.5-flash:generateContent` 归 gemini、`/v1/models/gpt-4o` 归 openai_responses |
| T14 | 出站代理：经本地 mock HTTP CONNECT 代理与 mock SOCKS5 代理各转发一次，上游收到的字节与直连相同；`socks5h` 下 mock 代理收到的是域名而非 IP；`http://` 无显式端口的上游经 CONNECT 代理时，mock 代理收到 `CONNECT host:80`（§5.6） |
| T15 | TLS：mock HTTPS 上游用自签证书，仅 `extra_ca_file` 指向该 CA 时成功，否则 TLS 失败（R2：返回 `502` `relay_bad_gateway`；R3 起触发故障转移）。证书为 `tests/fixtures/tls/` 下用 openssl 预生成的测试 CA（`ca.pem`，P-256，CA 私钥生成后已丢弃）与 `localhost` 服务端证书（`localhost.pem` / `localhost.key`，SAN 含 `DNS:localhost`、`IP:127.0.0.1`），不引入 `rcgen`（不在锁文件里）；重新生成步骤见 `tests/fixtures/tls/README.md`，三个文件需一并替换 |
| T16 | URL 拼接：`strip_prefix` 命中/未命中、`base_url` 带 query 合并、`check` 对含端点路径的 `base_url` 报错 |

另外做一次真实上游冒烟：本地起 `cc-proxy`，让 Claude Code、Codex CLI、Gemini CLI 分别指向它，完成一次带工具调用的流式对话。需要真实 Key，由使用者在本机执行；同时核对 §14 的待确认项。

## 12. 分阶段实施

每一步独立提交，提交前 `cargo fmt --all --check`、`cargo clippy --workspace -- -D warnings`、`cargo test --workspace --features cc-proxy-core/test-util` 全绿。R1 起所有模块在 `lib.rs` 用 `pub mod` 导出并在单元测试中使用，避免 `dead_code` 触发 `-D warnings`。

| 步骤 | 内容 | 验收 |
|---|---|---|
| R1 **已实施** | 纯逻辑模块与单元测试：`config`（含校验规则）、`interface`、`headers`（hop-by-hop、凭据移除、原位 host 替换、`expect` 移除）、`upstream_url`（拼接、query 合并、字节级 `key` 过滤、显式端口）、`auth`（候选收集、常量时间比较、匿名规则）、`breaker`、`error`。子步骤与约束见下方"R1 实施须知" | 单元测试覆盖 §2.2 表格每一行、§3 每条规则（含按头分流、`POST` 限定的 Gemini 冒号路径、微调模型 id）、§4 每个例子、§5.5 状态转换、§6 每条规则；T16 中不依赖网络的部分 |
| R2 **已实施** | `tls`、`upstream`（`UpstreamClient` enum、`UpstreamSet` 按代理设置共享 `Client`、CONNECT/SOCKS 鉴权拼装，§5.6）、`server`（自建 http1 accept loop + `service_fn`，不用 axum，§9）、`forward`（`Relay::handle`：单上游透传，四个接口，流式响应）；接入 R1 的 `auth`；原始 socket 测试基建（`tests/support`）。**验收结果**：T1–T5、T9 鉴权部分、T13、T14、T15 共 27 个集成测试外加若干单元测试全部通过；workspace 全量测试 3200 通过、0 失败、9 忽略。**变异验证**：去掉 `extensions` 搬运后 T2 失败；改成整体缓冲后 T4 失败；对调 `socks5` / `socks5h` 的解析方式后两个 SOCKS 测试都失败。T14 的 CONNECT 用例用带显式端口的上游，`http` 上游缺省端口补 80 由 `upstream_url` 单元测试覆盖。**未实现，留给 R3**：T12 取消测试、故障转移循环、`response_head` / `idle` 超时 | T1–T5、T9（鉴权部分）、T13、T14、T15 |
| R3 | 故障转移（含 `last_response` 保留与回放、`retry_canceled_requests` 取舍，§5.2）、熔断接入、超时、取消 | T6–T8、T12 |
| R4 | 日志、`/_relay/health`、`intercept`、T9 中的启动校验 | T9–T11 |
| R5 | `cc-proxy` 可执行程序：TOML 配置、`${ENV}` 展开、`serve`、`check`（dry-run URL）、示例配置与 `docs/` 使用说明（含 §6.2 匿名风险） | 用本机 mock 上游端到端跑通；真实上游冒烟与 §14 核对由使用者执行 |

**R1 实施须知**（R1 已按此完成，保留作为约束记录）：

1. **依赖范围**：R1 的 `Cargo.toml` 只加纯逻辑依赖——`http`、`url`、`serde`（`derive`）、`serde_json`（`error.rs` 的错误 body）、`subtle`、`thiserror`、`bytes`、`http-body-util`（`error.rs` 的 `BoxBody`）。**不引入** `tokio`、`hyper*`、`rustls*`、`axum`；TLS 栈与运行时到 R2 再加，避免 R1 提交就把整套依赖和 §9.2 的 feature 问题带进 workspace。
2. `interface.rs`：函数签名按 §3 的 `identify(method, path, headers)`；单测至少覆盖：`GET /v1/models` 带 / 不带 `anthropic-version`、`GET /v1/models/gpt-4o`、`/v1/models/gemini-2.5-flash:generateContent`、`/v1beta/models/x:streamGenerateContent`、`/_relay/health` 优先、`/v1/alpha/search` 与 `/` 返回 `None`。
3. `headers.rs`：重建 `HeaderMap` 时按入站迭代顺序 `append`，`host` 原位替换；断言按 §2.1 的顺序定义写（同名聚合、不同名按首次出现位置），不断言"逐条 wire 顺序完全一致"。`RelayRequest` 预留 `extensions: http::Extensions` 字段，R1 只透传不解释（`HeaderCaseMap` 是 `pub(crate)`，core 不能命名它）。
4. `upstream_url.rs`（不叫 `url.rs`：与外部 crate `url` 重名，在 crate 根会产生路径歧义）：拼接结果 authority 总是带显式端口（http→80、https→443，§5.6），单测覆盖两种 scheme；`strip_prefix` 与 query 合并按 §4 的例子逐一断言。
5. `auth.rs`：直接用 `subtle` 的 `[u8]::ct_eq`，不必自加长度分支（§6.1）。
6. `breaker.rs`：时间源用可注入的 `now: fn() -> Instant` 或 trait，纯同步实现，R1 不需要 `#[tokio::test]`。
7. `config.rs`：只做纯数据校验（§4 的 `base_url` 规则、§6.2 的匿名规则、`strip_prefix` 格式、`proxy_url` scheme 白名单）；不读文件、不解析 `${ENV}`（R5）。
8. 所有模块 `pub mod` 导出并在单测中使用，否则 `cargo clippy --workspace -- -D warnings` 会因 dead_code 失败。
9. R1 完成后先按 §15 v3 条目核对本文档是否已同步，再进入 R2。

各阶段可独立编译：`auth` 模块 R1 已有，R2 直接接入，不需要"先用 `allow_anonymous` 启动"的过渡；R3 只增加转发循环，不改对外 API。

后续（不在本期）：HTTP/2、改写组件（把现有代理的转换逻辑作为拦截器迁移）、桌面端改用新 core、按模型路由、只读用量观察者、`import` 桌面端配置、Codex 的 `alpha/search` 与 `images/*` 路径。

## 13. 风险

| 风险 | 应对 |
|---|---|
| 某些上游要求特定头或 URL 形式，透传后无法直接用 | 通过 `AuthScheme`、`strip_prefix`、`base_url` query 覆盖常见差异；更复杂的差异交给改写组件 |
| 故障转移在流式场景下的边界 | 严格遵守"提交点后不重试"（T7） |
| Responses 有状态用法跨上游断裂 | §5.2 已知限制；`GET/DELETE /v1/responses/*` 不做状态码重试 |
| 大 body 内存占用 | `max_body_bytes` 上限 200 MiB；`retry_body_bytes` 限制重试响应缓冲 |
| 匿名模式被本机其它进程或浏览器页面滥用 | 默认拒绝，显式 `allow_anonymous` 才放行，且仅限回环地址（§6.2） |
| `preserve_header_case` 依赖 hyper 的 `HeaderCaseMap` 扩展在 server → client 之间传递 | 已确认 hyper 1.8.1 两端都读写该扩展，且原型实测双向可用（§2.4）；升级 hyper 时由 T2 守护 |
| workspace 构建下 rustls `ring` / `aws-lc-rs` 二义 | core 只走 `builder_with_provider`，禁用 `ClientConfig::builder()` 系列（§9.2）；clippy 无法拦截，靠代码评审与 T15 |
| `GET /v1/models` 按头分流依赖 Claude Code 持续发送 `anthropic-version` | Anthropic SDK 默认头，§14 C1 冒烟核对；失败时 Claude Code 只记日志不影响会话 |
| 与现有桌面代理并存 | 两者互不依赖，默认端口不同；桌面端行为不变 |
| hyper 升级改变分帧头行为 | §2.3 的行为由 T2/T3/T13 守护 |

## 14. 待冒烟确认

以下在设计阶段未确认，由使用者在真实冒烟时核对，结果回填本节：

| 编号 | 事项 | 影响 |
|---|---|---|
| C1 | **已实证（v3）**：Claude Code 2.1.260 二进制中的 `[gatewayDiscovery]` 逻辑会发 `GET {ANTHROPIC_BASE_URL}/v1/models?limit=1000`，条件为 base URL 指向非第一方 host、有凭据、且设置 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`；头固定含 `anthropic-version: 2023-06-01`；非 2xx 只记 `[gatewayDiscovery] non-OK status`，不影响会话；响应按 `{data:[{id,display_name,…}]}` 校验并按 `/(claude\|ant…)/` 过滤 id。§3 已按 `anthropic-version` 头分流。**冒烟时核对**：开启该环境变量、指向中转后，`/model` 列表能出现 claude 上游返回的模型；不开启时中转无该请求 |
| C2 | **已实证（v3）**：Codex CLI 0.155.1 二进制字符串——连通性探测要求 `GET /models returns 2xx, 401, or 403`，404 时提示 `provider base URL route returned 404 - verify the configured API prefix`（`/v1/models` 请求由 `model-provider/src/models_endpoint.rs` 发出），模型拉取失败记 `Failed to fetch models: `，均为提示、不阻断会话。§3 不合成响应，透传上游结果。**冒烟时核对**：上游返回 2xx 时 Codex 的模型列表正常；上游 404 时仅出现该提示 |
| C3 | Codex CLI 0.155.1 二进制仍含 `previous_response_id`（5 处）与 `conversation_id`（2 处）字符串，无法从二进制判断默认路径是否使用；冒烟时用 mock 上游记录请求 body，确认是否带 `previous_response_id` / `store` | 决定 §5.2 Responses 限制是否影响实际使用 |
| C4 | 三个 CLI 实际发出的头是否全部为 HTTP/1.1（Codex 的 reqwest 默认可能协商 HTTP/2，但指向 `http://127.0.0.1` 时为 h1） | 决定 §2.3 的 HTTP/1.1 假设是否成立 |

## 15. 修订记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v1 | 2026-09-29 | 初稿 |
| v3.1 | 2026-09-29 | 按 R1/R2 实施结果同步（对照 `src-tauri/crates/cc-proxy-core/` 代码与测试）：模块 `url.rs` 改名 `upstream_url.rs`（与外部 crate `url` 重名，§9、§12）；Gemini `/v1/models/{name}:{action}` 仅 `POST` 归 gemini，`GET` 含冒号的 OpenAI 微调模型 id 按 `/v1/models/{id}` 分流（§3）；入站服务改为 hyper `service_fn` 直接调用 `Relay::handle`，不用 axum / tower，路由全在 `interface::identify`（§3、§9）；§5.1 顺序改为鉴权 → 识别接口 → 读 body，R2 单上游版本与错误码落点；中转自身错误 `type` 统一 `relay_` 前缀并列表（§3、§5.4）；`/_relay/*` R2 一律 404、`/_relay/health` R4 实现（§8）；`connect_timeout` 已实施、`response_head` / `idle` 超时与 `retry_canceled_requests` 取舍归 R3（§5.2、§5.4）；§5.6 出站代理各条标"已实施"，显式端口由 `upstream_url` 单测覆盖；§4 `UpstreamConfig` 字段按实际类型（`Secret`、`Option<AuthScheme>`、`Option<String>`）；§9.1 依赖表按 core 实际 `Cargo.toml` 重写（补 `base64`、`percent-encoding`、`http-body`、`cc-switch-domain`，dev 依赖 `httparse`、`tokio-rustls`、`tokio`；删 `axum`、`tower`、`futures-util`、`rustls-pki-types`、`async-trait`，`toml`/`clap`/`tracing-subscriber` 归 R5）；T15 改用 `tests/fixtures/tls/` 预生成证书、不引入 `rcgen`（§11）；§11 记录测试基建与 27 个集成测试；§12 R1、R2 标"已实施"并写入 R2 验收结果（27 集成测试 + workspace 3200 通过 / 0 失败 / 9 忽略）与变异验证 |
| v3 | 2026-09-29 | 按 Fable 二次审查（源码抽样核对 26 处 + `/tmp/relay-proto` 原型实测）修订：P1 `HeaderCaseMap` 按头名配对拼写，core 重写的 `host` / 鉴权头沿用入站拼写（§2.4、T2、T3）；P2 头顺序定义改为"同名聚合、不同名按首次出现位置"（§2.1、T2）；P3 `default-features = false` 在 workspace 构建中不能避免 provider 二义，唯一保护是 `builder_with_provider` 并禁用 `builder()` 系列，rustls 补 `std`，hyper-rustls 去掉 `webpki-tokio`/`native-tokio`（§9.1、§9.2、§13）；P4 `Tunnel` CONNECT 端口缺省 443，core 按 scheme 补端口（§5.6、T14、R2）；P5 四种连接器为不同 `Client` 类型，用 enum 统一，`with_auth` 签名差异，`connect_timeout` 不覆盖握手（§5.6、R2）；P6 Claude Code 2.1.260 实证会发 `GET /v1/models?limit=1000`，§3 改为按 `anthropic-version` 头分流并写入 `identify(method, path, headers)` 签名（§3、§13、§14 C1、T13）；P7 §6.1 "同时发两个头"改引 Claude Code 自身代码；P8 §2.3 Cargo.toml 行号改为 61-63；P9 `ct_eq` 不等长自行短路（§6.1）；P10 `on_response` 在提交点之前（§7）；P11 hyper-util `retry_canceled_requests` 内部重试与 body drop 排空行为（§5.2、R3）；P12 axum 单 `fallback` 路由（§3、§9、R2）；P13 Codex 0.155.1 对 `/models` 404 的行为与 `previous_response_id` 字符串证据（§14 C2、C3）；R1 实施须知并入 §12，R1 只引入纯逻辑依赖，`auth` 接入提前到 R2（§12）；取消语义、流式、100-continue、chunked→content-length 原型实测结论记入 §2.4、§5.4、§9 |
| v2 | 2026-09-29 | 按 Fable 审查修订：S1 `last_response` 保留与回放（§5.2）；S2 `GET /v1/models` 纳入 openai_responses、Codex 其它路径明确排除（§3）；S3 依赖表、`default-features = false`、CryptoProvider（§9）；I1 分帧头、`expect`、`content-length`、`via`、定义域（§2.1–2.3）；I2 Responses 有状态限制（§5.2）；I3 鉴权候选规则、默认拒绝匿名、浏览器风险（§6）；I4 字节级 `key` 过滤（§6.3）；I5 `base_url` query 合并、`strip_prefix` 规则、`check` 校验（§4）；I6 拦截器执行顺序与类型（§7）；I7 提交点、超时改名、取消依赖 drop 且禁止 spawn（§5.2、§5.4）；I8 出站代理纳入 R2（§5.6）；A1 默认保留头名大小写与顺序（§2.4）；A2 仅 HTTP/1.1、`proxy-connection`（§2.2–2.3）；A3 Gemini 路由与 OAuth 说明（§3）；A4 200 MiB（§5.1）；A5 系统证书 + `extra_ca_file`（§5.6）；A6 原始 socket mock 与边界用例 T12–T16（§11）；A7 `pub mod` 导出（§12）；A8 529/429 说明；A9 路由顺序（§8）；A10 待冒烟确认（§14） |
