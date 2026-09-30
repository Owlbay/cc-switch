# cc-proxy 使用说明

`cc-proxy` 是一个本地模型接口中转：Claude Code、Codex CLI、Gemini CLI 等工具指向它，它把请求转发给配置的上游，再把响应流回。默认**原样透传**：只替换鉴权头（换成上游 Key），不改动请求和响应内容。上游显式声明了另一种协议或模型映射时，该上游改走**协议转换**（见[协议转换](#协议转换)）。设计细节见 [passthrough-relay-design-zh.md](passthrough-relay-design-zh.md)、[protocol-conversion-design-zh.md](protocol-conversion-design-zh.md)。

支持的接口：

| 接口 | 路径 | 典型客户端 |
|---|---|---|
| Anthropic Messages | `POST /v1/messages`、`/v1/messages/count_tokens`；带 `anthropic-version` 头的 `GET /v1/models` | Claude Code |
| OpenAI Responses | `/v1/responses`、`/v1/responses/*`；不带 `anthropic-version` 的 `GET /v1/models` | Codex CLI |
| OpenAI Chat Completions | `POST /v1/chat/completions` | OpenAI 兼容客户端 |
| Gemini | `/v1beta/*`、`POST /v1/models/{name}:{action}` | Gemini CLI（API Key 模式） |

## 构建

```bash
cd src-tauri
cargo build --release -p cc-proxy
```

产物在 `src-tauri/target/release/cc-proxy`。

## 配置

1. 复制示例配置并收紧权限：

   ```bash
   mkdir -p ~/.cc-proxy
   cp src-tauri/crates/cc-proxy/config.example.toml ~/.cc-proxy/config.toml
   chmod 600 ~/.cc-proxy/config.toml
   ```

2. 按需保留 `[[upstreams.*]]`。每个接口可以配多个上游，按顺序故障转移；某个接口没有上游时，它的请求返回 `503`。
3. Key 与 token 建议通过环境变量提供：配置中的 `${VAR}` 在加载时展开，未定义会报错并指出所在位置（例如 `upstreams.claude[0].api_key`），不会打印变量值。`$${` 表示字面量 `${`。
4. 配置文件位置：`--config` 参数 > 环境变量 `CC_PROXY_CONFIG` > `~/.cc-proxy/config.toml`。

`base_url` 只写到服务根路径，中转会把入站 path 原样拼在后面：

| 上游 | 配置 |
|---|---|
| `https://api.deepseek.com` | `base_url = "https://api.deepseek.com"` → `/v1/chat/completions` |
| 路径已含版本段的上游 | `base_url = "https://ark.cn-beijing.volces.com/api/v3"`，`strip_prefix = "/v1"` → `/api/v3/chat/completions` |
| 需要固定 query 的上游（如 Azure） | `base_url = "https://x.openai.azure.com/openai?api-version=2024-10-21"`，query 合并在入站 query 之前 |

## 协议转换

让 Claude Code 或 Codex CLI 使用说另一种协议的上游，例如 Claude Code 接 DeepSeek（OpenAI Chat），或 Codex CLI 接 Anthropic。转换是**上游的属性**：在该上游上声明 `protocol`（上游说的协议）或 `model_map`（模型名映射）即开启，同一接口下的其他上游仍原样透传，可以混排并按顺序故障转移。

| 客户端（接口） | 可用的上游协议 |
|---|---|
| Claude Code（`claude`） | `claude`、`openai_chat`、`openai_responses`、`gemini` |
| Codex CLI（`openai_responses`） | `claude`、`openai_chat`、`openai_responses`、`gemini` |

`openai_chat`、`gemini` 接口下的上游只能透传（这两种协议作为客户端的转换尚未提供）。

```toml
[[upstreams.claude]]                # Claude Code → DeepSeek
id = "deepseek-for-claude"
base_url = "https://api.deepseek.com"
api_key = "${DEEPSEEK_API_KEY}"
protocol = "openai_chat"            # 上游协议；缺省等于所在接口
default_max_output_tokens = 8192    # 客户端没给输出上限时使用，缺省 16384
[upstreams.claude.model_map]        # 客户端模型名 -> 上游模型名；"*" 兜底，缺省不改名
"*" = "deepseek-chat"

[[upstreams.openai_responses]]      # Codex CLI → Anthropic
id = "anthropic-for-codex"
base_url = "https://api.anthropic.com"
api_key = "${ANTHROPIC_API_KEY}"
protocol = "claude"
model_map = { "*" = "claude-sonnet-4-5" }

[[upstreams.claude]]                # Claude Code → Gemini
id = "gemini-for-claude"
base_url = "https://generativelanguage.googleapis.com"
api_key = "${GEMINI_API_KEY}"
protocol = "gemini"
model_map = { "*" = "gemini-2.5-pro" }
```

- **鉴权头按上游协议取缺省**：`openai_chat` / `openai_responses` 上游用 `Authorization: Bearer`，`claude` 用 `x-api-key`，`gemini` 用 `x-goog-api-key`；可用 `auth` 覆盖。
- **同协议 + `model_map`（恒等转换）**：只改请求与响应里的模型名，其余字段原样保留；但 body 会经 JSON 重新序列化，流式响应会重新成帧（SSE 注释行、`id:` / `retry:` 字段不保留）。不配 `model_map` 时仍逐字节透传。
- **只转换主端点**：Claude 客户端只转换 `POST /v1/messages`，Codex 客户端只转换 `POST /v1/responses`。`count_tokens`、`/v1/models`、按 id 读取响应等端点在其他协议没有等价物，会跳过转换上游，交给同接口的透传上游；没有透传上游时返回 400 `relay_conversion_unsupported`。
- **无法表达的请求不发给上游**：以下请求会跳过转换上游，所有上游都跳过时返回 400 `relay_conversion_unsupported`：Responses 的 `previous_response_id` / `conversation`（服务端状态无法在别的上游重建）、`n > 1`、`logprobs`、结构化输出（`text.format` 为 JSON schema）、PDF 等文档发给 Chat / Responses 上游、图片 URL 发给 Gemini 上游（Gemini 只接受内联图片）。
- **可降级的内容静默处理**：`cache_control`、Codex 的 `store` / `include` / `prompt_cache_key` / `reasoning.summary` 等字段被忽略；Claude Code 默认携带的 WebSearch 工具发给 Chat 上游时被剔除，发给 Responses 上游时映射为内置 `web_search`，发给 Gemini 上游时映射为 `googleSearch`；Codex 的 `apply_patch`（custom 工具）会降级为参数 `{input}` 的函数工具，响应时再还原。
- **推理签名的往返**：上游返回的 thinking 签名、`encrypted_content`、Gemini `thoughtSignature` 会被封装成 `ccsw1.` 开头的字符串带给客户端，客户端下一轮原样带回时还原给同一种上游。签名无法回放时（换了协议不同的上游、或客户端没带回），发往 Anthropic 上游的请求会自动关闭 thinking，而不是报错。
- **不要在同一会话里混用转换上游与同协议透传上游**：故障转移到透传上游时，客户端历史里的 `ccsw1.` 签名、或来自 Chat 上游的无签名 thinking 块，会被 Anthropic 等上游拒绝（400）。需要故障转移时，让备用上游也走同一种转换。
- **Gemini 注意事项**：Gemini 2.5 Pro 不能关闭 thinking，客户端明确关闭 thinking 时该模型会返回 400；Gemini 3 要求回放 `thoughtSignature`，从别处带来的无签名历史可能被拒绝。
- **上游压缩**：转换请求不带 `accept-encoding`；上游仍返回压缩内容时返回 502 `relay_conversion_error`（不计入熔断）。
- `cc-proxy check` 对转换上游打印上游协议的端点，并在行尾标注 `convert=<客户端>-><上游>`（跨协议时附 `default_max_output_tokens`）。

## 校验

```bash
export CC_PROXY_TOKEN=... ANTHROPIC_API_KEY=... # 以及配置中引用的其它变量
cc-proxy check
```

`check` 会加载并校验配置、加载 TLS 证书，然后逐个上游打印最终请求的 URL、鉴权方式和出站方式，不发任何请求，也不打印 Key；URL 中 query 参数只保留名字。配置有误时退出码为 2。加载证书等环节的告警写到 stderr（默认只输出 warn 及以上）。

## 运行

```bash
cc-proxy serve                      # 使用配置中的监听地址
cc-proxy serve --listen 127.0.0.1:0 # 随机端口
```

- 启动成功后 stdout 只输出一行 `listening on ADDR`，便于脚本读取实际端口；日志全部写到 stderr。
- 收到 Ctrl-C 或 SIGTERM 后停止接受新连接，等待进行中的请求（包括流式响应）结束，最长 `--grace-secs` 秒（默认 10）；期间再收到一次信号立即退出。
- 退出码：正常退出 0；配置错误 2；运行时错误（如端口被占用）或强制退出（排空超时、再次收到信号，进行中的流可能被截断）1。
- `GET /_relay/health` 返回 `{"status":"ok"}`，不需要 token，可用于探活。

## 客户端接入

下面以中转监听 `127.0.0.1:15721`、客户端 token 为 `$CC_PROXY_TOKEN` 为例。中转接受四种 token 携带方式：`x-api-key`、`Authorization: Bearer`、`x-goog-api-key`、Gemini 的 `?key=`，任一匹配即可；这些凭据都不会转发给上游。

### Claude Code

`~/.claude/settings.json`：

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721",
    "ANTHROPIC_AUTH_TOKEN": "<CC_PROXY_TOKEN 的值>"
  }
}
```

`ANTHROPIC_AUTH_TOKEN` 以 `Authorization: Bearer` 发送；也可以改用 `ANTHROPIC_API_KEY`（以 `x-api-key` 发送），二者只保留一个。模型名原样透传，需是 `claude` 上游能识别的名字。

### Codex CLI

`~/.codex/config.toml`：

```toml
model_provider = "cc-proxy"

[model_providers.cc-proxy]
name = "cc-proxy"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
env_key = "CC_PROXY_TOKEN"
```

Codex 会在 `base_url` 后拼 `/responses`，启动时探测 `GET /v1/models`，都会路由到 `openai_responses` 上游。

### Gemini CLI（API Key 模式）

`~/.gemini/.env`：

```bash
GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:15721
GEMINI_API_KEY=<CC_PROXY_TOKEN 的值>
```

`~/.gemini/settings.json` 中 `security.auth.selectedType` 设为 `"gemini-api-key"`。Gemini CLI 的 OAuth（Code Assist）模式走 Google 自己的端点，不能指向中转。

### 待实际验证

以下行为来自代码与文档推断，尚未用真实客户端逐一验证，首次使用时请留意日志：

- Claude Code 的模型列表发现（`GET /v1/models?limit=1000`）在对应开关开启时才会请求。
- Codex CLI 的 `env_key` 写法在所用版本中是否生效；Responses 的 `previous_response_id` 等有状态用法在多上游故障转移时不保证连续（按 id 读取 / 删除只发第一个可用上游）。
- Gemini CLI 的 token 通过 `x-goog-api-key` 头还是 `?key=` 发送（两种中转都接受）。

## 安全

- **token 与上游 Key 是两回事**：客户端只持有中转 token，上游 Key 只在中转配置里。
- **默认拒绝匿名访问**：未配置 `auth_tokens` 时必须显式设置 `allow_anonymous = true`，且只能监听回环地址；监听非回环地址时必须配置 token。
- **匿名模式的风险**：回环地址上任何本机进程、同机其它用户，以及浏览器页面向 `http://127.0.0.1:15721` 发起的简单请求（如 `Content-Type: text/plain` 的 POST，不触发 CORS 预检）都能使用你的上游 Key。匿名模式下收到带 `Origin` 头的请求会记一条 warn 日志。建议始终配置 token。
- **配置文件权限**：文件对其他用户可读时，`check` 与 `serve` 会给出警告，建议 `chmod 600`。

## 日志

每个请求在 stderr 输出一行访问日志（target `cc_proxy_core::access`），例如：

```
INFO cc_proxy_core::access: relay request request_id=r-2 method=POST path=/v1/messages interface="claude" upstream="anthropic" convert="-" attempts=1 status=200 head_ms=412 total_ms=5210 request_bytes=18234 response_bytes=40211 outcome="complete" error="-"
```

- `convert`：透传为 `-`，转换时为方向，如 `claude->openai_chat`、恒等转换 `claude->claude`。

- `outcome`：`complete` 完整发出；`error` 上游在响应中途出错或空闲超时；`aborted` 客户端在响应中途断开；`cancelled` 客户端在收到响应前断开。
- 失败的尝试另有 warn 日志，带同一个 `request_id`。
- 日志**不包含**请求 / 响应 body、Key、token 与 query。
- `RUST_LOG` 写错时回退到默认级别并输出一条告警。
- 用 `RUST_LOG` 调整级别，例如 `RUST_LOG=warn cc-proxy serve` 只看告警，`RUST_LOG=cc_proxy_core::access=info,warn` 只保留访问日志与告警。

## 限制

- 只支持 HTTP/1.1（入站与出站）。
- 透传上游不改模型名、不改请求或响应内容；协议转换只在上游显式声明 `protocol` / `model_map` 时发生。
- 协议转换是有损的：跨协议无法表达的字段被丢弃（见设计文档 §5.4），Chat 上游的推理内容没有签名，不能回放给 Anthropic 上游。
- 按 `/v1/responses/{id}` 读取或删除响应只发给第一个可用上游，不做故障转移。
