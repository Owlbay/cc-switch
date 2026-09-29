# 本地代理独立化设计方案（第一步：模型代理）

> 状态：草案 v3.1（已按 P0 实施结果同步） · 基线：`origin/main@846de29c1`（2026-09-28）
> 范围：把 `src-tauri/src/proxy` 抽成不依赖 Tauri / SQLite 的代理引擎 crate，并提供可单独运行的 headless 程序 `cc-proxy`。
> 第二步（请求 / 响应信息改写管线）只在第 10 节给出预留接口，不在本方案实施范围内。
>
> **2026-09-29 方向调整**：目标改为“四个模型接口完全透传的中转，改写由独立组件控制”。P2 及之后“搬迁现有代理”的路线由 [`passthrough-relay-design-zh.md`](passthrough-relay-design-zh.md) 取代；本文的调研结论、P0 与已完成的 P1 前两步（`AppType`、共享常量迁入 domain）继续有效，P1 其余步骤暂停。
> 文中行号均指基线 `846de29c1`，路径相对 `src-tauri/`。

## 1. 背景与结论

### 1.1 选型结论

调研了 Rust 生态中的 7 个候选项目（cocoonstack/gateway、api7/aisix、lightseekorg/smg、RESMP-DEV/ccr-rust、raine/claude-code-proxy、a1re1/claudecodex、okhsunrog/llm-relay），结论是：**不引入外部项目作为底座，以 cc-switch 自身的 `proxy` 模块为基础独立化**。原因：

| 维度 | 说明 |
|---|---|
| 许可证 | cc-switch 为 MIT；cocoonstack/gateway、ccr-rust 为 AGPL-3.0，不可合入 |
| 定位 | aisix、cocoonstack、smg 面向多租户服务端 / 推理集群，依赖 etcd、Redis、账本等，与本地场景不符 |
| 覆盖面 | 本地编码 CLI 需要 Messages / Responses / Chat / Gemini `v1beta` 四类入站及 Codex / Copilot OAuth，现有 proxy 覆盖最全 |
| 体量 | 候选项目 5 万～40 万行，引入成本高于解耦现有代码 |

借鉴但不复制代码的设计：

- cocoonstack/gateway：分层 DAG（preprocess → account_select → model_access → post_process）——用于第二步改写管线。
- aisix：同协议字节透传、跨协议才转换；Bridge 按“厂商专用 → 协议族默认”两级分发；pre / during / post_call 钩子。
- raine/claude-code-proxy（MIT）：Codex Responses 转换的边界处理，可作为对照测试来源。

### 1.2 可行性依据（基线实测）

- `src/proxy` 共 76,774 行（统计方法：`find src/proxy -name '*.rs' | xargs cat | wc -l`），`#[test]` / `#[tokio::test]` 共 1,388 个（统计方法：§6.4 脚本的 `test fns` 行）；依赖 `Database` 的测试集中在 `provider_router`、`server`、`response_processor`、`usage/logger`、`forwarder` 五个文件（精确数字由 §6.4 脚本统计）。
- 67 个 `.rs` 文件中有 48 个（约 72%；全部 transform / streaming、sse、熔断器、各类 rectifier / optimizer）**没有任何 crate 外部依赖**。统计方法：对每个文件 `grep -v 'crate::proxy' | grep -qE '\bcrate::[a-z_]+'` 不命中即计入，含测试代码，因此是保守值。
- 非测试代码中的外部耦合集中在 **19 个文件**（与 §6.2 列表一一对应，附录 A 脚本在基线上输出 95 条命中、恰好覆盖这 19 个文件），详见第 4 节。

### 1.3 上游同步约束

本仓库是 `farion1231/cc-switch` 的 fork。按 `git log --since=2026-07-30 --until=2026-09-28 --oneline origin/main -- src/proxy` 统计，该区间内有 67 个提交改动 `src/proxy`（`forwarder.rs` 占 17 个），`--since=2026-09-15` 起有 17 个，其中包含 `mode` 模块重构。该统计以本仓库 `origin/main` 为准，包含 fork 自身合入的提交，只用于说明改动频率，不区分上游 / 本地来源。因此：

1. **先原地解耦，再物理搬迁**；搬迁单独成一个只含 `git mv` 与可见性调整的提交。
2. 每个 PR 前先同步 `origin/main`。
3. 优先考虑把 P0～P3 作为 RFC 回馈上游，被接受则冲突问题消失。

## 2. 目标与非目标

### 2.1 独立程序 `cc-proxy` 的能力范围

| 包含 | 不包含（留在桌面端） |
|---|---|
| 全部入站路由（与桌面端路径一致，见 `server.rs` 路由表） | 客户端配置写入、直连 / 代理模式切换（`mode/*`、`services/proxy.rs`） |
| 协议转换、流式、rectifier、optimizer、熔断、故障转移 | 托盘、前端事件、UI |
| API Key 类上游：Claude、OpenAI 兼容、Gemini、Codex API Key、Grok Build API Key | 读取 `~/.codex` 下的模型目录 |
| 用量解析与计费，可选写入 SQLite | Claude Desktop 3P 网关（token / 模型路由）——第二阶段 |
| 入站 Bearer token 鉴权（新增） | OAuth 登录流程 UI |
| 第二阶段：Copilot / Codex OAuth / xAI OAuth token 刷新 | |

### 2.2 非目标

- 不改变桌面端任何可观察行为（路由、日志内容、计费结果、事件名）。日志内容的验收口径见 §9.5。
- 不做多租户、配额、计费账本。
- 第一步不引入新的请求改写能力，只把已有改写逻辑原样保留。

## 3. 目标结构

```
src-tauri/
├── Cargo.toml                    # 新增 [workspace]，members = ["crates/*"]（根 package 自动成为成员）
├── Cargo.lock                    # 位置不变（workspace 根仍是 src-tauri/）
├── src/                          # cc-switch 桌面 app（包名不变）
│   ├── proxy/mod.rs              # P3 后仅 `pub use cc_proxy_core::*;`，保持 crate::proxy:: 引用不变
│   └── proxy_host/               # 新增：桌面端宿主实现
│       ├── mod.rs                # 组装 DesktopProxyHost（在 ProxyService::start / restart 时按当前 app_handle 组装）
│       ├── store.rs              # 包装 Database + mode::current
│       ├── usage.rs              # usage/logger 中的 SQL 迁移至此
│       ├── events.rs             # Tauri emit、usage_events
│       ├── failover.rs           # mode::controller::record_failover_route + 托盘刷新
│       ├── codex_live.rs         # CodexLiveAuthHook：codex_config 的 live auth IO
│       └── catalog.rs            # Codex 模型目录（IO）
└── crates/
    ├── cc-switch-domain/         # 纯类型与纯函数，无 IO
    ├── cc-proxy-core/            # 代理引擎 lib
    └── cc-proxy/                 # headless 二进制
```

依赖方向：`cc-switch (app) → cc-proxy-core → cc-switch-domain`；`cc-proxy (bin) → cc-proxy-core`。

硬约束（CI 断言）：`scripts/check-proxy-core-deps.sh`（无参数，任意目录执行；P0 已落地并接入 `ci.yml:155-157`）。`cargo tree -i` 的实际行为（cargo 1.95 实测）：

- `cargo tree -p cc-proxy-core -e normal -i tauri` 在该包不依赖 `tauri` 时**退出码 101**，stderr 为 `package ID specification \`tauri\` did not match any packages`，stdout 为空。v3 所写"`nothing to print`、退出码 0"只在不带 `-p`（对根包）时出现，对 `-p <member>` 不成立。
- `-p` 传入不存在的包名时 cargo 忽略 `-p`、输出整个 workspace 的依赖树，stdout 非空 → 判失败，方向安全。
- 依赖名拼错时同样得到 `did not match any packages`，会被误判为通过。

因此脚本分两步：先对根 manifest 跑 `cargo tree -e normal -i <dep> --depth 0`，确认 `tauri`、`rusqlite` 确实在 workspace 依赖图里（否则以 2 退出）；再对 `cc-switch-domain`、`cc-proxy-core` 逐个检查——stdout 非空判失败，退出码 101 且 stderr 含 `did not match any packages` 判通过，其它非零退出码判失败。三种情况均已实测。

`test-hooks` feature（`Cargo.toml:20`）在 `src/proxy` 内没有任何引用，只属于 app，core / domain 不声明该 feature。

## 4. 耦合点清单（基线 846de29c1，非测试代码）

| # | 耦合点 | 位置 | 替换为 |
|---|---|---|---|
| 1 | `mode::current::provider_in_use(&db, app)`（同步，`mode/current.rs:67`，内部读 `live-state.json` + SQLite） | `handler_context.rs:110`、`provider_router.rs:48` | `ProviderStore::provider_in_use` |
| 2 | `mode::current::is_proxy(app)`（同步，`mode/current.rs:35`） | `failover_switch.rs:82` | `ProviderStore::is_proxy_mode` |
| 3 | `db.get_proxy_config_for_app`（async，`dao/proxy.rs:140`）、`get_all_providers`（同步，`dao/providers.rs:20`）、`get_failover_queue`（同步，`dao/failover.rs:23`）、**`update_provider_health_with_threshold`（async 写，`dao/proxy.rs:520`）** | `provider_router.rs:69,89,96,162,178,281` | `ProviderStore::app_proxy_config` / `failover_candidates` / **`record_provider_health`** |
| 4 | `db.get_rectifier_config` / `get_optimizer_config` / `get_copilot_optimizer_config`（均同步，`dao/settings.rs:244,267,290`） | `handler_context.rs:106-108` | `ProviderStore::runtime_configs` |
| 5 | 用量 SQL：`lock_conn!`、`find_model_pricing_row`（`services/usage_stats.rs:2046`，参数 `&rusqlite::Connection`）、`get_pricing_model_source`（async，`dao/proxy.rs:95`）、`PRICING_SOURCE_*` | `usage/logger.rs:5-8,101,336,357`；`UsageLogger<'a> { db: &'a Database }`（`logger.rs:90-92`）的全部构造点：`handlers.rs:2778,2824`、`response_processor.rs:641` | `UsageSink`；SQL 迁移到 `proxy_host/usage.rs`；三个构造点改为 `UsageLogger::new(&*state.host)` |
| 6 | `sql_helpers::is_cache_inclusive_app`、`INPUT_TOKEN_SEMANTICS_*`（`services/sql_helpers.rs:23-32`，纯常量 / 纯函数）；`usage_stats::is_placeholder_pricing_model`（`:2096`，纯函数） | `usage/calculator.rs:63`、`usage/logger.rs:7,124,397` | 移入 domain，`services::sql_helpers` / `services::usage_stats` 反向 re-export |
| 7 | `usage_events::notify_log_recorded()` | `usage/logger.rs:218` | `ProxyEvents::usage_logged` |
| 8 | 故障转移副作用：`try_state::<AppState>` → `mode::controller::record_failover_route`（`mode/controller.rs:693`）、托盘刷新、`emit("provider-switched")` | `failover_switch.rs:92-118`；app 侧调用点 `commands/proxy.rs:351-357` | `FailoverHandler::switch_to`；去重逻辑保留在 core |
| 9 | `app_handle.state::<CodexOAuthState / CopilotAuthState / XaiOAuthState>()`（newtype 定义在 `commands/codex_oauth.rs:19`、`commands/copilot.rs:14`、`commands/xai_oauth.rs:13`） | `forwarder.rs:1213,1396,1735,1786,1855,2710,2744`（`state::<…>()` 取用）。`app_handle` 在 forwarder 非测试代码中共 **13 处**：字段 `:168`、构造参数 `:248`、四处 `tokio::spawn` 内 `fm.try_switch(ah.as_ref(), …)`（`:568,670,818,980`）、七处 OAuth 取用（`:1210,1395,1734,1785,1854,2707,2739`） | `ProxyHost` 直接持有三个 manager 的 `Arc`（见 §5.4）；四处 `try_switch` 调用随 §5.3 签名改为传 `host` |
| 10 | `codex_config::codex_live_auth_matches_managed_request`（`codex_config.rs:577`，读 `~/.codex/auth.json`） | `forwarder.rs:1223` | `CodexLiveAuthHook::live_session_matches` |
| 11 | `claude_desktop_config::map_proxy_request_model`（`claude_desktop_config.rs:687`，纯函数，返回 `Result<Value, AppError>`） | `forwarder.rs:1249` | 纯函数移入 domain `claude_desktop_routes`，返回 domain 错误（见 §5.5） |
| 12 | Codex OAuth manager 内的 live auth IO：`sync_codex_managed_oauth_live_auth_after_refresh`（`:1002`）、`clear_codex_live_auth_for_managed_account`（`:1405,1478`）、`prepare_codex_live_auth_for_managed_account_removal`（`:1400,1471`）、`ensure_codex_live_auth_unchanged_for_managed_account`（`:226`，经 `CodexLiveAuthSwitchGuard::ensure_unchanged -> Result<(), AppError>`）、`read_codex_live_auth_refresh_for_managed_account`（`:804`，返回 `Option<CodexManagedLiveRefresh>`，该结构体定义在 `codex_config.rs:183-188`，`pub(crate)`）；`codex_managed_oauth_auth_value`（`:995`，纯函数） | `providers/codex_oauth_auth.rs` | 注入 `CodexLiveAuthHook`；`codex_managed_oauth_auth_value` 移入 domain；`CodexManagedLiveRefresh` 整体移入 core `host.rs` 并改名 `LiveRefresh`（字段不变，`pub`），`codex_config.rs` 以 `pub(crate) use` 保持原名可用；`ensure_unchanged` 改返回 `ProxyError::Host`（见 §5.4） |
| 13 | `/models` 读取 Codex 模型目录（`get_codex_config_dir`、`read_codex_config_text`、`resolve_cc_switch_catalog_path`、`read_codex_model_catalog_text`，均为文件 IO） | `handlers.rs:89-100` | `ModelCatalog::codex_models` |
| 14 | Claude Desktop：`model_list_response`（`claude_desktop_config.rs:651`，纯函数）、`get_or_create_gateway_token(db)`（`:279`，读写 DB） | `handlers.rs:161,275` | `model_list_response` 移入 domain；`ProviderStore::gateway_token` |
| 15 | Codex / Grok 纯解析：`extract_codex_base_url`（`:953`，用 `toml`）、`extract_codex_auth_api_key`（`:933`）、`extract_codex_api_key`（`:941`）、`extract_codex_experimental_bearer_token`（`:2324`）、`strip_codex_unified_session_bucket`（`:2416`，用 `toml_edit`，返回 `Result<String, AppError>`）、`codex_url_host_matches_any`（`:123`）、`CodexCatalogToolProfile`（`:230`，依赖 `model_capabilities::image_input_capability_from_modalities`）、`extract_codex_id_token_subject / _user_identity`（`:357,361`，用 `base64` + `serde_json`）、`grok_config::extract_{base_url,credentials,model_config}`（`grok_config.rs:175,209,231`）、`pi_config::provider_base_url`（`pi_config/mod.rs:260`，返回 `Result<String, AppError>`） | `providers/codex.rs:259-989`、`providers/codex_oauth_auth.rs`、`provider.rs:179-222` | 移入 `domain::codex_parse` / `domain::grok_parse` / `domain::pi_parse`，原模块 re-export；`AppError` 返回值改为 `DomainError`（§4.2） |
| 16 | 常量 `CODEX_OFFICIAL_PROVIDER_ID`（`dao/providers_seed.rs:15`）、`PRICING_SOURCE_REQUEST/RESPONSE`（`dao/proxy.rs`）、`ONE_M_CONTEXT_MARKER`（`claude_desktop_config.rs:39`） | `providers/codex.rs:293`、`handlers.rs:50`、`response_processor.rs:16`、`model_mapper.rs:5`、`usage/logger.rs:5` | 移入 domain，原位置 re-export |
| 17 | `crate::redact_url_for_log` / `redact_url_for_log_with_secrets` / `redact_url_origin_for_log`（`lib.rs:175,181,219`） | `forwarder.rs:2305,2307,3773` | 移入 `cc_proxy_core::util::redact`，`lib.rs` re-export |
| 18 | `model_capabilities::{is_confirmed_text_only_model, image_input_capability_from_settings, ImageInputCapability}`（均 `pub(crate)`，`model_capabilities.rs:11-68`） | `media_sanitizer.rs:2-4`（第 1 行的 `#[cfg(test)]` 只修饰第 2 行） | `model_capabilities.rs` 整体移入 domain 并改 `pub`；其 `:202` 引用 `ONE_M_CONTEXT_MARKER`，随 #16 一并处理 |
| 19 | `crate::commands::{CodexOAuthState, …}` 类型引用 | `forwarder.rs:25` | 随 #9 消失 |
| 20 | `AppError`（`NoProvidersConfigured`、`AllProvidersCircuitOpen` 等） | `provider_router.rs:7,128,131`、`handler_context.rs:144-147`、`failover_switch.rs:8,44`、`codex_oauth_auth.rs:224`、`usage/logger.rs:6` | core 内改用 `ProxyError`（已有同名变体 `proxy/error.rs:36-39`）；宿主 trait 返回 `anyhow::Result`，app 侧负责转换 |
| 21 | `crate::provider::{Provider, ProviderMeta, CodexChatReasoningConfig, LocalProxyRequestOverrides, AuthBinding…}` | `handler_context.rs:6`、`provider_router.rs:8`、`model_mapper.rs:6`、`media_sanitizer.rs:4`、`forwarder.rs:28-31`（跨行 `use crate::{…}`）、`providers/{adapter,claude,codex,gemini,mod,transform_codex_chat}.rs` | 整体移入 domain（见 4.1） |
| 22 | `AppType` | `handler_context.rs:5`、`handlers.rs:49`、`handler_config.rs:5`、`provider_router.rs:5`、`providers/mod.rs:44`、`failover_switch.rs:79`、`forwarder.rs:29` | 枚举本体移入 domain（见 4.1、4.2） |

core 内部自身的全局状态（不属于耦合点，但独立程序必须知道）：

- `http_client.rs:14-20` 三个 `OnceCell` 全局（`GLOBAL_CLIENT`、`CURRENT_PROXY_URL`、`CC_SWITCH_PROXY_PORT`），由 `http_client::init`（桌面端在 `lib.rs:1201,1219` 调用）与 `set_proxy_port` 初始化；app 内 `services/{balance,coding_plan,skill,stream_check,subscription,…}.rs` 20 余处直接使用 `http_client::get()`。P3 后它们留在 core，`cc-proxy` 启动时必须调用 `http_client::init(outbound_proxy_url)`，否则 `get()` 走未初始化路径。
- `hyper_client.rs:60,580` 的 `OnceLock` 客户端 / TLS 连接器，无外部输入，无需处理。

### 4.1 `Provider` / `AppType` 的处理

- `Provider` 原样移入 domain。transform 层读取 `meta` 的十几个字段（`api_format`、`codex_chat_reasoning`、`local_proxy_request_overrides`、`max_output_tokens`、`custom_user_agent`、`auth_binding` 等），重新定义精简类型会改动上百处且与上游冲突。
- `Provider::resolve_usage_credentials`（`provider.rs:145-222`）**并非 IO**，只是调用 #15 的纯解析函数（`codex_config::extract_codex_api_key / extract_codex_base_url`、`grok_config::extract_*`、`pi_config::provider_base_url`）。随 #15 一起移入 domain 后该方法可原样保留在 domain 的 `impl Provider`，不需要 `ProviderLiveExt`。`provider.rs` 中没有其它 `crate::` 依赖（仅 `settings::CustomEndpoint` 与 `app_config::AppType`）。
- `ProviderMeta.custom_endpoints: HashMap<String, crate::settings::CustomEndpoint>`（`provider.rs:447`）：`CustomEndpoint`（`settings.rs:13-19`）为纯数据，一并移入 domain，`settings` 模块 re-export。
- `AppType`：枚举与 `as_str / is_additive_mode / supports_local_proxy / all`（`app_config.rs:414-465`）及 `impl FromStr`（`:467-490`）移入 domain；`app_config.rs` 中与 `SkillStore`、`prompt`、`MultiAppConfig` 相关的部分保留在 app。仓库中**不存在** `impl Display for AppType`，无需迁移。
- 孤儿规则核对：全仓库对 `Provider` / `ProviderMeta` / `AppType` / `CustomEndpoint` / `CodexCatalogToolProfile` 的 `impl` 块只出现在各自定义文件内（无 `rusqlite::ToSql/FromSql`、无 `From<…> for AppError`），唯一需要处理的是 `FromStr for AppType` 的 `Err` 类型，见 4.2。
- 所有迁移均以 `pub use cc_switch_domain::…` 保持原路径可用，app 其余代码零修改（P1 阶段）。

### 4.2 domain 的错误类型

`impl FromStr for AppType` 的 `type Err = AppError`（`app_config.rs:468`），失败时返回 `AppError::localized("unsupported_app", zh, en)`。`AppError` 留在 app（`error.rs:105` 有 `From<rusqlite::Error>`），domain 不能引用它；而 `FromStr` 与 `AppType` 都将是 app 之外的项，`impl` 只能写在 domain。同样返回 `AppError` 的还有 `strip_codex_unified_session_bucket`（`codex_config.rs:2416-2419`，`AppError::Message`）、`pi_config::provider_base_url`（`pi_config/mod.rs:260`）、`proxy_model_routes / map_proxy_request_model`（`claude_desktop_config.rs:561-720`，`AppError::localized`）。

方案：

```rust
// crates/cc-switch-domain/src/error.rs
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// 对应 AppError::Localized，Display 必须与 error.rs:52 的 "{zh} ({en})" 逐字一致
    #[error("{zh} ({en})")]
    Localized { key: &'static str, zh: String, en: String },
    /// 对应 AppError::Message，Display 为 "{0}"
    #[error("{0}")]
    Message(String),
}

impl FromStr for AppType { type Err = DomainError; /* 文案原样搬迁 */ }
```

```rust
// src/error.rs（app 侧，AppError 是本地类型，允许写 From）
impl From<cc_switch_domain::DomainError> for AppError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::Localized { key, zh, en } => AppError::Localized { key, zh, en },
            DomainError::Message(m) => AppError::Message(m),
        }
    }
}
```

- app 内 32 处 `AppType::from_str(..).map_err(|e| e.to_string())?` / `?`（如 `commands/config.rs:70`、`commands/provider.rs:29,35,47`、`commands/skill.rs:24`）：`?` 走 `From` 自动转换；`map_err(|e| e.to_string())` 依赖 Display，`DomainError` 与 `AppError` 的 `#[error]` 模板相同，输出逐字一致。
- **不提供** `impl From<DomainError> for ProxyError`。原因：core 内两处消费 `DomainError` 的调用点各自有不同的包装变体（`forwarder.rs:1249-1250` 用 `InvalidRequest(e.to_string())`，`handlers.rs:161-162` 用 `ConfigError(e.to_string())`），若先经 `From` 统一映射到某个变体再按原分支包装，Display 会变成 `"无效的请求: 无效的请求: …"`。这两处保持原写法，只把闭包里的 `e` 从 `AppError` 换成 `DomainError`（见 §5.5）。
- P1 验收增加一条：对 `AppType::from_str("bogus")`、`strip_codex_unified_session_bucket("=")` 等错误路径写快照测试，断言 `to_string()` 与基线一致。现有 `tests/app_type_parse.rs:27` 已对 `from_str` 的 `err.to_string()` 做断言，可直接复用。

### 4.3 domain 的依赖集合

按上述迁移文件的 `use` 实测：

| crate | 用途 |
|---|---|
| `serde`、`serde_json` | 全部类型的 derive；`Provider.settings_config: Value` |
| `http` | `ProviderMeta::custom_user_agent_header`（`provider.rs:1,600`） |
| `indexmap` | `ProviderManager`（`provider.rs:2,261`） |
| `toml` | `extract_codex_base_url`（`codex_config.rs:954`）、`codex_top_level_model` |
| `toml_edit` | `strip_codex_unified_session_bucket`（`codex_config.rs:16,2416`） |
| `base64` | `extract_codex_id_token_*`（`codex_config.rs:10`） |
| `thiserror` | `DomainError` |

`providers/codex.rs` 整体留在 core（其 `:19` 的 `regex` 属于 core 依赖）；`is_codex_official_provider`（`:292`）只是引用 domain 常量 `CODEX_OFFICIAL_PROVIDER_ID`，本身不迁移。

不允许出现：`tauri*`、`rusqlite`、`axum`、`hyper`、`reqwest`、`tokio`、`once_cell`（`codex_config.rs:11` 的 `OnceCell` 属于 IO 部分，留在 app）。`scripts/check-proxy-core-deps.sh` 对 domain 与 core 同时检查 `tauri`、`rusqlite`（§3）；`tokio axum reqwest` 不在脚本里（前置检查要求依赖名存在于 workspace 图中，这三者 app 都依赖，可按需追加到脚本的 `dep` 列表）。

### 4.4 core 的依赖集合

按 `src/proxy` 非测试代码的 `use` / 路径引用实测（括号内为使用该 crate 的文件数），版本与根包 `Cargo.toml:25-85` 逐项一致，避免 workspace 内出现两个版本：

| 用途 | crate |
|---|---|
| 序列化 / 数据 | `serde`、`serde_json`（`preserve_order`）、`indexmap`、`rust_decimal`(3)、`uuid`(7，`v4`)、`chrono`(7)、`base64`(7)、`sha2`(4)、`url`(5)、`regex`(1，`providers/codex.rs:19`)、`toml`(1) |
| 异步 / HTTP 服务端 | `tokio`（`macros, rt-multi-thread, time, sync`）、`futures`、`bytes`、`axum`(12)、`tower`(1)、`http`、`http-body-util`、`httparse`(1) |
| 上游客户端 | `reqwest`(9，`rustls-tls, json, stream, socks`)、`hyper`(2)、`hyper-util`、`hyper-rustls`(1)、`tokio-rustls`(1)、`rustls`(1)、`webpki-roots`(1)、`rustls-native-certs`(1) |
| 内容编码 | `flate2`(2)、`brotli`(1)、`zstd`(2) |
| 基础设施 | `log`(33)、`thiserror`、`anyhow`（宿主 trait 返回值）、`once_cell`(1，`http_client.rs:14-20`)、**`async-trait`（根包当前没有该依赖，P2a 引入 `host.rs` 时新增，仅 core 需要）** |
| dev-dependencies | `tempfile`、`serial_test`（与根包 `Cargo.toml:119-121` 一致） |

`tower-http`、`dirs`、`tauri*`、`rusqlite` 不进入 core；CI 用 §3 的脚本对 `tauri`、`rusqlite` 判空。`chrono` 只用于时间戳，不带 `serde` feature 也可，但为避免 feature 统一问题与根包保持相同 features。

## 5. 宿主 trait 设计（`cc-proxy-core/src/host.rs`）

设计原则：**签名的 sync / async 与被替换的 DAO 调用保持一致**，避免在持锁上下文里引入 `block_on`；trait 内不重做现有业务判断（回退、默认值、去重），这些留在 core。

### 5.1 结构

```rust
pub struct ProxyHost {
    pub store: Arc<dyn ProviderStore>,
    pub usage: Arc<dyn UsageSink>,
    pub events: Arc<dyn ProxyEvents>,
    pub failover: Arc<dyn FailoverHandler>,
    pub catalog: Arc<dyn ModelCatalog>,
    pub codex_live: Arc<dyn CodexLiveAuthHook>,
    /// OAuth manager 是 core 自己的类型（providers/{codex_oauth_auth,copilot_auth,xai_oauth_auth}.rs），
    /// 直接注入实例，不再经 AppHandle::state 取用。None 时与现在 app_handle == None 的分支行为一致。
    pub codex_oauth: Option<Arc<CodexOAuthManager>>,
    pub copilot: Option<Arc<RwLock<CopilotAuthManager>>>,
    pub xai: Option<Arc<RwLock<XaiOAuthManager>>>,
}
```

三个 `Option<Arc<…>>` 的类型与 `commands/codex_oauth.rs:19`、`commands/copilot.rs:14`、`commands/xai_oauth.rs:13` 中 newtype 的内部字段完全相同，桌面端组装 host 时直接 `state.0.clone()`。

### 5.2 `ProviderStore`

```rust
#[async_trait]
pub trait ProviderStore: Send + Sync {
    /// 替换 #1（mode::current::provider_in_use，同步）。桌面实现内部读 live-state.json + SQLite，与现状相同。
    fn provider_in_use(&self, app: &AppType) -> anyhow::Result<Option<Provider>>;
    /// 替换 #2（mode::current::is_proxy，同步）
    fn is_proxy_mode(&self, app: &AppType) -> bool;
    /// 替换 get_proxy_config_for_app（async）。返回值沿用 types::AppProxyConfig（原 DAO 返回类型）
    async fn app_proxy_config(&self, app_type: &str) -> anyhow::Result<AppProxyConfig>;
    /// 替换 get_all_providers（同步，dao/providers.rs:20）
    fn all_providers(&self, app_type: &str) -> anyhow::Result<IndexMap<String, Provider>>;
    /// 替换 get_failover_queue（同步，dao/failover.rs:23）。只返回按 sort_index 排好的 provider_id
    fn failover_queue(&self, app_type: &str) -> anyhow::Result<Vec<String>>;
    /// 替换 update_provider_health_with_threshold（async 写，dao/proxy.rs:520）
    async fn record_provider_health(
        &self,
        provider_id: &str,
        app_type: &str,
        success: bool,
        error_msg: Option<String>,
        failure_threshold: u32,
    ) -> anyhow::Result<()>;
    /// 替换 #4 三个同步读取；失败时各自 unwrap_or_default，与 handler_context.rs:106-108 相同
    fn runtime_configs(&self) -> RuntimeConfigs;
    /// 替换 #14 get_or_create_gateway_token(db)（同步，原返回 Result<String, AppError>）。
    /// Ok(None) 表示宿主没有 Claude Desktop 网关能力；Err 对应原来的 AppError。
    fn gateway_token(&self) -> anyhow::Result<Option<String>> { Ok(None) }
}
```

`gateway_token` 的三种结果在 `handlers.rs:270-295 validate_claude_desktop_gateway_auth` 中的处理：

| 返回 | 行为 |
|---|---|
| `Ok(Some(expected))` | 与现状相同：常量时间比较 token（§7.4） |
| `Err(e)` | 与现状 `handlers.rs:276` 相同：`ProxyError::AuthError(e.to_string())` |
| `Ok(None)` | **新增分支，仅 headless 端会走到**：返回 `ProxyError::AuthError("Claude Desktop gateway 未启用".to_string())`，请求被拒绝（HTTP 401，与其它 `AuthError` 一致） |

桌面端 `DesktopProxyHost::gateway_token` 永远返回 `Ok(Some(_))` 或 `Err(_)`（直接转发 `get_or_create_gateway_token`），不会走到第三行，行为不变。

`provider_router.rs` 的队列合并、`provider_supports_failover` 判断、熔断计数（`:69-135`）全部留在 core，不放进 trait；因此不再需要文档 v1 的 `failover_candidates`，改为把 DAO 的两个读取原样映射。

### 5.3 `UsageSink` / `ProxyEvents` / `FailoverHandler`

```rust
#[async_trait]
pub trait UsageSink: Send + Sync {
    /// 替换 UsageLogger::log_request 中的 SQL（logger.rs:100-221）。core 先算出 UsageSemantic 与 cost，
    /// 再把完整行交给 sink；request_id 碰撞检测（load_existing_semantic）属于 SQL，一并下沉
    fn record(&self, log: &RequestLog) -> anyhow::Result<RecordOutcome>;
    /// 替换 get_model_pricing（logger.rs:335，同步）
    fn model_pricing(&self, model_id: &str) -> anyhow::Result<Option<ModelPricing>>;
    /// 替换 db.get_pricing_model_source（async，dao/proxy.rs:95）。
    /// claude-desktop → claude 的回退与非法值告警（logger.rs:349-370）留在 core
    async fn pricing_model_source(&self, app_type: &str) -> anyhow::Result<String>;
}

pub struct RecordOutcome { pub inserted: bool, pub collision: bool }

pub trait ProxyEvents: Send + Sync {
    /// 替换 #7；core 在 RecordOutcome.inserted 为 true 时调用，时序与 logger.rs:211-218 相同
    fn usage_logged(&self) {}
}

#[async_trait]
pub trait FailoverHandler: Send + Sync {
    /// 替换 failover_switch.rs:92-118 的整段副作用：record_failover_route → 托盘刷新 → emit("provider-switched")。
    /// 返回 true 表示 record_failover_route 返回了 true；实现方负责这三步的顺序与现状一致。
    /// 只传 AppType；事件负载里的字符串由实现方用 app.as_str() 取得（app_config.rs:415），
    /// 与 failover_switch.rs:79 先 parse 再用 app_type 字符串的现状等价。
    /// 错误：record_failover_route 返回的 String 原样放进 anyhow::Error（anyhow::anyhow!(msg)），不加 context。
    async fn switch_to(&self, app: &AppType, provider_id: &str, provider_name: &str)
        -> anyhow::Result<bool>;
}
```

`FailoverSwitchManager::try_switch` 的去重（`failover_switch.rs:47-71`）、`app_type.parse::<AppType>()` 与 `is_proxy_mode` 前置检查留在 core，签名改为 `try_switch(&self, host: &ProxyHost, app_type: &str, provider_id, provider_name) -> Result<bool, ProxyError>`；`switch_to` 的 `Err(e)` 在 core 内映射为 `ProxyError::Host(e.to_string())`（§5.6），使 `commands/proxy.rs:357` 的 `"[Recovery] 自动切换失败: {e}"` 输出与现在 `AppError::Message` 的 `"{0}"` 逐字一致。**app 侧受影响调用点**：`commands/proxy.rs:351-357`（恢复检测时 `FailoverSwitchManager::new().try_switch(Some(&app_handle), …)`），P2b 改为从 `ProxyService` 取当前 host；core 内四处 `tokio::spawn` 调用（`forwarder.rs:568,670,818,980`）改为 clone `Arc<ProxyHost>` 传入。

### 5.4 OAuth：直接注入 manager，hook 只保留 codex_config IO

forwarder 对三个 manager 的调用（`forwarder.rs:1214 chatgpt_account_id_for_account`、`:1797-1804 default_account_id / get_valid_token_for_account`、`:1401-1402 get_api_endpoint / get_default_api_endpoint`、`:1863-1864 xai get_valid_token_for_account / get_valid_token`、`:2718-2720 fetch_models_for_account / fetch_models`、`:2744-2760`）全部是 manager 自身的公开方法，manager 类型随 core 迁移，**forwarder 逻辑零改动**，只把 `self.app_handle.as_ref()…state::<XxxState>()` 换成 `self.host.codex_oauth.as_ref()` 等。

真正需要抽象的只有 `codex_oauth_auth.rs` / `forwarder.rs` 对 `codex_config` 文件 IO 的调用：

```rust
/// 原 codex_config.rs:183-188 的 CodexManagedLiveRefresh，字段原样，随 hook 移入 core
pub struct LiveRefresh {
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub last_refresh_ms: Option<i64>,
    pub chatgpt_account_id: String,
}

pub trait CodexLiveAuthHook: Send + Sync {
    /// forwarder.rs:1223 codex_live_auth_matches_managed_request(account_id, request_access_token)
    fn live_session_matches(&self, account_id: &str, access_token: &str) -> Result<bool, ProxyError>;
    /// codex_oauth_auth.rs:804 read_codex_live_auth_refresh_for_managed_account
    fn read_live_refresh(&self, account_id: &str, managed_id_token: Option<&str>)
        -> Result<Option<LiveRefresh>, ProxyError>;
    /// codex_oauth_auth.rs:1002 sync_codex_managed_oauth_live_auth_after_refresh（原返回 Result<bool, _>，
    /// 调用点 :1002-1010 只区分 Ok / Err，bool 丢弃）
    fn sync_after_refresh(&self, account_id: &str, expected_refresh_token: &str, refreshed_auth: &Value)
        -> Result<(), ProxyError>;
    /// codex_oauth_auth.rs:1400,1471 prepare_codex_live_auth_for_managed_account_removal
    fn prepare_removal(&self, account_id: &str, managed_id_token: Option<&str>) -> Result<(), ProxyError>;
    /// codex_oauth_auth.rs:1405,1478 clear_codex_live_auth_for_managed_account
    fn clear_live_auth(&self, account_id: &str) -> Result<(), ProxyError>;
    /// codex_oauth_auth.rs:226 ensure_codex_live_auth_unchanged_for_managed_account(account_id, expected_refresh_token)
    fn ensure_unchanged(&self, account_id: &str, expected_refresh_token: &str) -> Result<(), ProxyError>;
}
```

- **错误类型统一用 `ProxyError::Host(String)`（§5.6 新增，`#[error("{0}")]`），不能用 `AuthError`**：`AuthError` 的 Display 是 `"认证失败: {0}"`（`proxy/error.rs:74-75`），而这六个函数的错误在现状中都是以 `{error}` / `to_string()` 原文嵌入更外层文案的——`forwarder.rs:1223-1229` `format!("Codex OAuth 会话校验失败: {error}")`、`codex_oauth_auth.rs:804-808` `CodexOAuthError::TokenFetchFailed(error.to_string())`、`:1002-1010` 的 `log::warn!`、`codex_config.rs:636-661` 返回的 `AppError::Message` 经 `ensure_unchanged` 直达前端。用 `Host` 包装后 `to_string()` 就是原文，文案不变。
- `CodexOAuthManager::new(data_dir, hook: Arc<dyn CodexLiveAuthHook>)`；桌面实现 `proxy_host/codex_live.rs` 逐一转发到 `codex_config`，把 `AppError` 转成 `ProxyError::Host(e.to_string())`；独立程序第一阶段用 `NoopCodexLiveAuth`（全部返回 `Ok(false)` / `Ok(None)` / `Ok(())`）。
- `CodexLiveAuthSwitchGuard::ensure_unchanged`（`codex_oauth_auth.rs:224`）返回值由 `AppError` 改为 `ProxyError`；app 侧新增 `impl From<ProxyError> for AppError`，映射规则见 §5.6，使 `services/provider/codex_direct.rs:597-612`（`run_with_edits -> Result<OperationReport, AppError>`，`outgoing.ensure_unchanged(account)?`）中的 `?` 继续可用且文案不变。
- 桌面端 manager 构造点：`store.rs`（`CodexOAuthManager`）、`lib.rs:1164`（`CopilotAuthManager`）、`lib.rs:1186`（`XaiOAuthManager`）。
- **app 侧受 P2c 影响的调用点**（都是 manager 的 `pub(crate)` 方法或类型，P3 需改 `pub`，签名不变）：
  - `services/provider/codex_direct.rs`：`CodexLiveAuthSwitchGuard`、`ensure_unchanged`、`resolve_codex_catalog_tool_profile`、`is_codex_official_provider`
  - `services/provider/live.rs`：`CodexLiveAuthSwitchGuard`、`prepare_live_auth_for_account_switch_away`（`:1173`）、`get_valid_token_bundle_for_account`（`:1047`）、`ensure_account_exists`（`:1899`）
  - `services/provider/codex_editor.rs`、`services/codex_oauth_models.rs`（含 `CODEX_OAUTH_CLIENT_VERSION` / `CODEX_OAUTH_ORIGINATOR`）、`commands/{auth,codex_oauth,copilot,xai_oauth}.rs`、`store.rs`

### 5.5 `ModelCatalog` 只保留 IO；Claude Desktop 纯函数移入 domain

`claude_desktop_config.rs` 的 `proxy_model_routes`（`:561`）、`model_list_response`（`:651`）、`map_proxy_request_model`（`:687`）、`is_claude_safe_model_id`（`:237`）及其私有辅助函数只读 `provider.meta.claude_desktop_model_routes`，没有 IO；且 `map_proxy_request_model` 对未知 route **必须报错**（`forwarder.rs:1246-1250` 注释与 `ProxyError::InvalidRequest` 映射；`handlers.rs:161-162` 映射为 `ProxyError::ConfigError`）。因此：

- 这些函数移入 `cc_switch_domain::claude_desktop_routes`，返回 `Result<_, DomainError>`；`claude_desktop_config.rs` re-export。`forwarder.rs:1249-1250` 的 `.map_err(|e| ProxyError::InvalidRequest(e.to_string()))` 与 `handlers.rs:161-162` 的 `.map_err(|e| ProxyError::ConfigError(e.to_string()))` **一个字符都不改**，闭包参数类型从 `AppError` 自然变为 `DomainError`；两者的 `#[error]` 模板相同（§4.2），错误文本不变。不经过任何 `From` 中转（§4.2 已说明原因）。
- `ModelCatalog` 精简为：

```rust
pub trait ModelCatalog: Send + Sync {
    /// 替换 #13：读取 ~/.codex/config.toml 与 cc-switch 目录文件；None 表示按 handlers.rs:106-115 的 stale guard 返回 {"models": []}
    fn codex_models(&self) -> Option<serde_json::Value> { None }
}
```

### 5.6 配套改动

- `ProxyState`、`RequestForwarder`、`ProviderRouter`、`FailoverSwitchManager` 中的 `db: Arc<Database>` 与 `app_handle: Option<AppHandle>` 统一替换为 `host: Arc<ProxyHost>`。
- `ProxyServer::new(config, db, app_handle)` → `ProxyServer::new(config, host)`；生产调用点仅 `services/proxy.rs:104,458` 两处。`ProxyService.app_handle` 是 `Arc<RwLock<Option<AppHandle>>>`（`services/proxy.rs:24`），启动后才注入，所以 `DesktopProxyHost` 在每次 `start / restart` 时按当时的 `app_handle` 组装，`None` 时 `failover` / `events` 用 no-op 实现，与现状 `app_handle == None` 分支一致。
- `handler_context.rs:144-148` 的 `AppError → ProxyError` 映射删除，`select_providers_with_current` 直接返回 `ProxyError`（`AllProvidersCircuitOpen` / `NoProvidersConfigured` / 其它 → `DatabaseError(e.to_string())`）。桌面 `ProviderStore` 实现把 DAO 的 `AppError` 用 `anyhow::Error::from(e)` 原样放入（不加 `context`），`e.to_string()` 输出与现在的 `AppError` Display 相同。
- **新增 `ProxyError::Host(String)`**（`proxy/error.rs`）：

```rust
/// 宿主（ProviderStore / FailoverHandler / CodexLiveAuthHook …）返回的错误，Display 为原文，不加前缀。
/// 用于所有"现状把 AppError 以 {e} / to_string() 原文嵌入其它文案或直接返回前端"的路径。
#[error("{0}")]
Host(String),
```

  `IntoResponse`（`proxy/error.rs:140-166`）中 `Host(_)` 映射为 `(StatusCode::INTERNAL_SERVER_ERROR, self.to_string())`；基线上没有任何一条 HTTP 响应路径会产生 `Host`（它只出现在 hook / failover / guard 的返回值，这些都在到达 `IntoResponse` 前被外层重新包装或写入日志），所以这个映射只是为了穷尽 match。
- **`ProxyError` 与 `AppError` 之间的映射规则**（app 侧 `src/error.rs`，`AppError` 是本地类型，允许写 `From`）：

| 方向 | 规则 |
|---|---|
| `From<ProxyError> for AppError` | `Host(m)` → `AppError::Message(m)`（文案原样）；其余变体 → `AppError::Message(err.to_string())`（带 `ProxyError` 自身前缀，基线上不存在这类路径，仅为 `?` 可编译） |
| `From<DomainError> for AppError` | 见 §4.2 |
| `DomainError → ProxyError` | **不提供 `From`**，各调用点按原分支手写 `InvalidRequest(e.to_string())` / `ConfigError(e.to_string())`（§5.5） |
| `AppError → ProxyError` | **不提供 `From`**（core 不认识 `AppError`）；宿主实现层把 `AppError` 转成 `ProxyError::Host(e.to_string())` 或 `anyhow::Error::from(e)` |

- 使用 `Host` 的位置（P2b / P2c）：`FailoverSwitchManager::try_switch` 对 `switch_to` 的 `Err`（§5.3）；`CodexLiveAuthHook` 六个方法（§5.4）；`CodexLiveAuthSwitchGuard::ensure_unchanged`。

### 5.7 `test-util` feature 与跨 crate 测试辅助

app 的测试直接调用 core 内 `#[cfg(test)]` 的方法，外部 crate 的 `cfg(test)` item 对 app 不可见，P3 后会编译失败：

| core 内 `#[cfg(test)]` item | app 调用点 |
|---|---|
| `CodexOAuthManager::add_test_account_with_access_token`（`codex_oauth_auth.rs:1541`） | `services/provider/mod.rs:2475` |
| `add_test_account_with_user_identity`（`:1557`，内部调用 `codex_config::test_codex_id_token`，`codex_config.rs:393` 同为 `cfg(test)`） | `services/provider/mod.rs:821,2504,2620,2625,2734` |
| `add_test_account_with_workspace_and_access_token`（`:1569`） | `services/provider/mod.rs` |
| `test_cache_access_token`（`:1610`）、`test_refresh_token_for_account`（`:1624`）、`test_set_token_updated_at_ms`（`:1633`） | `services/provider/mod.rs:2592,2675,2707` |

方案：

- `cc-proxy-core` 与 `cc-switch-domain` **在 P0 创建空 crate 时就声明** `[features] test-util = []`（P0 的 CI 命令已带 `--features cc-proxy-core/test-util`，不提前声明会直接失败，见 §8.0 第 2 步）；P2c 才往 feature 下放内容。
- 上表方法与 `test_codex_id_token`（随 id_token 解析移入 domain）改为 `#[cfg(any(test, feature = "test-util"))] pub`。
- core 提供的 `InMemoryProviderStore`、`MemoryUsageSink`、`CountingEvents`（替代 `usage_events::take_test_notify_count`，`usage/logger.rs:523-544`）、`NoopFailover`、`NoopCodexLiveAuth` 同样置于 `test-util`。
- app 的 `Cargo.toml`：`[dependencies] cc-proxy-core = { path = "crates/cc-proxy-core" }`，`[dev-dependencies] cc-proxy-core = { path = "crates/cc-proxy-core", features = ["test-util"] }`（同一路径依赖可在 dev-dependencies 中追加 feature）。
- 生产构建不启用 `test-util`；CI 的 `cargo tree` 断言用默认 feature 跑。

### 5.8 P3 可见性清单（`pub(crate)` → `pub`）

P3 把 `src/proxy` 移出 crate 后，app 侧对 proxy 内 `pub(crate)` 项的引用全部失效。用 `scripts/check-proxy-core-visibility.sh`（附录 B；默认参数 `src-tauri/src/proxy src-tauri/src`，任意目录执行）在基线上扫描（遍历 `src/proxy` 中所有 `pub(crate) fn/struct/enum/const/static/type/trait` 的名字，逐个在 `src/proxy` 之外 `grep -w`），命中 16 项，扣除同名误报后需要改为 `pub` 的如下：

| 项 | 定义位置 | app 侧使用 |
|---|---|---|
| `provider_supports_failover` | `provider_router.rs:18` | `commands/failover.rs:29,94,114,207` |
| `CodexLiveAuthSwitchGuard`（含 `ensure_unchanged`，`:224`） | `providers/codex_oauth_auth.rs:218` | `services/provider/codex_direct.rs:43,120,612,631,648`、`services/provider/live.rs` |
| `CODEX_OAUTH_CLIENT_VERSION`、`CODEX_OAUTH_ORIGINATOR` | `providers/codex_oauth_auth.rs:72-73` | `services/codex_oauth_models.rs` |
| `get_valid_token_bundle_for_account`、`prepare_live_auth_for_account_switch_away`、`ensure_account_exists` | `providers/codex_oauth_auth.rs:1047,1173,1899` | `services/provider/live.rs` |
| 六个 `#[cfg(test)]` 测试辅助方法 | `providers/codex_oauth_auth.rs:1541-1633` | `services/provider/mod.rs`（§5.7，改 `#[cfg(any(test, feature = "test-util"))] pub`） |
| `pub(crate) mod`：`content_encoding`、`failover_switch`、`json_canonical`、`server`、`sse`、`switch_lock`、`tool_media`、`types` | `proxy/mod.rs:8-35` | `services/proxy.rs:9-11`（`server::ProxyServer`、`switch_lock::SwitchLockManager`、`types`）、`commands/proxy.rs:6,352`、`database/dao/{proxy,settings}.rs`、`commands/settings.rs`、`mode/controller.rs:1344`、`services/provider/{mod,codex_direct}.rs`、`claude_desktop_config.rs:1449`（均为 `types::*`） |

误报（不需处理）：`acquire`（命中的是 `database/backup.rs` 等处的同名方法，`SwitchLockManager` 对外只用 `lock_for_app`）；`extract_reasoning_field_text`（`provider.rs:415` 仅出现在文档注释中）。

app 侧对 core 类型的迁移去向：`CodexManagedLiveRefresh`（`codex_config.rs:183`）→ core `host::LiveRefresh`（§5.4），`codex_config.rs` 以 `pub(crate) use cc_proxy_core::host::LiveRefresh as CodexManagedLiveRefresh;` 保持原名。

`mod forwarder;`、`mod handlers;`（`proxy/mod.rs:13,17`）是私有模块，app 侧没有引用，P3 保持私有。

## 6. 文件归属

### 6.1 原样移入 core（仅改 import）

根目录：`body_filter`、`cache_injector`、`circuit_breaker`、`content_encoding`、`copilot_optimizer`、`error`、`error_mapper`、`gemini_url`、`http_client`、`hyper_client`、`json_canonical`、`log_codes`、`session`、`sse`、`switch_lock`、`thinking_budget_rectifier`、`thinking_optimizer`、`thinking_rectifier`、`tool_media`、`types`

`providers/`：`auth`、`codex_chat_common`、`codex_chat_history`、`codex_responses_sse`、`copilot_auth`、`copilot_model_map`、`gemini_schema`、`gemini_shadow`、`reasoning_bridge`、`streaming*`、`transform`、`transform_codex_anthropic`、`transform_codex_chat_moonshot_schema`、`transform_codex_responses_*`、`transform_gemini`、`transform_responses`、`xai_oauth_auth`、`models/*`

`usage/parser.rs`

### 6.2 先按第 4 节解耦，再移入 core（19 个文件）

`forwarder`、`handlers`、`server`、`handler_context`、`handler_config`、`provider_router`、`response_processor`、`failover_switch`、`model_mapper`、`media_sanitizer`、`providers/{mod, adapter, claude, codex, gemini, transform_codex_chat, codex_oauth_auth}`、`usage/{logger, calculator}`

### 6.3 从 core 移回 app

`usage/logger.rs` 中的全部 SQL（写 `proxy_request_logs`、查 `model_pricing`、`load_existing_semantic` 读已有语义）→ `proxy_host/usage.rs`；core 只保留 `RequestLog`、`UsageSemantic` 结构与计算逻辑。

### 6.4 测试

- 纯逻辑测试随文件迁移。
- 依赖 DB 的测试：能用 `InMemoryProviderStore` / `MemoryUsageSink` 的改用内存实现；必须验证 SQL 的迁移到 app 的 `tests/proxy_host_*.rs`。
- **不手填数字**。迁移前后各跑一次下面的脚本，把输出贴到 PR 描述中，要求“测试总数一致（允许位置变化）”：

```bash
# 在 src-tauri/ 执行；迁移前 SCOPE=src/proxy，迁移后 SCOPE="crates/cc-proxy-core/src crates/cc-switch-domain/src src/proxy_host tests"
SCOPE=${SCOPE:-src/proxy}
echo "test fns:  $(grep -rhoE '#\[(tokio::)?test(\(|\])' $SCOPE --include='*.rs' | wc -l)"
echo "db tests:  (files with Database:: inside tests)"
for f in $(grep -rl 'Database::' $SCOPE --include='*.rs'); do
  echo "  $f: $(grep -c 'Database::' "$f") refs, $(grep -cE '#\[(tokio::)?test' "$f") tests"
done
```

基线实测（供对照，不作为验收依据）：`provider_router.rs` 8 个测试 / 8 处 `Database::`，`server.rs` 4 / 4，`response_processor.rs` 8 / 3，`usage/logger.rs` 7 / 7，`forwarder.rs` 79 / 1。

## 7. 独立程序 `cc-proxy`

### 7.1 CLI

```
cc-proxy serve  [--config ~/.cc-proxy/config.toml] [--listen 127.0.0.1:15721]
cc-proxy check  # 校验配置并探测各上游
cc-proxy import --from ~/.cc-switch/cc-switch.db [--app claude,codex,gemini] [--inline-secrets]
cc-proxy env    claude|codex|gemini  # 打印客户端所需环境变量 / 配置片段，不写文件
```

### 7.2 配置

`settings_config` / `meta` 沿用 cc-switch provider JSON 结构，保证 `import` 零转换：

```toml
[server]
listen = "127.0.0.1:15721"
auth_tokens = []            # 监听非回环地址时必须非空，否则拒绝启动
outbound_proxy = ""         # 可选；传给 http_client::init，见 §4 全局状态说明

[storage]
usage_db = "~/.cc-proxy/usage.db"   # 可选；缺省仅输出日志

[apps.claude]
current = "kimi"
failover = ["kimi", "glm"]
auto_failover = true
max_retries = 2

[[apps.claude.providers]]
id = "kimi"
name = "Kimi"
settings_config = { env = { ANTHROPIC_BASE_URL = "https://example.invalid", ANTHROPIC_AUTH_TOKEN = "${KIMI_KEY}" } }
meta = { apiFormat = "anthropic" }

[rectifier]
[optimizer]
[copilot_optimizer]
```

### 7.3 宿主实现

| trait / 字段 | 独立程序实现 |
|---|---|
| `ProviderStore` | `FileProviderStore`：TOML + `${ENV}` 展开；SIGHUP / 文件变更热加载（`arc-swap`）；`record_provider_health` 写内存表；`gateway_token` 返回 `Ok(None)`，Claude Desktop 网关路由按 §5.2 拒绝 |
| `UsageSink` | `SqliteUsageSink`（独立 schema：`proxy_request_logs`、`model_pricing`，内置默认价格表）或 `LogUsageSink` |
| `ProxyEvents` | `LogEvents` |
| `FailoverHandler` | `InMemoryFailover`：更新内存 current 并写回 `state.json` |
| `CodexLiveAuthHook` | 第一阶段 `NoopCodexLiveAuth` |
| `codex_oauth / copilot / xai` | 第一阶段全部 `None`；第二阶段复用三个 manager 的 `new(data_dir, …)` |
| `ModelCatalog` | 默认实现（`codex_models` 返回 `None`） |

启动顺序：解析配置 → `http_client::init(outbound_proxy)` → 组装 `ProxyHost` → `ProxyServer::new(config, host).start()`。

### 7.4 安全默认值

- 默认只监听 `127.0.0.1`；非回环地址必须配置 `auth_tokens`。
- 入站 token 使用常量时间比较（`subtle`）。桌面端 Claude Desktop gateway token 现为 `token != expected`（`handlers.rs:290`），P4 一并改为 `subtle::ConstantTimeEq`（行为不变，只是时序）。
- 日志中的 URL 与密钥统一走 `util::redact`。
- 配置文件权限检查：若 group/other 可读则告警。

### 7.5 `import` 的读取方式与密钥策略

- 桌面端以 `Connection::open` 默认模式打开数据库（`database/mod.rs:106`），未设置 WAL / `busy_timeout`，单连接 + `update_hook`。`import` 必须用 `OpenFlags::SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` 打开并设置 `busy_timeout(5s)`，遇到 `SQLITE_BUSY` 重试后仍失败则提示“请先退出 CC Switch”。绝不以读写模式打开，绝不执行 schema 迁移。
- 数据库路径需先读 `app_store::get_app_config_dir_override()` 对应的覆盖（`config.rs:260`），再回退 `~/.cc-switch/`；`current` 取自 `~/.cc-switch/live-state.json`（`live/engine.rs:36,44`）的代理路由，没有则取直连指针。
- `settings_config` 内的 API key 为明文。`import` 默认把识别出的密钥字段（`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GEMINI_API_KEY` / `GOOGLE_API_KEY` / Grok `api_key` / Codex `auth.OPENAI_API_KEY`）替换为 `${CC_PROXY_<APP>_<PROVIDER_ID>_KEY}` 占位，并把真实值另写到 `~/.cc-proxy/secrets.env`（`0600`）；`--inline-secrets` 才写入 TOML，并打印告警。

### 7.6 与桌面端的关系

- 近期：互不影响，桌面端继续内嵌 core。
- 远期（可选）：桌面端增加“连接外部 cc-proxy”模式。

## 8. 分阶段实施

| PR | 内容 | 上游冲突风险 | 验收 |
|---|---|---|---|
| P0（**已实施，待提交**） | workspace 骨架；空的 `cc-switch-domain`、`cc-proxy-core` crate（各自声明 `test-util`，core 转发给 domain）；`scripts/check-proxy-core-{coupling,deps,visibility}.sh`；CI 按 §8.1 修改。具体步骤与实测结果见 §8.0 | 低 | 构建与全部测试通过（3108 通过 / 0 失败 / 9 忽略）；`pnpm tauri build --debug --bundles app`（关闭 updater 产物）产出 `CC Switch.app`；耦合脚本基线 95 条 / 19 文件，可见性脚本 16 项 |
| P1 | 抽出 domain：`AppType`（含 §4.2 `DomainError`）、`Provider`/`ProviderMeta`/`CustomEndpoint`、codex/grok/pi 纯解析、`model_capabilities`、常量、`sql_helpers` 纯函数、Claude Desktop 路由纯函数；原路径 re-export；app 侧 `From<DomainError> for AppError` | 中 | 零行为变化，测试全绿；§4.2 错误文本快照测试 |
| P2a | 引入 `host.rs` 与 `ProviderStore`、`UsageSink`；替换 #1-#7；新增 `proxy_host/{store,usage}.rs` | 中高 | §8.2 脚本输出中不再出现 `database` / `mode` / `services` / `usage_events`；用量统计冒烟 |
| P2a（续） | 同一 PR 内为 core 加 `async-trait` 依赖（§4.4） | — | — |
| P2b | `ProxyEvents`、`FailoverHandler`；新增 `ProxyError::Host` 与 `From<ProxyError> for AppError`（§5.6）；替换 #8、#17、#20；改 `commands/proxy.rs:351-357` 与 `forwarder.rs:568,670,818,980` | 中 | 故障转移冒烟：托盘 + 前端事件；§9.5 日志比对 |
| P2c | 注入三个 manager、`CodexLiveAuthHook`（含 `LiveRefresh` 迁移）、`ModelCatalog`、`gateway_token` 的 `None` 分支（§5.2）；替换 #9-#14、#19；`test-util` feature 下放内容 | **高** | Copilot / Codex OAuth / xAI / Claude Desktop 冒烟；`~/.codex/auth.json` 同步回归；§8.2 脚本无输出 |
| P3 | `git mv src/proxy crates/cc-proxy-core/src`；按 §5.8 清单 `pub(crate)` → `pub`；app 侧 re-export。只含移动与可见性调整。开工前重跑附录 B 脚本核对清单 | 中 | `scripts/check-proxy-core-deps.sh` 通过（§3）；`cargo test --workspace --features cc-proxy-core/test-util` 全绿；§6.4 脚本计数一致；附录 B 脚本在 app 侧无未处理命中 |
| P4 | `cc-proxy` 二进制：配置、`FileProviderStore`、`SqliteUsageSink`、入站鉴权、`serve/check/env` | 无 | 端到端：Claude Code 与 Codex CLI 经 cc-proxy 完成流式对话、工具调用、故障转移 |
| P5 | `import`（§7.5）、热加载、打包（cargo install / release 产物）、文档 | 无 | 桌面端运行中执行 import 不报错、不改动 cc-switch.db；导入后可直接启动 |
| P6（第二阶段） | OAuth 上游（manager 注入 + `auth login`） | 低 | 先评估服务条款风险 |

### 8.0 P0 实施步骤（已实施，待提交）

以下每一步均已在本地执行并核对过产物；括号内为实测结果。

1. `src-tauri/Cargo.toml` 顶部追加 `[workspace] members = ["crates/*"]`、`resolver = "2"`（根包 `edition = "2021"` 本已默认 resolver 2，显式写出避免日后改成虚拟 workspace 时行为变化）；根 `[package]` 段与 `[profile.release]`（`:111`）不动。
2. 新建 `crates/cc-switch-domain` 与 `crates/cc-proxy-core`（`lib.rs` 为空），`edition` / `rust-version` / `license` / `authors` 与根包一致，`publish = false`。feature 声明：domain `test-util = []`；core `test-util = ["cc-switch-domain/test-util"]`（转发给 domain），core 以 `path = "../cc-switch-domain"` 依赖 domain。**app 在 P0 不依赖这两个 crate**，P1 起再加 `[dependencies]` / `[dev-dependencies]`（§5.7）。
3. `Cargo.lock` 只新增 `cc-proxy-core`、`cc-switch-domain` 两个条目（11 行），随 PR 提交；CI 缓存 key（`ci.yml:141-142`、`wsl2-nightly.yml:37-38`）不改。
4. 按 §8.1 改 `ci.yml:150,153,155-163` 与 `wsl2-nightly.yml:63,89`；`ci.yml:218-231`、`wsl2-nightly.yml:71,79` 保持。依赖断言用 `scripts/check-proxy-core-deps.sh`（§3 已更正 `cargo tree -p … -i` 的真实行为：不依赖时退出码 101 + `did not match any packages`，而不是 v3 所写的"`nothing to print`、退出码 0"）。
5. 三个脚本都放在**仓库根目录** `scripts/`（与已有 `.mjs` 脚本同级），路径参数一律相对仓库根目录，任意目录可执行：`check-proxy-core-coupling.sh`（`SCOPE` 默认 `src-tauri/src/proxy`）、`check-proxy-core-visibility.sh`（默认 `src-tauri/src/proxy src-tauri/src`）、`check-proxy-core-deps.sh`（无参数）。基线实跑：coupling 95 条 / 19 文件，visibility 16 项（含 2 项误报，见 §5.8），deps 通过。
6. 本地验证（均通过）：`cargo test --workspace --features cc-proxy-core/test-util` 与 `cargo test --features test-hooks` 均为 **3108 通过 / 0 失败 / 9 忽略**；`cargo fmt --all --check`；`cargo clippy --workspace -- -D warnings`（**不加 `--all-targets`**，原因见 §8.1）；打包用 `pnpm tauri build --debug --bundles app --config '{"bundle":{"createUpdaterArtifacts":false}}'`，产出 `CC Switch.app`——`tauri.conf.json` 的 `createUpdaterArtifacts: true` 需要签名私钥，本地没有，当前 CLI 也没有 `--no-sign`，updater 产物由 release CI 验证。`pnpm dev` 的文件监视（`src-tauri/crates/` 变更是否触发重编译）**未验证**（交互式，未执行）。
7. `cargo test --features test-hooks` 本地通过（源码只有 4 处 `feature = "test-hooks"` 引用），已作为独立步骤 `Run tests (test-hooks)` 加入 CI（`ci.yml:162-163`）。
8. 未加 `async-trait`（P2a 引入，§4.4）；未动 `src/proxy` 任何文件。

### 8.1 workspace 化对 CI / 构建的具体改动（P0）

现状：`src-tauri` 是单 package（`Cargo.toml` 无 `[workspace]`），`Cargo.lock` 在 `src-tauri/`，`.gitignore` 忽略 `src-tauri/target/`。加 `[workspace] members = ["crates/*"]` 后根 package 自动成为成员，target 目录与 lock 文件位置不变，`[profile.release]`（`Cargo.toml:111`）位于 workspace 根，继续生效；`pnpm tauri build`（`release.yml:237,260,262,267`）读取的仍是 `src-tauri/Cargo.toml`，不需要 `tauri.conf.json` 改动。

需要改的地方（根 package 是 workspace 成员时，不带 `--workspace` 的 cargo 命令只作用于根包）：

| 文件:行 | 现状 | 改为 |
|---|---|---|
| `.github/workflows/ci.yml:150` | `cargo fmt --check --manifest-path src-tauri/Cargo.toml` | `cargo fmt --all --check --manifest-path src-tauri/Cargo.toml` |
| `ci.yml:153` | `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` | `cargo clippy --manifest-path src-tauri/Cargo.toml --workspace -- -D warnings`。**不加 `--all-targets`**：实测会因上游测试代码 `src/proxy/providers/transform_codex_chat.rs:4498` 的 `clippy::op_ref`（`url == &data_url`）失败；原 CI 从未对测试代码跑过 clippy，P0 保持原检查范围，`--all-targets` 作为独立 PR 的后续事项（§11） |
| `ci.yml:155-157`（新增） | — | 步骤 `Check proxy core crates stay free of tauri/rusqlite`，`shell: bash`，执行 `scripts/check-proxy-core-deps.sh`（§3） |
| `ci.yml:159-160` | `cargo test --manifest-path src-tauri/Cargo.toml` | `cargo test --manifest-path src-tauri/Cargo.toml --workspace --features cc-proxy-core/test-util` |
| `ci.yml:162-163`（新增） | — | 步骤 `Run tests (test-hooks)`：`cargo test --manifest-path src-tauri/Cargo.toml --features test-hooks`（本地实测通过后加入） |
| `ci.yml:218-231` | `cargo test --lib --no-run --message-format=json` 取 `cc_switch_lib` 测试二进制 | 保持（`--lib` 只针对根包，且按 `target.name == "cc_switch_lib"` 过滤，不受影响） |
| `ci.yml:141-142`、`wsl2-nightly.yml:37-38` | 缓存 key 用 `src-tauri/Cargo.lock` | 保持 |
| `wsl2-nightly.yml:63`（`--no-run` 编译）、`:89`（全量测试） | `cargo test --tests --manifest-path …` | 追加 `--workspace` |
| `wsl2-nightly.yml:71,79` | `cargo test --tests … -- --ignored --list` / 按名字跑单个测试 `config::tests::atomic_write_replaces_existing_wsl_unc_file` | 保持（针对 app 内按名指定的测试，不需要 `--workspace`） |
| `release.yml` | `pnpm tauri build …` | 保持；本地只能用 `--bundles app` 并关闭 `createUpdaterArtifacts` 验证（§8.0 第 6 步），updater 产物由 release CI 验证；P3 后再跑一次 |

新 crate 的 `edition = "2021"`、`rust-version = "1.85.0"` 与根包一致。

### 8.2 P2 完成判据（可靠脚本）

原 `awk '/#\[cfg\(test\)\]/{exit}'` 会在首个 `#[cfg(test)]` 处截断，但以下文件的首个 `#[cfg(test)]` 只修饰单个 item，其后仍是生产代码：`media_sanitizer.rs:1`、`providers/streaming_codex_anthropic.rs:14`、`body_filter.rs:41`、`providers/transform_responses.rs:1309`、`forwarder.rs:3557`（漏掉 `:3773` 的 `crate::redact_url_for_log`）、`providers/codex_oauth_auth.rs:1540`（漏掉 `:1563-1803` 的 `crate::codex_config::*`）。原正则也不含 `crate::provider`、`crate::model_capabilities`、`crate::redact_*`、`rusqlite`，且匹配不到跨行的 `use crate::{ app_config::AppType, provider::… }`（`forwarder.rs:28-31`）。

改用附录 A 的脚本（已落地为仓库根目录 `scripts/check-proxy-core-coupling.sh`）。判据：`scripts/check-proxy-core-coupling.sh` 无输出、退出码 0（`SCOPE` 默认 `src-tauri/src/proxy`；P3 后用 `SCOPE=src-tauri/crates/cc-proxy-core/src`）。脚本只剥离 `#[cfg(test)]` 后紧跟 `mod xxx {` 的整个块（按花括号配对），对修饰单个 `use` / `fn` 的 `#[cfg(test)]` 只剥离该 item 本身；正则覆盖 `crate::(?!proxy\b)`、`\btauri\b`、`\brusqlite\b`，并把跨行 `use crate::{ … }` 折叠成单行后再匹配。

该脚本只是辅助；P3 后 `scripts/check-proxy-core-deps.sh` 才是硬判据。

## 9. 验证策略

1. **行为基线**：P2 之前录制请求 / 响应夹具（Claude、Codex Chat、Codex Responses、Gemini 的流式与非流式各至少一组，含 tool call、thinking、错误重试），P2、P3 后逐字节比对。
2. **每个 PR**：`cargo fmt --all --check`、`cargo clippy --workspace -- -D warnings`（不加 `--all-targets`，见 §8.1）、`cargo test --workspace --features cc-proxy-core/test-util`、`cargo test --features test-hooks`、`scripts/check-proxy-core-deps.sh`、`pnpm typecheck`、`pnpm test:unit`。
3. **P4 端到端**：`crates/cc-proxy/tests/` 用 axum mock 上游（参考 `server.rs` 现有 mock 写法）。
4. **打包**：P0、P3 后各执行一次 `pnpm tauri build --debug --bundles app --config '{"bundle":{"createUpdaterArtifacts":false}}'`，确认 workspace 化不影响 Tauri 构建；updater 产物需要签名私钥，只能由 release CI 验证。
5. **日志 Display 一致性口径**：§2.2 “日志内容不变” 指 **日志编号（`log_codes.rs`）+ 固定文案 + 参数值** 不变；错误对象经 `{e}` / `to_string()` 输出的文本必须逐字一致。基线 `ProxyError` 的全部变体都带前缀（`proxy/error.rs:9-79`，如 `AuthError` = `"认证失败: {0}"`），因此凡是现状把 `AppError` 原文嵌入其它文案的路径，都必须走 §5.6 新增的 `ProxyError::Host(String)`（`#[error("{0}")]`）。涉及的换型点：
   - `AppError::Localized` ↔ `DomainError::Localized`：`#[error("{zh} ({en})")]` 模板相同（§4.2）。
   - `AppError::Message` → `ProxyError::Host`：`try_switch` 返回值由 `AppError` 改为 `ProxyError` 后，`commands/proxy.rs:357` 的 `"[Recovery] 自动切换失败: {e}"` 与 `failover_switch.rs:100` 的 `map_err(AppError::Message)` 路径，经 `Host(msg)` 的 Display 输出原始字符串本身。
   - `CodexLiveAuthHook` 六个方法与 `CodexLiveAuthSwitchGuard::ensure_unchanged`：`forwarder.rs:1223-1229`（`"Codex OAuth 会话校验失败: {error}"`）、`codex_oauth_auth.rs:804-808`（`TokenFetchFailed(error.to_string())`）、`:1002-1010`（warn 日志）、`codex_direct.rs:612`（`?` 直达前端）——全部经 `Host`，文本不变。
   - `DomainError → ProxyError`：`forwarder.rs:1249-1250`、`handlers.rs:161-162` 的闭包写法不变，不经 `From`（§5.5）。
   - `handler_context.rs:148` `DatabaseError(e.to_string())`：`ProxyError::DatabaseError` 的模板保持 `"数据库错误: {0}"` 不变；`e` 由 `AppError` 变为 `anyhow::Error`，桌面 `ProviderStore` 用 `anyhow::Error::from(e)` 不加 `context`，`to_string()` 相同（§5.6）。
   - 验收方法：P2b / P2c 前后各跑一次故障转移与 OAuth 失败冒烟，`diff` 过滤掉时间戳与 request_id 后的日志文件；另为 `Host`、`From<ProxyError> for AppError` 写单测，断言 `to_string()` 不含前缀。

## 10. 第二步预留：改写管线

第一步不实现，只保证接口不阻碍：

- core 内部把现有改写点（`model_mapper`、`thinking_*`、`cache_injector`、`body_filter`、`media_sanitizer`、`copilot_optimizer`）保持为独立函数，不在 P2 中进一步内联。
- 第二步引入 `Pipeline`（请求前处理 → 选择上游 → 调用 → 响应后处理），节点可作用于流式响应；同协议无改写时走透传。

## 11. 风险与应对

| 风险 | 应对 |
|---|---|
| `forwarder.rs`（5,525 行）改动最频繁 | P2 只替换取用点（`app_handle` 13 处，清单见耦合点 #9；非测试 `crate::` 引用 7 处：`:25,28（跨行 use）,1223,1249,2305,2307,3773`），不改逻辑；P2c 单独 PR |
| 上游继续重构 `mode` / live 写入 | `mode` 只通过 `ProviderStore` / `FailoverHandler` 接入，影响限定在 `proxy_host/` |
| `Provider` 进入 domain 后需同步上游字段 | 原样迁移 + re-export，上游改动可直接落到 domain 文件 |
| Codex OAuth 与 live auth 双向同步 | `CodexLiveAuthHook` 保持调用时序不变；P2c 专项回归 |
| `DomainError` 文案与 `AppError` 漂移 | §4.2 快照测试；两个枚举的 `#[error]` 模板互相引用同一常量注释 |
| workspace 化影响 Tauri 构建 / 签名 / updater | P0 即做打包验证；§8.1 逐项改 CI |
| 独立程序暴露到非回环地址 | 强制 token；默认只监听回环地址 |
| `import` 与桌面进程争用 SQLite | 只读打开 + `busy_timeout`，失败提示退出桌面端（§7.5） |
| clippy 未覆盖测试代码（`--all-targets` 会因上游 `transform_codex_chat.rs:4498` 的 `clippy::op_ref` 失败） | P0 保持原检查范围；开启 `--all-targets` 并修上游告警作为独立 PR 的后续事项，与本方案各阶段无依赖 |

## 附录 A：耦合检查脚本（`scripts/check-proxy-core-coupling.sh`）

脚本正文以仓库根目录 `scripts/check-proxy-core-coupling.sh` 为准，本文不再保留副本。要点：

- 用法：任意目录执行 `scripts/check-proxy-core-coupling.sh`；`SCOPE` 默认 `src-tauri/src/proxy`，P3 后用 `SCOPE=src-tauri/crates/cc-proxy-core/src`；相对路径按仓库根目录解析，目录不存在以 2 退出。
- 逻辑：剥离 `#[cfg(test)]` 后紧跟的 `mod xxx {` 整块（按花括号配对）；对修饰单个 item 的 `#[cfg(test)]`，以 `;` 结尾的单行 item 直接跳过，带函数体的按花括号配对跳过；把跨行 `use … {` … `};` 折叠成一行；去掉 `//` 起的行尾注释后匹配 `\bcrate::(?!proxy\b)|\btauri\b|\brusqlite\b`。
- 输出：逐行 `文件:行:内容`；无命中退出 0，有命中退出 1。
- 已知局限（可接受）：块注释 `/* … */` 内的匹配不会被忽略；`#[cfg(test)]` 与 item 之间若夹有其它属性行（如 `#[allow(…)]`）需要把该属性放到 `#[cfg(test)]` 之前。基线 `src/proxy` 中不存在这两种写法。

基线实测（846de29c1）：退出码 1，95 条命中，分布在 §6.2 的 19 个文件（`codex_oauth_auth.rs` 16、`providers/codex.rs` 14、`usage/logger.rs` 11、`forwarder.rs` 10、`failover_switch.rs` 10、`handlers.rs` 8、`provider_router.rs` 5、`handler_context.rs` 5、`server.rs` 3，其余各 1-2）。

## 附录 B：`pub(crate)` 可见性扫描脚本（`scripts/check-proxy-core-visibility.sh`）

脚本正文以仓库根目录 `scripts/check-proxy-core-visibility.sh` 为准，本文不再保留副本。要点：

- 用法：任意目录执行 `scripts/check-proxy-core-visibility.sh [PROXY_DIR] [APP_SRC]`，默认 `src-tauri/src/proxy src-tauri/src`；P3 后用 `src-tauri/crates/cc-proxy-core/src src-tauri/src`。
- 逻辑：收集 `PROXY_DIR` 内所有 `pub(crate) (async)? fn|struct|enum|const|static|type|trait <name>` 的名字，逐个在 `APP_SRC`（排除 `PROXY_DIR`）里 `grep -rlw`。
- 输出：`<item>: <app 侧文件列表>`，stderr 打印 `total: N item(s)`；有命中退出 1。按名匹配会有同名误报，需人工排除（基线误报：`acquire`、`extract_reasoning_field_text`）。不覆盖 `pub(crate) mod` 声明（`proxy/mod.rs:8-35`），那 8 个模块按 §5.8 表格处理。

基线实测：160 个 `pub(crate)` 项，16 项在 app 侧命中，逐项归属见 §5.8。P3 迁移后重跑，要求只剩误报项。

## 修订记录

v3.1（按 P0 实施结果同步，基线不变；以下均为实测）：

1. **`cargo tree` 行为更正**（§3、§8.0 第 4 步）：`-p <member> -i <dep>` 不依赖时退出码 101 + `did not match any packages`，不是 v3 所写的"`nothing to print`、退出码 0"；v3 的内联片段只是碰巧正确且拼错依赖名会静默通过。§3 改为引用 `scripts/check-proxy-core-deps.sh`，并说明其前置检查与三种判定。
2. **clippy 不加 `--all-targets`**（§8.0 第 6 步、§8.1、§9 第 2 条）：上游测试代码 `transform_codex_chat.rs:4498` 触发 `clippy::op_ref`；P0 保持原检查范围只加 `--workspace`，`--all-targets` 列入 §11 作为独立 PR 的后续事项。
3. **wsl2-nightly 只改 `:63`、`:89`**：`:71`、`:79` 针对 app 内按名指定的单个测试，不加 `--workspace`（§8.0、§8.1）。
4. **脚本位置与用法**：三个脚本放仓库根目录 `scripts/`，路径参数相对仓库根目录、任意目录可执行；§5.8、§8.0、§8.2、附录 A/B 的用法同步更新，附录不再保留脚本副本，以 `scripts/` 下文件为准。
5. **CI 实际改动**（§8.1）：fmt `--all`；clippy `--workspace`；新增 `Check proxy core crates stay free of tauri/rusqlite`（`scripts/check-proxy-core-deps.sh`，`shell: bash`）；test `--workspace --features cc-proxy-core/test-util`；新增 `Run tests (test-hooks)`。
6. **Cargo 细节**（§8.0 第 2、3 步）：core `test-util = ["cc-switch-domain/test-util"]`，core 以 path 依赖 domain，app 在 P0 不依赖两者；`Cargo.lock` 只多两个条目。
7. **本地验证与打包**（§8.0 第 6 步、§8 P0 行、§9 第 4 条）：两组测试均 3108 通过 / 0 失败 / 9 忽略；打包命令改为 `pnpm tauri build --debug --bundles app --config '{"bundle":{"createUpdaterArtifacts":false}}'`（updater 产物需签名私钥，由 release CI 验证）；`pnpm dev` 文件监视标为未验证。
8. §8 表格 P0 行与 §8.0 标注"已实施，待提交"。

v3（按 Fable 二次审查修订，基线不变）：

1. **`ProxyError::Host`**：§5.6 新增 `Host(String)`（`#[error("{0}")]`）与 `ProxyError` / `AppError` / `DomainError` 三者的映射规则表；§5.3、§5.4、§9.5 中所有"把 `AppError` 原文嵌入其它文案"的路径（`try_switch`、`CodexLiveAuthHook` 六个方法、`ensure_unchanged`）改走 `Host`，不再用 `AuthError`（其 Display 带 `认证失败: ` 前缀，会改变 `forwarder.rs:1223`、`codex_oauth_auth.rs:804,1002`、`codex_direct.rs:612`、`commands/proxy.rs:357` 的输出）。
2. **删除 `From<DomainError> for ProxyError`**：§4.2 说明原因，§5.5 改为 `forwarder.rs:1249-1250` / `handlers.rs:161-162` 闭包写法原样不动，避免 `"无效的请求: 无效的请求: …"` 双前缀。
3. **P3 可见性清单**：新增 §5.8，补 `provider_supports_failover`（`provider_router.rs:18`，`commands/failover.rs` 4 处）、`CodexLiveAuthSwitchGuard`、`CODEX_OAUTH_*` 常量、8 个 `pub(crate) mod` 与误报说明；`CodexManagedLiveRefresh`（`codex_config.rs:183`）迁移为 core `host::LiveRefresh`（§4 #12、§5.4）；耦合点 #5 补 `UsageLogger` 全部构造点（`handlers.rs:2778,2824`、`response_processor.rs:641`）。扫描脚本放入附录 B，并在 P3 验收中使用。
4. **core 依赖表**：新增 §4.4（含 `async-trait`，P2a 引入；根包当前没有该依赖）。
5. **forwarder `app_handle` 13 处**：耦合点 #9 与 §11 列出全部行号，含四处 `tokio::spawn` 内的 `try_switch` 调用。
6. **`gateway_token` 的 `None` 语义**：§5.2 增加三分支表，headless 端返回 `AuthError("Claude Desktop gateway 未启用")`（401），桌面端永不返回 `None`；§7.3 同步。
7. **`FailoverHandler::switch_to` 参数精简**：去掉冗余的 `app_type: &str`，只传 `&AppType`；错误用 `anyhow::anyhow!(msg)` 不加 context。
8. **统计口径**：§1.2 行数改为 76,774 并写明命令，"约 60%" 改为 48/67（约 72%）并写明方法；§1.3 上游提交数改为 67 / 17 / 17 并注明 `git log` 参数与"含 fork 自身提交"。
9. **`test-util` 在 P0 声明**：§5.7 与 §8 P0 行、§8.0 第 2 步一致，避免 P0 的 CI 命令因 feature 不存在而失败。
10. **P0 实施步骤**：新增 §8.0（8 步），包含 `resolver = "2"`、`Cargo.lock`、`cargo tree` 判空的 stderr / stdout 实测结论、两个脚本落地与基线数字、`test-hooks` 单独验证。
11. 附录 A 补基线实测数字（95 条 / 19 文件）；§4.2 指出现有 `tests/app_type_parse.rs:27` 可复用为快照测试。

v2（按 Fable 审查修订，基线不变）：

1. **P2 判据**：§8.2 说明原 `awk` 截断与正则遗漏的具体文件行号，改为附录 A 的剥离脚本（括号匹配 `mod tests`、处理修饰单个 item 的 `#[cfg(test)]`、折叠跨行 `use`、正则覆盖 `crate::(?!proxy)` / `tauri` / `rusqlite`）。
2. **AppType 孤儿规则 / 错误类型**：新增 §4.2 `DomainError`（`Localized` / `Message`）、domain 内 `FromStr for AppType`、app 侧 `From<DomainError> for AppError`，要求 Display 模板逐字一致并加快照测试；删除不存在的 `Display for AppType` 描述；同一方案覆盖 `strip_codex_unified_session_bucket`、`pi_config::provider_base_url`、Claude Desktop 路由函数。
3. **ProviderStore 健康写回**：耦合点 #3 补 `provider_router.rs:96,178`；§5.2 增加 `record_provider_health`（async）、`all_providers` / `failover_queue`（同步），sync / async 与 DAO 一致；删掉 v1 的 `failover_candidates`。
4. **test-util feature**：新增 §5.7，列出 `codex_oauth_auth.rs:1540-1640` 六个 `cfg(test)` 方法与 `codex_config::test_codex_id_token` 在 `services/provider/mod.rs` 中的调用行，给出 `#[cfg(any(test, feature = "test-util"))]` 与 dev-dependencies 写法；P3 验收加 `cargo test --workspace`。
5. **ModelCatalog 精简**：§5.5 把 `map_proxy_request_model` / `model_list_response` 等纯函数移入 domain 并保留错误语义，`ModelCatalog` 只剩 `codex_models`；耦合点 #11、#14 同步更新。
6. **UsageSink 签名**：`pricing_model_source` 改 async，与 `dao/proxy.rs:95` 一致；`record` 返回 `RecordOutcome` 以保持 `usage_logged` 触发时序。
7. **OAuth 直接注入 manager**：§5.1 `ProxyHost` 持有三个 `Option<Arc<…>>`，删除 `CredentialProvider`；§5.4 `CodexLiveAuthHook` 只保留 6 个 codex_config IO 方法，并列出 app 侧受影响的调用点。
8. **`ensure_unchanged` 返回类型**：§5.4 改为 `ProxyError`，app 侧增加 `From<ProxyError> for AppError`。
9. **P2b 遗漏调用点**：耦合点 #8 与 §5.3 补 `commands/proxy.rs:351-357`；§5.6 说明 `ProxyService.app_handle` 延迟注入时 host 的组装时机。
10. **CI / workspace**：新增 §8.1 逐文件逐行列出 `ci.yml`、`wsl2-nightly.yml`、`release.yml` 的改动与保持项；§3 `cargo tree` 改为判空脚本并说明退出码问题；明确 `test-hooks` 只属于 app。
11. **domain 依赖集合**：新增 §4.3，列出 serde / serde_json / http / indexmap / toml / toml_edit / base64 / regex / thiserror 及禁止项；`resolve_usage_credentials` 改为随纯解析留在 domain。
12. **行号 / 条目修正**：§1.2 文件数 17 → 19，行数与测试数按实测更新；#16 补 `response_processor.rs:16`、`providers/codex.rs:293`；#17 补 `forwarder.rs:3773`；#6 把 `is_placeholder_pricing_model` 归入 domain。
13. **§6.4 测试统计**：改为脚本输出，不再手填；基线数字仅作对照（`response_processor.rs` 实为 8 个测试）。
14. **http_client 全局状态**：§4 末尾与 §7.3 说明 `init` / `set_proxy_port` 的初始化要求，配置增加 `outbound_proxy`。
15. **import**：新增 §7.5，规定只读 + `busy_timeout` 打开、路径覆盖、`live-state.json` 读取、密钥占位与 `secrets.env`；CLI 增加 `--inline-secrets`。
16. **日志一致性验收**：新增 §9.5，定义 Display 逐字一致的口径与 diff 方法；§7.4 顺带把 gateway token 比较改为常量时间。
