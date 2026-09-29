# cc-proxy 使用说明

`cc-proxy` 是一个本地模型接口中转：Claude Code、Codex CLI、Gemini CLI 等工具指向它，它把请求**原样**转发给配置的上游，再把响应原样流回。中转只替换鉴权头（换成上游 Key），不改动请求和响应内容。设计细节见 [passthrough-relay-design-zh.md](passthrough-relay-design-zh.md)。

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

## 校验

```bash
export CC_PROXY_TOKEN=... ANTHROPIC_API_KEY=... # 以及配置中引用的其它变量
cc-proxy check
```

`check` 会加载并校验配置、加载 TLS 证书，然后逐个上游打印最终请求的 URL、鉴权方式和出站方式，不发任何请求，也不打印 Key；URL 中 query 参数只保留名字。配置有误时退出码为 2。

## 运行

```bash
cc-proxy serve                      # 使用配置中的监听地址
cc-proxy serve --listen 127.0.0.1:0 # 随机端口
```

- 启动成功后 stdout 只输出一行 `listening on ADDR`，便于脚本读取实际端口；日志全部写到 stderr。
- 收到 Ctrl-C 或 SIGTERM 后停止接受新连接，等待进行中的请求（包括流式响应）结束，最长 `--grace-secs` 秒（默认 10）；期间再收到一次信号立即退出。
- 退出码：配置错误 2，运行时错误（如端口被占用）1。
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
INFO cc_proxy_core::access: relay request request_id=r-2 method=POST path=/v1/messages interface="claude" upstream="anthropic" attempts=1 status=200 head_ms=412 total_ms=5210 request_bytes=18234 response_bytes=40211 outcome="complete" error="-"
```

- `outcome`：`complete` 完整发出；`error` 上游在响应中途出错或空闲超时；`aborted` 客户端在响应中途断开；`cancelled` 客户端在收到响应前断开。
- 失败的尝试另有 warn 日志，带同一个 `request_id`。
- 日志**不包含**请求 / 响应 body、Key、token 与 query。
- 用 `RUST_LOG` 调整级别，例如 `RUST_LOG=warn cc-proxy serve` 只看告警，`RUST_LOG=cc_proxy_core::access=info,warn` 只保留访问日志与告警。

## 限制

- 只支持 HTTP/1.1（入站与出站）。
- 中转不做协议转换、不改模型名、不改请求或响应内容；需要这些能力时要用改写组件（尚未提供）。
- 按 `/v1/responses/{id}` 读取或删除响应只发给第一个可用上游，不做故障转移。
