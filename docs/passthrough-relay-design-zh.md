# 透传中转 core 设计方案

> 状态：草案 v3.4（已按 R5 实施结果同步，R1–R5 全部实施）· 基线：`refactor/standalone-proxy-core@ba16ab849`
> 实现位置：`src-tauri/crates/cc-proxy-core/`（引擎 lib）与 `src-tauri/crates/cc-proxy/`（可执行程序）。R1–R5 已实施，R4 不含拦截器（§7 暂缓），见 §12。使用说明见 `docs/cc-proxy-usage-zh.md`。
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
| 在 `response_head_timeout` 内没有收到响应头 | 记一次失败，换下一个上游（超时 drop `client.request` future，hyper 客户端随之关闭该上游连接） |
| 上游返回状态码属于 `retry_on_status`（默认 `429, 500, 502, 503, 504, 529`），且还有后续尝试 | 读取 body（上限 `retry_body_bytes`，默认 1 MiB）保存为 `last_response`，记一次失败，换下一个上游 |
| 上游返回状态码属于 `retry_on_status`，但这已是最后一次可能的尝试 | 仍记一次失败，然后**直接流式转发**该响应（不缓冲、不截断，`retry-after` 等头原样） |
| 其它状态码（含 2xx、4xx） | 记一次成功，原样转发给客户端，结束 |
| 提交点之后，上游中断 | 直接中断客户端连接，**不重试**（否则会拼接出两段不同的响应）。hyper 对流式响应错误直接断开、不写终止 chunk |

**已实施（R3，`forward.rs` `Relay::handle`）**：

- 总尝试次数上限 `max_attempts = min(配置值, 该接口的上游数量)`，配置缺省即上游数量；`Some(0)` 在配置校验时拒绝。熔断器拒绝放行的上游直接跳过、不计入尝试次数。
- "是否还有后续尝试"（`has_next`）= `attempts < max_attempts` 且列表后面还有 `breaker.is_available(now)` 为真的上游。`is_available` 只是快照、不占用许可：若到达下一个上游时它的半开探测名额已被并发请求占走，`try_acquire` 返回 `None`，循环结束并回放刚缓冲的 `last_response`，与直接流式转发结果等价（body ≤ `retry_body_bytes` 时；超限则退化为 `502`），因此不需要提前预占许可。
- 全部失败时的收尾：
  - 存在完整缓冲的 `last_response` → 回放**最近一次缓冲的那个**：状态码、头（去 hop-by-hop，含 `retry-after`）、body 与 `HeaderCaseMap` 扩展原样，哪怕之后的尝试是网络错误或超时。对客户端来说上游的 `429` / `503` + `retry-after` 比中转的 `502` 更有信息量。
  - 没有任何可回放的响应（全部是网络错误、超时，或可重试响应的 body 超过 `retry_body_bytes`）→ `502` `relay_bad_gateway`，message 为最后一次失败的描述。
  - 一次都没有尝试（该接口的上游全部处于熔断打开）→ `503` `relay_no_upstream`。
- 有状态的 `GET` / `DELETE /v1/responses/*`（`is_stateful_responses_call`）：`max_attempts` 固定为 1，只发给第一个熔断器放行的上游；无论是可重试状态码还是网络错误都不换上游（状态码直接流式转发，网络错误返回 `502`）。
- 每次尝试从入站请求 clone `http::Extensions`（`HeaderCaseMap` 实现 `Clone`）与已缓冲的 `Bytes` body，重建上游请求。
- 熔断许可由 `AttemptGuard` 持有：结果确定时 `success()` / `failure()` 消费许可并记账；future 在结果确定前被 drop（客户端断开导致取消）时，`Drop` 调 `release` 归还许可，不计成功也不计失败，半开探测名额不会泄漏。
- `401`、`403` 默认不重试（多半是 Key 配置错误，换上游会掩盖问题），可通过 `retry_on_status` 配置。
- `529` 是 Anthropic 的 `overloaded_error` 状态码（非标准），OpenAI / Gemini 不会返回；`429` 带 `retry-after` 时中转不等待，直接换上游。
- 被丢弃的重试响应：hyper-util 的 HTTP/1.1 连接不可共享（`client.rs:879`）。body 未读完就 drop `Incoming` 时，hyper 先尝试排空剩余 body（`proto/h1/conn.rs` `poll_drain_or_close_read`），排空不了则关闭连接不回池；读完（≤ `retry_body_bytes`）则可回池。这是可接受的代价，不做额外优化。缓冲 `last_response` 的读取同样受 `idle_timeout` 约束（§5.4），上游发错误 body 时停住不会拖住整个请求，超时按"body 读取失败"处理、不可回放。
- **已实施（R3）：`retry_canceled_requests` 保持默认 `true`**。hyper-util legacy Client 自带一次内部重试（`client.rs:252-259, 1034`）：当复用的池连接在请求**尚未开始发送**时被对端关闭，Client 会在同一上游重发一次。这不违反"每次原样重发同一请求"，上游也只会收到一次完整请求，也不经过 §5.2 循环计数；保持 `true` 的理由是它恰好兜住"空闲池连接被上游关闭、本地尚未察觉"的竞争窗口，否则这种情况会被当成一次上游失败并计入熔断。
- **已知限制**：OpenAI Responses 的有状态用法（`previous_response_id`、`conversation`、`GET/DELETE /v1/responses/{id}`）引用的 id 是上游私有的，切换上游后会得到上游的 404，且这是一次"成功"透传、不再重试。Codex CLI 当前用 `store: false` 全量历史，不受影响。对 `GET/DELETE /v1/responses/*` core 只发第一个可用上游、不做状态码重试（见上文）。

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
  - `response_head_timeout`（原 `first_byte_timeout`，改名以免与 SSE 首个数据帧混淆；**已实施，R3**）：`tokio::time::timeout(response_head_secs, client.request(req))`，覆盖建连、TLS、发送与等响应头。超时后 drop 该 future——hyper 客户端 dispatcher 在响应回调被 drop 时关闭该上游连接（`proto/h1/dispatch.rs` `Client::poll_ready`）——按一次失败记入熔断并换下一个上游（§5.2）。
  - `idle_timeout`（默认 300 秒；**已实施，R3**，`idle.rs` `IdleTimeoutBody`）：包装上游 `Incoming`，每收到一帧就 `reset` 一个 `Pin<Box<Sleep>>`；内层 `Pending` 时 poll 该 `Sleep` 以注册 waker，到期返回 `Err(IdleTimeout)`。同一个包装用在两处：流式回传时 hyper 服务端收到 body `Err` 即结束连接、不写终止 chunk（`proto/h1/dispatch.rs` `poll_write` → 连接 future 以 `Err` 结束），客户端能看出响应不完整；缓冲可重试响应的 body（`buffer_response`）时超时按"body 读取失败"处理，该响应不可回放。
- **中转自身响应**（`error.rs`，已实施）：body 固定为 `{"error":{"type":"<type>","message":"..."}}`，`content-type: application/json`，`type` 统一带 `relay_` 前缀以免与上游错误类型混淆：

  | `RelayErrorKind` | 状态码 | `type` | 触发点 |
  |---|---|---|---|
  | `Unauthorized` | 401 | `relay_unauthorized` | §6 鉴权失败 |
  | `NotFound` | 404 | `relay_not_found` | §3 不匹配、`/_relay/*` 未定义路径 |
  | `PayloadTooLarge` | 413 | `relay_payload_too_large` | body 超过 `max_body_bytes` |
  | `BadRequest` | 400 | `relay_bad_request` | 读取入站 body 出错 |
  | `NoUpstream` | 503 | `relay_no_upstream` | 该接口无上游；或上游全部处于熔断打开、一次都没有尝试（已实施，R3） |
  | `BadGateway` | 502 | `relay_bad_gateway` | 全部尝试失败且没有可回放的 `last_response`（网络错误、超时、body 超限；已实施，R3） |
  | `Internal` | 500 | `relay_internal_error` | 构造上游请求失败（如 api_key 不是合法头值） |
- **客户端断开即取消上游**：依赖 drop 传播。hyper 服务端在客户端断开时 drop 正在执行的 service future，上游的 `client.request()` future 或 `Incoming` 随之 drop，hyper-util 关闭该上游连接。前提是转发路径不 `tokio::spawn`；违反这一条取消语义就丢失了，这是实现硬约束（T12 验证）。机制：hyper 服务端在半关闭关闭（默认）时对繁忙连接检测到 EOF 即返回 `IncompleteMessage`（`proto/h1/conn.rs` `mid_message_detect_eof`），连接 future 结束、在途 service future 被 drop；hyper 客户端 dispatcher 在响应回调被 drop 时 `close()`（`proto/h1/dispatch.rs` `Client::poll_ready`）。原型实测：客户端在响应头到达前断开，handler future ≈1 ms 内被 drop，上游 socket ≈1.5 ms 内收到 EOF；上传 body 中途断开时上游收到 0 字节；流式响应上游分两段写，客户端分别在 3 ms / 206 ms 收到，未整体缓冲。**已实施（R3）**：`tests/cancellation.rs` 用 mock 上游的 EOF 观察通道（`MockUpstream::closed_within`，在延迟写出期间同时读 socket）验证了响应头前断开、上传中途断开、流式中途断开三种情况（T12）以及提交点后不重试（T7）；取消发生在 `AttemptGuard` 记账前时许可被归还（§5.2）。

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

> **按用户决定暂缓，本期不实现**（v3.3）：R4 未加入 `intercept.rs`，`Relay::handle` 内没有任何拦截器钩子，core 始终是纯透传；§11 T10 同步暂缓。以下设计内容保留，作为以后迁移改写组件时的参考，不代表当前代码。

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

**已实施（R4，`access_log.rs`）**。core 只依赖 `tracing`，不引入 `tracing-subscriber`；订阅者由可执行程序（R5）或宿主安装。

- **每个请求恰好一行**访问日志：`tracing::info!`，target 固定为 `cc_proxy_core::access`（常量 `ACCESS_TARGET`），级别统一为 info，用 `outcome` 字段区分结果，不按级别区分。消息文本固定为 `relay request`。字段：

  | 字段 | 含义 |
  |---|---|
  | `request_id` | 进程内 `AtomicU64` 计数器生成，格式 `r-{n:x}`（从 `r-1` 起、十六进制）；**不写入任何响应头**，保持透传 |
  | `method`、`path` | 入站 method 与 path；**path 不含 query** |
  | `interface` | §3 识别结果（`claude` / `openai_responses` / `gemini` …），未识别时 `-` |
  | `upstream` | 最后一次尝试的上游 id，未尝试时 `-` |
  | `attempts` | 上游尝试次数（含失败的尝试） |
  | `status` | 发给客户端的状态码，未产生响应（`cancelled`）时 `0` |
  | `head_ms` | 从收到请求到收到最终上游响应头的耗时；没有上游响应时 `0` |
  | `total_ms` | 从收到请求到写出这行日志的耗时（流式响应即 body 结束时） |
  | `request_bytes` | 读入的请求 body 字节数（`413` 等读 body 前失败时 `0`） |
  | `response_bytes` | 发给客户端的 body 字节数：固定 body 取其长度，流式 body 累计 data 帧字节数 |
  | `outcome` | 见下表 |
  | `error` | `outcome = error` 时的错误描述（如空闲超时），否则 `-` |

  | outcome | 语义 | 写出时机 |
  |---|---|---|
  | `complete` | 响应完整发出。含中转自身的 401/404/413/502/503、`/_relay/health`、回放的缓冲响应，以及 HEAD / 204 / 304 等无 body 的上游响应 | 固定 body：`AccessLog::finish` 在 `handle` 返回前立即写出；流式 body：`LoggedBody::poll_frame` 读到 `None`，或读到一个 data 帧后内层 `is_end_stream()` 为真（已知长度的 body 在最后一帧之后 hyper 直接 drop body、不会再 poll 出 `None`，`dispatch.rs:382-385`） |
  | `error` | 响应开始后上游出错或空闲超时（§5.4），客户端连接被中断 | `poll_frame` 读到 `Err`，`error` 字段为其 Display |
  | `aborted` | 响应开始后客户端断开 | `LoggedBody` 在上述两种情况之前被 drop |
  | `cancelled` | 响应开始前客户端断开（`handle` future 被 hyper drop，§5.4） | `AccessLog` 的 `Drop`：未写出过就写 `cancelled` |

  `AccessLog` 用一次性标记保证不重复写出；`Relay::handle` 只负责创建 `AccessLog` 并在 `dispatch` 返回后按响应类型选择 `finish` / `stream`，所以每条返回路径都被覆盖。`LoggedBody` 在 `IdleTimeoutBody` 外层，`size_hint` / `is_end_stream` 委托内层，不改变 hyper 的分帧决定。
- 每次上游尝试失败（网络错误、响应头超时、可重试状态码）和匿名模式下带 `Origin` 的请求各有一行 `tracing::warn!`，**都带同一个 `request_id`**，便于与访问日志关联；这些 warn 不属于访问日志（target 不同），T11 按 target 过滤后计数。
- **不记录**请求或响应 body，不记录任何 Key 或 token；path 不含 query，warn 里的 `error` 字段是 hyper-util `legacy::Error` 的 Display（`client error (Connect)` 之类，不含 URI，`hyper-util-0.1.20/src/client/legacy/client.rs:1628`）。
- 不解析 token 用量（解析需要理解响应内容，本期不做；以后可以用只读的旁路观察者实现，同样不改动流）。
- `/_relay/*` 端点（`forward.rs` `relay_endpoint`，**已实施 R4**）：路由顺序是 `/_relay/*` 先于鉴权与 §3 的接口匹配（`Relay::dispatch` 第一步）。`GET` 或 `HEAD /_relay/health` 返回 `200` + `content-type: application/json` + `{"status":"ok"}`，**不需要鉴权**，不暴露任何配置信息（不列上游、不列监听地址、不含版本），从不接触上游；其它方法（如 `POST /_relay/health`）和 `/_relay/` 下的其它路径（`/_relay/`、`/_relay/config`、`/_relay/health/extra`）一律 `404` `relay_not_found`；`/_relay` 之外的路径（含 `/`）按 §3 处理。这些响应同样各写一行访问日志（`interface = -`）。

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
│       ├── breaker.rs       # §5.5 熔断器（时间由调用方传入；R3 起每个 ResolvedUpstream 持有一个）
│       ├── idle.rs          # §5.4 IdleTimeoutBody：响应 body 空闲超时（R3）
│       ├── tls.rs           # §5.6 根证书合并、CryptoProvider
│       ├── upstream.rs      # UpstreamClient enum、UpstreamSet、出站代理连接器
│       ├── forward.rs       # §5 Relay::handle：转发、故障转移循环、AttemptGuard（R2/R3）
│       ├── server.rs        # 入站 accept loop（hyper http1 Builder + service_fn，preserve_header_case）
│       ├── access_log.rs    # §8 访问日志：AccessLog、LoggedBody、ACCESS_TARGET（R4）
│       │                    # （intercept.rs：§7 拦截器，按用户决定暂缓，未加入）
│       └── error.rs         # 中转自身错误响应（RelayErrorKind、relay_error）
│   └── tests/
│       ├── support/mod.rs   # 原始 socket 测试基建：mock 上游、原始客户端、relay 启动
│       ├── passthrough.rs   # T1–T5、T9（鉴权部分）、T13、URL 拼接、中转错误
│       ├── outbound_proxy.rs# T14：mock CONNECT / SOCKS5 代理
│       ├── tls_upstream.rs  # T15：自签 HTTPS 上游 + extra_ca_file
│       ├── failover.rs      # T6、T8（R3）
│       ├── timeouts.rs      # 空闲超时（R3）
│       ├── cancellation.rs  # T7、T12（R3）
│       ├── relay_endpoints.rs # /_relay/health、T9 启动校验（R4）
│       ├── access_log.rs    # T11：捕获用 Subscriber + 8 个场景（R4）
│       ├── shutdown.rs      # 优雅退出：进行中的流完成后才返回、空闲连接不阻塞（R5）
│       └── fixtures/tls/    # 预生成的测试 CA 与 localhost 证书（README.md 记录重新生成方法）
└── cc-proxy/                # 可执行程序（R5，bin 名 cc-proxy）
    ├── config.example.toml  # 示例配置（tests/cli.rs 保证它始终能通过 check）
    ├── src/
    │   ├── main.rs          # clap 子命令 serve / check；日志初始化；信号处理与限时排空
    │   ├── config_file.rs   # 配置路径查找、TOML 读取、${VAR} 展开（§10）
    │   └── check.rs         # dry-run 报告（每个上游的典型路径 URL、鉴权方式、出站方式）、文件权限提示
    └── tests/
        └── cli.rs           # 用 CARGO_BIN_EXE_cc-proxy 跑真实进程的端到端测试（§11）
```

入站不用 `axum::serve`：它内部用 `hyper_util::server::conn::auto::Builder`（`axum-0.7.9/src/serve.rs:19,254`）且不暴露 `preserve_header_case`/`auto_date_header`。**已实施（R2）**：core 自己写 accept loop，用 `hyper::server::conn::http1::Builder`（`timer(TokioTimer)`、`preserve_header_case`）+ `hyper::service::service_fn` 直接调用 `Relay::handle`，**不用 axum / tower**；所有路由判断在 `interface::identify` 内完成。每个连接一个 `tokio::spawn`，单个请求的转发路径上没有任何 spawn，取消语义（§5.4）依然靠 drop 传播。`HeaderCaseMap` 经 `extensions` 在 server → client、client → server 两个方向都能传递（T2、T3 集成测试守护）。

**优雅退出（已实施 R5，`server.rs`）**。`serve(listener, relay, shutdown)` 的语义：

- 连接任务放在一个 `tokio::task::JoinSet` 里（不再是裸 `tokio::spawn`）；accept loop 每轮先 `try_join_next` 回收已结束的任务，避免长期运行时集合无限增长。
- `shutdown` future 完成后：跳出 accept loop 并 drop 监听器（新连接立即被拒），通过 `watch` 通道向每个连接任务发停止信号；连接任务收到后调用 hyper `Connection::graceful_shutdown`——空闲的 keep-alive 连接立即关闭，正在处理的请求（含 SSE 流）继续到结束。信号只会从 `false` 变为 `true` 一次，发送端被 drop 同样视为停止，所以连接建立在信号之前或之后都不会错过。
- 之后 `join_next` 到集合为空，`serve` 才返回。**core 不设超时**：需要限时的调用方自己用 `tokio::time::timeout` 之类包住 `serve`，drop 该 future 即 abort 所有连接任务。
- `cc-proxy serve` 的用法：`shutdown` future 等 SIGINT / SIGTERM（unix；其它平台只等 Ctrl-C），在其内部记"收到退出信号"日志（排空可能在同一轮就完成，日志放在外面会错过）；随后 `select!` 等 `serve` 返回、`--grace-secs`（默认 10）超时或再次收到信号，后两者立即退出进程。退出码：正常退出 0、配置错误 2、运行时错误（绑定失败、运行时创建失败）或强制退出（排空超时、再次收到信号——此时可能截断了进行中的流，用非 0 告知调用方）1。

**`cc-proxy` 可执行程序结构与依赖（已实施 R5）**。`main.rs` 用 clap derive 定义 `serve [--config] [--listen ADDR] [--grace-secs N]` 与 `check [--config]`；`--listen` 覆盖配置后由 `Relay::new` 内的 `validate` 重新校验（含 §6.2 匿名规则）。`serve` 绑定成功后向 **stdout 只打印一行** `listening on ADDR`（`--listen 127.0.0.1:0` 时给脚本 / 测试拿实际端口），所有日志经 `tracing-subscriber` 的 `fmt` + `EnvFilter` 写到 **stderr**，`serve` 默认级别 `info`、`check` 默认 `warn`（报告写 stdout，core 加载证书等环节的告警仍能到 stderr），`RUST_LOG` 覆盖；`RUST_LOG` 设置了但无法解析时回退到该默认级别，并在订阅器安装后记一条 warn 说明原因。`check` 不发请求：加载、校验、`Relay::new`（真实加载 TLS 与 `extra_ca_file`），再对每个上游用该接口的典型路径（`/v1/messages`、`/v1/chat/completions`、`/v1/responses`、`/v1beta/models/...:streamGenerateContent`）调用 `upstream_uri` 打印最终 URL、鉴权方式（`x-api-key` / `bearer` / ...）与出站方式（`direct` / `http-proxy` / `socks5`）；URL 中 query 只保留参数名（`api-version=***`），不打印任何 Key。unix 上配置文件对 group / other 可读（`mode & 0o077 != 0`）时 `check` 与 `serve` 都给 warn。

`cc-proxy` 依赖（`src-tauri/crates/cc-proxy/Cargo.toml`）：

| crate | Cargo.toml 版本 | 锁定版本 | features | default-features | 用途 / 选型理由 |
|---|---|---|---|---|---|
| `cc-proxy-core` | path | — | — | — | 引擎 |
| `clap` | `~4.5` | 4.5.61 | `std`, `derive`, `help`, `usage`, `error-context` | **`false`** | 子命令解析。固定 `~4.5`：4.6 会把 `syn 3` 带进锁文件；关掉 `color` / `suggestions` 以免引入 `anstream` / `strsim` |
| `toml` | `0.8` | 0.8.23 | — | 默认 | 复用锁文件已有的 0.8.23（锁文件另有 toml 0.9，写 `0.9` 会多一套 `toml_edit`）；先 `from_str` 成 `toml::Value` 再 `try_into::<RelayConfig>()` |
| `tokio` | `1` | 1.50.0 | `rt-multi-thread`, `macros`, `net`, `time`, `signal`, `sync` | 默认 | 多线程运行时、`ctrl_c` / `SignalKind::terminate` |
| `tracing` | `0.1` | 0.1.44 | — | 默认 | 日志 |
| `tracing-subscriber` | `0.3` | 0.3.23 | `std`, `fmt`, `env-filter` | **`false`** | 日志输出；`env-filter` 的 `regex` 已在锁文件；不开 `ansi`，避免 `nu-ansi-term` |
| `serde` | `1.0` | 1.0.228 | `derive` | 默认 | 与 core 一致 |
| `thiserror` | `2.0` | 2.0.18 | — | 默认 | `ConfigFileError` |
| `http`（dev） | `1` | 1.4.0 | — | 默认 | `check.rs` 单测用 `identify` 反查典型路径 |
| `tokio`（dev） | `1` | 1.50.0 | `rt-multi-thread`, `macros`, `net`, `io-util`, `time` | 默认 | `tests/cli.rs` 的原始 socket mock 上游与客户端 |

不引入 `dirs`（默认配置路径直接用 `HOME` / `USERPROFILE`）、`tempfile`（测试用 `std::env::temp_dir()` + 进程 id + 计数器）、`tokio-util`（排空用 `JoinSet` + `watch` 即可）。

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
| `tokio`（dev） | `1` | 1.50.0 | `macros`, `rt-multi-thread`, `test-util` | 默认 | **dev-dependency**：集成测试多线程运行时；`test-util` 供 `idle.rs` 单元测试用 `start_paused` 虚拟时钟（R3） |

未纳入（较 v3 删除或推迟）：`axum`、`tower`、`futures-util`（入站不用 axum，流适配用 `http-body-util`）；`rustls-pki-types`（经 `rustls::pki_types` 使用）；`async-trait`（R4 引入拦截器时再加）；`rcgen`（不在锁文件里，T15 改用预生成证书）；`toml`、`clap`、`tracing-subscriber`（属 `cc-proxy` 可执行程序，见上表）。core 自身的 `tokio` 不需要 `signal`，信号处理只在 `cc-proxy` 里。

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

上面只是节选；可直接使用的完整示例在 `src-tauri/crates/cc-proxy/config.example.toml`（`tests/cli.rs` 的 `example_config_passes_check` 保证它始终能通过 `check`），安装、客户端接入与日志说明在 `docs/cc-proxy-usage-zh.md`。

**`${VAR}` 展开（已实施 R5，`config_file.rs`）**：

- 先把整个文件 `toml::from_str` 成 `toml::Value`，再递归遍历，**只对字符串值**展开；表的键、数字、布尔等不碰。不在原始文本上替换，所以变量值里的引号、`]]`、换行都不会破坏 TOML 结构（单测覆盖）。
- `${NAME}` 的 `NAME` 必须匹配 `[A-Za-z_][A-Za-z0-9_]*`；`$${` 是转义，得到字面量 `${`；其它 `$` 原样保留；`${` 没有闭合的 `}` 报错（避免拼错的引用被当作字面量 Key 发出去）。
- **不做二次展开**：替换结果里的 `${...}` 按字面量保留，环境变量的值不能再引用别的变量。
- 变量未定义时报错并给出所在位置（如 `配置文件 x.toml 中 upstreams.claude[0].api_key: 引用了未定义的环境变量 ANTHROPIC_API_KEY`），只出现变量名，不出现任何变量值。
- 展开后 `Value::try_into::<RelayConfig>()`（toml 0.8.23 的 Value 反序列化器与 `deny_unknown_fields`、`SocketAddr`、`PathBuf`、`#[serde(transparent)] Secret`、kebab-case 单元枚举都兼容；代价是错误里没有行列号，所以展开错误自带字段路径）。serde 的类型错误会回显字符串值（如 `invalid type: string "sk-…", expected u64`），而该值可能是展开后的环境变量（`${SECRET}` 误写在数值字段上），因此配置解析错误里的 `string "..."` 一律替换为 `string "***"`（单测 `type_errors_do_not_echo_expanded_values`）；取值校验由 `Relay::new` 内的 `validate` 完成。

配置文件路径查找顺序：`--config` 参数 > 环境变量 `CC_PROXY_CONFIG`（非空） > `$HOME/.cc-proxy/config.toml`（Windows 为 `%USERPROFILE%\.cc-proxy\config.toml`）；都没有时报配置错误（退出码 2）。

- 某个接口没有配置上游时，该接口的请求返回 `503`。
- `cc-proxy check --config <path>` 只校验配置、打印每个上游的 dry-run URL，不发请求；成功退出码 0，配置有误 2。

## 11. 测试与验收

透传正确性是本方案的核心，用"上游镜像"集成测试逐项证明。**mock 上游用原始 `TcpListener` + `httparse`**，记录收到的原始字节（头的大小写、顺序、分帧都可见），并按测试用例手写响应字节（含手写 chunked 分帧）；不用 hyper 做 mock 服务端，它会重新解析和分帧。客户端侧同样用原始 socket 发请求、收原始响应字节后自行去分帧再比较。**已实施（R2）**：基建在 `tests/support/mod.rs`（mock 上游按 `Segment` 序列分段写出响应、记录 `RecordedRequest`；原始客户端；`relay_with` 用 `RelayConfig` 启动中转），`passthrough.rs`、`outbound_proxy.rs`（含 mock CONNECT / SOCKS5 代理）、`tls_upstream.rs` 三个文件共 27 个集成测试。**已实施（R3）**：基建增加 mock 上游的 EOF 观察通道（延迟写出期间 `select!` 读 socket，读到 0 / 错误即上报，`closed_within`）；新增 `failover.rs`（T6、T8 共 14 个）、`timeouts.rs`（空闲超时 3 个）、`cancellation.rs`（T7、T12 共 4 个）。配置的超时单位是秒，测试用 1 秒并以 `< 5 s` 的宽松上界断言；T8 的"跳过"与"探测"拆成两个测试，跳过用 60 秒打开期，避免慢机上在断言前进入半开。**已实施（R4）**：新增 `relay_endpoints.rs`（`/_relay/health` 2 个、T9 启动校验 3 个）与 `access_log.rs`（T11，8 个）。T11 不引入 `tracing-subscriber`（不在锁文件里）：测试文件里自写一个最小 `tracing::Subscriber`（`Capture`），`enabled` 恒真，`event` 用 `field::Visit` 把每个事件的 target 与全部字段收集成字符串（`record_str` / `record_debug`），在 `#[tokio::test]`（current_thread）里用 `tracing::subscriber::set_default` 安装——线程局部即可，因为 `serve`、连接任务与 mock 上游都 `tokio::spawn` 在同一个 `block_on` 线程上，mock 上游用 IP 直连也没有 DNS 的 `spawn_blocking`。访问日志按 `ACCESS_TARGET` 过滤后计数；`aborted` / `cancelled` 由连接任务里的 drop 写出，测试用 `wait_access(n)` 轮询（最长 2 秒）等待，不能在断开后立即断言。

| 编号 | 验收项 |
|---|---|
| T1 | 四个接口各一组：上游收到的 method、path、query（去掉 `key`，其它参数逐字节不变，含 `alt=sse&x=a%20b+c` 用例）、body 与客户端发出的**逐字节相同** |
| T2 | 上游收到的头 = 客户端头 − 入站鉴权头（含非 Bearer 的 `authorization`）− hop-by-hop（含 `proxy-connection`）− `expect` + 上游鉴权头 + 上游 host；头名大小写、顺序（按 §2.1 定义：不同名头按首次出现位置、交错同名头聚合，含一条 `Anthropic-Beta` / `User-Agent` / `anthropic-beta` 交错用例）、重复头条数、非 UTF-8 头值不变；`host` 在原位置且沿用入站拼写（入站 `Host` → 上游 `Host`）；上游鉴权头沿用入站同名头拼写（入站 `Authorization` / `X-Api-Key` → 上游 `Authorization` / `X-Api-Key`），入站无同名头时为小写；`preserve_header_case = false` 时全小写 |
| T3 | 客户端收到的状态码、响应头（去掉 hop-by-hop）、body 与上游返回的相同（按 §2.1 定义域；上游不带 `date` 时允许中转补 `date`；hyper 补的 `transfer-encoding` / `content-length` 在上游存在同名头时沿用其拼写） |
| T4 | SSE 流：上游用原始 socket 分多次、以任意 chunk 边界写出（含把一个事件拆在两个 chunk 里），客户端去分帧后字节相同，且在上游写出第一段后即可收到（不整体缓冲） |
| T5 | `content-encoding: gzip` / `br` 的响应原样转发、不被解压；请求 `accept-encoding` 原样到上游 |
| T6 | 故障转移：首个上游返回 `503` / 连接失败 / 响应头超时时，第二个上游收到的 body 与原请求逐字节相同（头除 `host` 外含大小写与顺序一致）；`400` 不触发切换；最后一个上游返回可重试状态码时直接流式转发（含其 `retry-after`、`x-request-id`）；上游 `429` 之后再遇网络错误时回放缓冲的 `429`（含 `retry-after`）；可重试响应 body 超过 `retry_body_bytes` 且无其它可回放响应时收到 `502`；全部为网络错误时收到 `502`；`max_attempts = 1` 时不切换；`GET /v1/responses/{id}` 遇 `503` 不切换 |
| T7 | 提交点之后上游中断：上游写出响应头与首个 chunk 后关闭连接，客户端收到 `200` 与首个事件但没有 chunked 结束标记，第二个上游**没有**收到请求 |
| T8 | 熔断，拆成两个测试：(1) 连续失败达到阈值后跳过该上游（打开期 60 秒，断言期间不会进入半开）；(2) `open_secs`（1 秒）后放行一次探测；另有全部熔断时返回 `503` `relay_no_upstream` 且不接触上游、按上游独立计数（另一接口的成功不影响）。"只放行一个探测"的并发验证在 `breaker.rs` 单元测试（16 线程并发 `try_acquire` 只有一个拿到 `Probe`；`is_available` 不消耗探测名额） |
| T9 | 入站鉴权：四种携带方式均可通过；同时带正确 `x-api-key` 与错误 `authorization` 通过、反之亦通过；错误 token 返回 `401`；token 不出现在上游请求（头与 query）中；**启动校验（已实施 R4，`relay_endpoints.rs`）**：非回环地址且无 token 时 `Relay::new` 拒绝启动（即使设了 `allow_anonymous`，错误同时包含"必须配置 server.auth_tokens"与"只能与回环地址"）；无 token 且未设 `allow_anonymous` 时拒绝启动；配置了 token 后任意地址都可启动 |
| T10 | **暂缓**（拦截器按用户决定本期不实现，§7）：未安装拦截器时 T1–T5 成立；安装测试拦截器后其修改（改 path、改 body 且 `content-length` 重算、包装 SSE 流、短路返回）生效；被丢弃的重试响应不触发 `on_response` |
| T11 | **已实施（R4，`access_log.rs`）**。请求带 `?session=QUERY-MARKER` / `?key=QUERY-MARKER`、`x-api-key: <中转 token>`、body 含 `SECRET-BODY-MARKER`，上游响应含 `RESPONSE-BODY-MARKER`；每个用例断言捕获到的**全部**事件（含 warn）的所有字段都不包含中转 token、上游 Key、body 标记、响应 body 标记与 query 标记，并且访问日志恰好一条、字段齐全。8 个场景：(1) 正常 `200`：`method` / `path`（无 query）/ `interface` / `upstream` / `attempts = 1` / `status` / `outcome = complete` / `request_bytes` = 请求 body 长度 / `response_bytes` = 客户端收到的 body 长度，`head_ms` / `total_ms` / `error` 字段存在；(2) 故障转移 `503` → 连接失败 → `200`：一条访问日志（`attempts = 3`、`upstream = b`），两条失败 warn 与它带同一个 `request_id`；(3) 中转自身响应：`401`（`interface = -`，`response_bytes` = 错误 JSON 长度）与 `GET /_relay/health` 各一条 `complete`；(4) 无 body 响应：上游 `204` 记 `complete` 而非 `aborted`；(5) 回放缓冲响应：`429` + 连接失败后回放 `429`，`attempts = 2`、`complete`、`response_bytes` 正确；(6) 响应头到达前客户端断开：`cancelled`、`attempts = 1`；(7) SSE 中途客户端断开：`aborted`、`status = 200`、`response_bytes` = 已发出的 15 字节；(8) SSE 中途空闲超时：`error`，`error` 字段含 `no data` |
| T12 | 客户端在响应头到达前断开：上游连接被关闭（mock 上游观察到 EOF），第二个上游不被调用；客户端在上传 body 中途断开：上游未收到请求；客户端在流式响应中途断开：上游连接也被关闭（mock 上游观察到 EOF） |
| T13 | 边界：客户端 chunked 上传 → 上游收到 `content-length` 且 body 相同；客户端带 `expect: 100-continue` → 客户端收到 `100 Continue`、上游未收到 `expect`；`HEAD` 与 `204` 无 body；上游响应 trailer 被丢弃；`GET /v1/models` 不带 `anthropic-version` 透传到 openai_responses 上游、带 `anthropic-version` 透传到 claude 上游；`/v1/models/gemini-2.5-flash:generateContent` 归 gemini、`/v1/models/gpt-4o` 归 openai_responses |
| T14 | 出站代理：经本地 mock HTTP CONNECT 代理与 mock SOCKS5 代理各转发一次，上游收到的字节与直连相同；`socks5h` 下 mock 代理收到的是域名而非 IP；`http://` 无显式端口的上游经 CONNECT 代理时，mock 代理收到 `CONNECT host:80`（§5.6） |
| T15 | TLS：mock HTTPS 上游用自签证书，仅 `extra_ca_file` 指向该 CA 时成功，否则 TLS 失败（R2：返回 `502` `relay_bad_gateway`；R3 起触发故障转移）。证书为 `tests/fixtures/tls/` 下用 openssl 预生成的测试 CA（`ca.pem`，P-256，CA 私钥生成后已丢弃）与 `localhost` 服务端证书（`localhost.pem` / `localhost.key`，SAN 含 `DNS:localhost`、`IP:127.0.0.1`），不引入 `rcgen`（不在锁文件里）；重新生成步骤见 `tests/fixtures/tls/README.md`，三个文件需一并替换 |
| T16 | URL 拼接：`strip_prefix` 命中/未命中、`base_url` 带 query 合并、`check` 对含端点路径的 `base_url` 报错 |
| T17 | **已实施（R5，core `tests/shutdown.rs`，2 个）**。(1) `in_flight_stream_finishes_before_serve_returns`：mock 上游发出 SSE 头与首个事件后延迟 500 ms 再发剩余部分；客户端收到首个事件后触发 `shutdown`，150 ms 后断言 `serve` 任务**仍未结束**、新连接被拒（`connect` 失败），随后客户端收到完整 body `data: 1\n\ndata: 2\n\n`，`serve` 在 2 秒内返回。(2) `idle_connections_do_not_block_shutdown`：一条连上但从不发请求的连接 + 一条完成过一次 keep-alive 请求后保持的连接，触发 `shutdown` 后 `serve` 在 2 秒内返回 |
| T18 | **已实施（R5，`cc-proxy/tests/cli.rs`，7 个）**，用 `std::process::Command` 跑 `env!("CARGO_BIN_EXE_cc-proxy")`，每个测试写独立的 600 权限配置文件。(1) `check_prints_dry_run_urls_without_secrets`：claude 直连上游 + 带 `?api-version=` 与 `socks5h` 代理的 openai_chat 上游，stdout 含 `anthropic -> https://api.anthropic.com:443/v1/messages  auth=x-api-key via=direct` 与 `...openai/v1/chat/completions?api-version=***  auth=bearer via=socks5`、未配置的接口标注 503；stdout / stderr 都不含中转 token、上游 Key 与 `api-version` 的值；600 权限时 stderr 为空。(2) `example_config_passes_check`：`config.example.toml` 在设好其引用的 5 个环境变量后通过 `check`，四个上游都出现在报告中。(3) `check_reports_undefined_variable_by_location`：退出码 2，stderr 含 `upstreams.claude[0].api_key` 与变量名。(4) `check_rejects_public_listener_without_tokens`：`0.0.0.0` + `allow_anonymous` 退出码 2，stderr 含 `server.auth_tokens`。(5) `missing_config_file_is_a_config_error`：不存在的 `--config` 退出码 2，stderr 含该路径。(6) `serve_relays_and_drains_on_sigterm`（unix）：`serve --listen 127.0.0.1:0`，从 stdout 第一行解析地址（20 秒内），原始 socket 发 `POST /v1/messages?q=QUERY-MARKER` 到 mock SSE 上游，收到首个事件后 `kill -TERM`；断言客户端仍收到完整 chunked 结束标记、上游收到的起始行 / `x-api-key: <上游 Key>` / body 逐字节正确且不含中转 token，进程 10 秒内以 0 退出，stderr 含访问日志（`outcome="complete"`）与"收到退出信号"，且不含 token、Key、body 标记、query 标记。(7) `forced_exit_after_grace_returns_non_zero`（unix）：`--grace-secs 0`，同样在收到首个 SSE 事件后 `kill -TERM`，进程 10 秒内退出且退出码为 1（排空期为 0，进行中的流被截断）。`ChildGuard` 在 Drop 里 kill 子进程，失败也不遗留进程 |

另外做一次真实上游冒烟：本地起 `cc-proxy`，让 Claude Code、Codex CLI、Gemini CLI 分别指向它，完成一次带工具调用的流式对话。需要真实 Key，由使用者在本机执行；同时核对 §14 的待确认项。

## 12. 分阶段实施

每一步独立提交，提交前 `cargo fmt --all --check`、`cargo clippy --workspace -- -D warnings`、`cargo test --workspace --features cc-proxy-core/test-util` 全绿。R1 起所有模块在 `lib.rs` 用 `pub mod` 导出并在单元测试中使用，避免 `dead_code` 触发 `-D warnings`。

| 步骤 | 内容 | 验收 |
|---|---|---|
| R1 **已实施** | 纯逻辑模块与单元测试：`config`（含校验规则）、`interface`、`headers`（hop-by-hop、凭据移除、原位 host 替换、`expect` 移除）、`upstream_url`（拼接、query 合并、字节级 `key` 过滤、显式端口）、`auth`（候选收集、常量时间比较、匿名规则）、`breaker`、`error`。子步骤与约束见下方"R1 实施须知" | 单元测试覆盖 §2.2 表格每一行、§3 每条规则（含按头分流、`POST` 限定的 Gemini 冒号路径、微调模型 id）、§4 每个例子、§5.5 状态转换、§6 每条规则；T16 中不依赖网络的部分 |
| R2 **已实施** | `tls`、`upstream`（`UpstreamClient` enum、`UpstreamSet` 按代理设置共享 `Client`、CONNECT/SOCKS 鉴权拼装，§5.6）、`server`（自建 http1 accept loop + `service_fn`，不用 axum，§9）、`forward`（`Relay::handle`：单上游透传，四个接口，流式响应）；接入 R1 的 `auth`；原始 socket 测试基建（`tests/support`）。**验收结果**：T1–T5、T9 鉴权部分、T13、T14、T15 共 27 个集成测试外加若干单元测试全部通过；workspace 全量测试 3200 通过、0 失败、9 忽略。**变异验证**：去掉 `extensions` 搬运后 T2 失败；改成整体缓冲后 T4 失败；对调 `socks5` / `socks5h` 的解析方式后两个 SOCKS 测试都失败。T14 的 CONNECT 用例用带显式端口的上游，`http` 上游缺省端口补 80 由 `upstream_url` 单元测试覆盖。**未实现，留给 R3**：T12 取消测试、故障转移循环、`response_head` / `idle` 超时 | T1–T5、T9（鉴权部分）、T13、T14、T15 |
| R3 **已实施** | 故障转移循环（`max_attempts`、`has_next` 快照、`last_response` 回放最近一次缓冲、`retry_canceled_requests` 保持 `true`、有状态 Responses 调用限 1 次，§5.2）、每上游熔断器接入（`ResolvedUpstream.breaker`、`AttemptGuard`、`is_available`，§5.5）、`response_head_timeout` 与 `idle_timeout`（`IdleTimeoutBody`，§5.4）、取消语义验证。提交：9b2263ebd、03fbf4fc3、b3318e1c4、05e72d924。**验收结果**：T6、T8 共 14 个测试，空闲超时 3 个，T7、T12 共 4 个，全部通过；workspace 全量 3225 个测试通过、0 失败、9 忽略；core 的测试连续跑 3 次都稳定。**变异验证**：让熔断器始终放行，T8 失败；可重试状态不触发故障转移，T6 失败；关掉空闲超时，stalled 测试失败；把上游请求放进 `spawn`，T12 失败 | T6–T8、T12 |
| R4 **已实施（不含拦截器）** | `/_relay/health`（`GET` / `HEAD`，先于鉴权，不暴露配置，其它 `/_relay/*` 404，§8）、T9 启动校验测试、访问日志（`access_log.rs`：`AccessLog` / `LoggedBody`、`ACCESS_TARGET`、四种 `outcome`、warn 带 `request_id`，§8）、`Relay::handle` 拆成 `handle` + `dispatch` 以覆盖每条返回路径。`intercept` 按用户决定暂缓（§7、T10）。提交：13d8ade11、e5458e0b4。**验收结果**：`relay_endpoints.rs` 5 个、`access_log.rs` 8 个全部通过；workspace 全量 3240 个测试通过、0 失败、9 忽略；core 的测试连续跑 2 次都稳定。**变异验证**：日志 `path` 带上 query 后 T11 失败；"已知长度 body 被误记为 `aborted`"（hyper 在最后一帧后直接 drop body、不再 poll 出 `None`）这个缺陷由 T11 场景 (1) 发现，修正为在 `poll_frame` 里每读完一个 data 帧就检查 `is_end_stream` | T9（启动校验）、T11；T10 暂缓 |
| R5 **已实施** | `cc-proxy` 可执行程序（§9、§10）：`config_file.rs`（路径查找、TOML → `Value` → 只展开字符串值的 `${VAR}` → `RelayConfig`）、`check.rs`（dry-run 报告、权限提示）、`main.rs`（clap `serve` / `check`，stdout 一行 `listening on ADDR`，日志到 stderr，SIGINT / SIGTERM 限时排空）；core `server.rs` 改为 `JoinSet` + `watch` + `graceful_shutdown`，进行中的流完成后 `serve` 才返回；`config.example.toml` 与 `docs/cc-proxy-usage-zh.md`（三个 CLI 的接入、§6.2 匿名风险、日志说明）。提交：9de481721（bin、配置加载、check）、5d79be7d4（core 优雅退出 + `tests/shutdown.rs`）、28757c5c7（serve、日志、`tests/cli.rs`）、9525bb377（示例配置与使用说明）、ba16ab849（审查后的次要修复：`check` 初始化日志到 stderr 默认 `warn`、解析错误中的字符串值替换为 `***`、强制退出返回 1、`RUST_LOG` 无法解析时回退并告警）。**验收结果**：workspace 全量 3258 个测试通过、0 失败、9 忽略（ba16ab849 前）；`cc-proxy` 共 18 个测试（`config_file` 8、`check` 3、`tests/cli.rs` 7）。**变异验证**：去掉连接任务的 `graceful_shutdown` 调用后空闲连接测试（T17-2）超时；去掉 `serve` 末尾的 `join_next` 排空后进行中的流测试（T17-1）失败；让程序收到信号后立即退出时 SIGTERM 端到端测试（T18-6）报告流被截断 | T17、T18 通过；真实上游冒烟与 §14 核对由使用者执行 |

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
| C5 | 使用说明（`docs/cc-proxy-usage-zh.md` "待实际验证"）中的 Codex 接入写法：`[model_providers.<id>]` 用 `base_url = "http://127.0.0.1:15721/v1"`、`wire_api = "responses"`、`env_key = "CC_PROXY_TOKEN"`，`env_key` 在所用版本中是否生效（桌面端接管写的是 `auth.json` 的 `OPENAI_API_KEY` 占位符，`src-tauri/src/services/proxy.rs:232`，与 `requires_openai_auth` 回退是两条路，说明里推荐 `env_key`） | 决定使用说明 Codex 一节是否要改 |
| C6 | Gemini CLI（API Key 模式）：`~/.gemini/.env` 的 `GOOGLE_GEMINI_BASE_URL` / `GEMINI_API_KEY`（与桌面端接管一致，`src-tauri/src/live/project/gemini.rs:24-25`）加 `settings.json` 的 `security.auth.selectedType = "gemini-api-key"` 是否足够；token 经 `x-goog-api-key` 头还是 `?key=` 发出（两种中转都接受，§6.1） | 决定使用说明 Gemini 一节是否要改 |
| C7 | Claude Code：`ANTHROPIC_AUTH_TOKEN`（`Authorization: Bearer`）与 `ANTHROPIC_API_KEY`（`x-api-key`）二选一指向中转后，带工具调用的流式对话与 `/model` 列表（C1 开关开启时）正常；Responses 有状态用法（`previous_response_id`，C3）在多上游故障转移下不保证连续，说明里已注明 | 决定使用说明 Claude 一节与"限制"一节是否要改 |

## 15. 修订记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v1 | 2026-09-29 | 初稿 |
| v3.4 | 2026-09-30 | 按 R5 实施结果同步（对照 `crates/cc-proxy/src/{main,config_file,check}.rs`、`crates/cc-proxy/tests/cli.rs`、core `server.rs` 与 `tests/shutdown.rs`、`config.example.toml`、`docs/cc-proxy-usage-zh.md`，提交 9de481721、5d79be7d4、28757c5c7、9525bb377，以及审查后修复 ba16ab849）：状态改为 R1–R5 全部实施，基线更新；§9 目录补 `cc-proxy` 实际结构（`check.rs`、`config.example.toml`、`tests/cli.rs`）与 core `tests/shutdown.rs`，写明 `serve` 的优雅退出语义（`JoinSet` + `try_join_next` 回收、`watch` 停止信号、hyper `graceful_shutdown`、core 不设超时由调用方包裹）、`cc-proxy serve` / `check` 的行为（stdout 只一行 `listening on ADDR`、日志到 stderr、`serve` 默认 `info` / `check` 默认 `warn`、`RUST_LOG` 无法解析时回退并告警、退出码 0 / 2 / 1 且强制退出记 1、限时排空与再次信号即退）与依赖表（`clap ~4.5` 精简 features 及固定 4.5 的理由、`tracing-subscriber` 不开 `ansi`、`toml 0.8` 复用锁文件、不引入 `dirs` / `tempfile` / `tokio-util`）；§10 写明 `${VAR}` 实际规则（解析后只展开字符串值、不二次展开、`$${` 转义、未闭合报错、未定义变量按位置报错且不印值、解析错误中的字符串值替换为 `***`、`Value::try_into` 兼容性）与配置路径查找顺序，指向 `config.example.toml` 与 `docs/cc-proxy-usage-zh.md`；§11 新增 T17（shutdown 2 个）与 T18（cc-proxy 端到端 7 个，含强制退出返回 1）及其验证点；§12 R5 标"已实施"，写入 5 个提交、验收结果（workspace 3258 通过 / 0 失败 / 9 忽略，cc-proxy 18 个测试）与三项变异验证；§14 新增 C5–C7，同步使用说明中"待实际验证"的 Codex `env_key`、Gemini `.env` / `selectedType` 与 token 携带方式、Claude Code 两种凭据变量 |
| v3.3 | 2026-09-29 | 按 R4 实施结果同步（对照 `access_log.rs`、`forward.rs` 与 `tests/{access_log,relay_endpoints}.rs`，提交 13d8ade11、e5458e0b4）：§7 拦截器标"按用户决定暂缓，本期不实现"，设计内容保留作参考；§8 按 `access_log.rs` 重写：字段表、`ACCESS_TARGET`、四种 `outcome`（`complete` / `error` / `aborted` / `cancelled`）的语义与写出时机（含已知长度 body 在最后一帧后被 hyper 直接 drop 的处理）、`request_id` 格式 `r-{n:x}` 且不写入响应头、warn 带 `request_id`、`/_relay/health` 的 `GET` / `HEAD` 免鉴权与其它方法 / 路径 404；§9 目录补 `access_log.rs` 与 R3/R4 测试文件，去掉 `intercept.rs`；§11 记录 T11 的捕获用 Subscriber 与 `set_default` 在 current_thread 下可用的理由，T9 启动校验标"已实施"，T10 标"暂缓"，T11 写入 8 个场景；§12 R4 标"已实施（不含拦截器）"并写入提交、验收结果（workspace 3240 通过 / 0 失败 / 9 忽略，core 连跑 2 次稳定）与两项变异验证 |
| v3.2 | 2026-09-29 | 按 R3 实施结果同步（对照 `forward.rs`、`idle.rs`、`breaker.rs`、`upstream.rs` 与 `tests/{failover,timeouts,cancellation}.rs`）：§5.2 "全部失败"规则改为回放最近一次完整缓冲的 `last_response`，只有完全没有可回放响应时才 `502`，一次都没尝试（全部熔断）时 `503`；§5.2 补 `max_attempts = min(配置, 上游数)`、有状态 `GET/DELETE /v1/responses/*` 只试 1 次且网络错误也不换上游、最后一次尝试遇可重试状态码直接流式转发但仍记失败、`has_next` 用 `is_available` 快照及其并发等价性、`AttemptGuard` drop 归还许可、`retry_canceled_requests` 保持默认 `true` 的理由；§5.4 `response_head_timeout` / `idle_timeout` 标"已实施"，`IdleTimeoutBody` 同时用于流式回传与缓冲错误 body，错误表更新 `NoUpstream` / `BadGateway` 触发点；§9 目录补 `idle.rs`，dev `tokio` 加 `test-util`；§11 T6/T7/T8/T12 按实际测试改写（T8 拆成跳过与探测两个测试，T12 补流式中途断开），记录新增三个测试文件与 EOF 观察基建；§12 R3 标"已实施"并写入提交、验收结果与变异验证 |
| v3.1 | 2026-09-29 | 按 R1/R2 实施结果同步（对照 `src-tauri/crates/cc-proxy-core/` 代码与测试）：模块 `url.rs` 改名 `upstream_url.rs`（与外部 crate `url` 重名，§9、§12）；Gemini `/v1/models/{name}:{action}` 仅 `POST` 归 gemini，`GET` 含冒号的 OpenAI 微调模型 id 按 `/v1/models/{id}` 分流（§3）；入站服务改为 hyper `service_fn` 直接调用 `Relay::handle`，不用 axum / tower，路由全在 `interface::identify`（§3、§9）；§5.1 顺序改为鉴权 → 识别接口 → 读 body，R2 单上游版本与错误码落点；中转自身错误 `type` 统一 `relay_` 前缀并列表（§3、§5.4）；`/_relay/*` R2 一律 404、`/_relay/health` R4 实现（§8）；`connect_timeout` 已实施、`response_head` / `idle` 超时与 `retry_canceled_requests` 取舍归 R3（§5.2、§5.4）；§5.6 出站代理各条标"已实施"，显式端口由 `upstream_url` 单测覆盖；§4 `UpstreamConfig` 字段按实际类型（`Secret`、`Option<AuthScheme>`、`Option<String>`）；§9.1 依赖表按 core 实际 `Cargo.toml` 重写（补 `base64`、`percent-encoding`、`http-body`、`cc-switch-domain`，dev 依赖 `httparse`、`tokio-rustls`、`tokio`；删 `axum`、`tower`、`futures-util`、`rustls-pki-types`、`async-trait`，`toml`/`clap`/`tracing-subscriber` 归 R5）；T15 改用 `tests/fixtures/tls/` 预生成证书、不引入 `rcgen`（§11）；§11 记录测试基建与 27 个集成测试；§12 R1、R2 标"已实施"并写入 R2 验收结果（27 集成测试 + workspace 3200 通过 / 0 失败 / 9 忽略）与变异验证 |
| v3 | 2026-09-29 | 按 Fable 二次审查（源码抽样核对 26 处 + `/tmp/relay-proto` 原型实测）修订：P1 `HeaderCaseMap` 按头名配对拼写，core 重写的 `host` / 鉴权头沿用入站拼写（§2.4、T2、T3）；P2 头顺序定义改为"同名聚合、不同名按首次出现位置"（§2.1、T2）；P3 `default-features = false` 在 workspace 构建中不能避免 provider 二义，唯一保护是 `builder_with_provider` 并禁用 `builder()` 系列，rustls 补 `std`，hyper-rustls 去掉 `webpki-tokio`/`native-tokio`（§9.1、§9.2、§13）；P4 `Tunnel` CONNECT 端口缺省 443，core 按 scheme 补端口（§5.6、T14、R2）；P5 四种连接器为不同 `Client` 类型，用 enum 统一，`with_auth` 签名差异，`connect_timeout` 不覆盖握手（§5.6、R2）；P6 Claude Code 2.1.260 实证会发 `GET /v1/models?limit=1000`，§3 改为按 `anthropic-version` 头分流并写入 `identify(method, path, headers)` 签名（§3、§13、§14 C1、T13）；P7 §6.1 "同时发两个头"改引 Claude Code 自身代码；P8 §2.3 Cargo.toml 行号改为 61-63；P9 `ct_eq` 不等长自行短路（§6.1）；P10 `on_response` 在提交点之前（§7）；P11 hyper-util `retry_canceled_requests` 内部重试与 body drop 排空行为（§5.2、R3）；P12 axum 单 `fallback` 路由（§3、§9、R2）；P13 Codex 0.155.1 对 `/models` 404 的行为与 `previous_response_id` 字符串证据（§14 C2、C3）；R1 实施须知并入 §12，R1 只引入纯逻辑依赖，`auth` 接入提前到 R2（§12）；取消语义、流式、100-continue、chunked→content-length 原型实测结论记入 §2.4、§5.4、§9 |
| v2 | 2026-09-29 | 按 Fable 审查修订：S1 `last_response` 保留与回放（§5.2）；S2 `GET /v1/models` 纳入 openai_responses、Codex 其它路径明确排除（§3）；S3 依赖表、`default-features = false`、CryptoProvider（§9）；I1 分帧头、`expect`、`content-length`、`via`、定义域（§2.1–2.3）；I2 Responses 有状态限制（§5.2）；I3 鉴权候选规则、默认拒绝匿名、浏览器风险（§6）；I4 字节级 `key` 过滤（§6.3）；I5 `base_url` query 合并、`strip_prefix` 规则、`check` 校验（§4）；I6 拦截器执行顺序与类型（§7）；I7 提交点、超时改名、取消依赖 drop 且禁止 spawn（§5.2、§5.4）；I8 出站代理纳入 R2（§5.6）；A1 默认保留头名大小写与顺序（§2.4）；A2 仅 HTTP/1.1、`proxy-connection`（§2.2–2.3）；A3 Gemini 路由与 OAuth 说明（§3）；A4 200 MiB（§5.1）；A5 系统证书 + `extra_ca_file`（§5.6）；A6 原始 socket mock 与边界用例 T12–T16（§11）；A7 `pub mod` 导出（§12）；A8 529/429 说明；A9 路由顺序（§8）；A10 待冒烟确认（§14） |
