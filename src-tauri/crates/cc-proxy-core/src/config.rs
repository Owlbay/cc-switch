//! 中转配置类型与校验（设计文档 §4、§5、§6、§10）。
//!
//! 纯数据：不读文件、不展开环境变量（由 `cc-proxy` 负责），只做结构与取值校验。

use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::convert::ModelMap;
use crate::interface::Interface;
use crate::upstream_url::{is_valid_strip_prefix, looks_like_endpoint_path};

/// 不在 `Debug` / 日志中暴露的字符串（Key、token）。
#[derive(Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// 写入上游请求的鉴权方式（§5.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthScheme {
    /// `authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>`
    XApiKey,
    /// `x-goog-api-key: <key>`
    XGoogApiKey,
    /// 不写入任何鉴权头
    None,
}

impl AuthScheme {
    /// 各接口的默认鉴权方式
    pub fn default_for(interface: Interface) -> Self {
        match interface {
            Interface::Claude => AuthScheme::XApiKey,
            Interface::OpenaiChat | Interface::OpenaiResponses => AuthScheme::Bearer,
            Interface::Gemini => AuthScheme::XGoogApiKey,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelayConfig {
    pub server: ServerConfig,
    pub tls: TlsConfig,
    pub timeouts: TimeoutConfig,
    pub failover: FailoverConfig,
    pub breaker: BreakerConfig,
    pub upstreams: UpstreamsConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub auth_tokens: Vec<Secret>,
    /// 只允许与回环地址组合；为 true 且未配置 token 时不做入站鉴权
    pub allow_anonymous: bool,
    pub max_body_bytes: u64,
    pub preserve_header_case: bool,
    /// 全局出站代理（http / https / socks5 / socks5h）
    pub proxy_url: Option<String>,
}

pub const DEFAULT_LISTEN: &str = "127.0.0.1:15721";
pub const DEFAULT_MAX_BODY_BYTES: u64 = 200 * 1024 * 1024;

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN
                .parse()
                .expect("valid default listen address"),
            auth_tokens: Vec::new(),
            allow_anonymous: false,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            preserve_header_case: true,
            proxy_url: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    pub native_roots: bool,
    pub extra_ca_file: Option<PathBuf>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            native_roots: true,
            extra_ca_file: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutConfig {
    pub connect_secs: u64,
    pub response_head_secs: u64,
    pub idle_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_secs: 10,
            response_head_secs: 120,
            idle_secs: 300,
        }
    }
}

pub const DEFAULT_RETRY_ON_STATUS: [u16; 6] = [429, 500, 502, 503, 504, 529];

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FailoverConfig {
    pub retry_on_status: Vec<u16>,
    pub retry_body_bytes: u64,
    /// 缺省为该接口的上游数量
    pub max_attempts: Option<usize>,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            retry_on_status: DEFAULT_RETRY_ON_STATUS.to_vec(),
            retry_body_bytes: 1024 * 1024,
            max_attempts: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BreakerConfig {
    pub failure_threshold: u32,
    pub open_secs: u64,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_secs: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamsConfig {
    pub claude: Vec<UpstreamConfig>,
    pub openai_chat: Vec<UpstreamConfig>,
    pub openai_responses: Vec<UpstreamConfig>,
    pub gemini: Vec<UpstreamConfig>,
}

impl UpstreamsConfig {
    pub fn for_interface(&self, interface: Interface) -> &[UpstreamConfig] {
        match interface {
            Interface::Claude => &self.claude,
            Interface::OpenaiChat => &self.openai_chat,
            Interface::OpenaiResponses => &self.openai_responses,
            Interface::Gemini => &self.gemini,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub id: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Secret,
    /// 缺省按上游协议取 [`AuthScheme::default_for`]
    #[serde(default)]
    pub auth: Option<AuthScheme>,
    #[serde(default)]
    pub strip_prefix: Option<String>,
    /// 覆盖全局 `proxy_url`；空字符串表示该上游直连
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// 上游说的协议；缺省等于所在接口（透传）。与接口不同时需要协议转换器
    #[serde(default)]
    pub protocol: Option<Interface>,
    /// 模型名映射（客户端模型 → 上游模型，`*` 兜底）。非空时即使同协议也走恒等转换，
    /// 不再逐字节透传
    #[serde(default)]
    pub model_map: ModelMap,
    /// 转换路径上客户端没有指定输出上限时写入上游请求的值（缺省
    /// [`DEFAULT_MAX_OUTPUT_TOKENS`]）；透传路径不使用
    #[serde(default)]
    pub default_max_output_tokens: Option<u64>,
}

/// 转换路径的缺省输出上限
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 16384;

impl UpstreamConfig {
    /// 上游实际说的协议
    pub fn protocol_for(&self, interface: Interface) -> Interface {
        self.protocol.unwrap_or(interface)
    }

    /// 是否需要经过转换器（协议转换或同协议 + model_map 的恒等转换）
    pub fn converts(&self, interface: Interface) -> bool {
        self.protocol_for(interface) != interface || !self.model_map.is_empty()
    }

    pub fn default_max_output_tokens(&self) -> u64 {
        self.default_max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
    }

    /// 缺省按**上游协议**取鉴权方式（如 claude 接口下的 openai_chat 上游默认 Bearer）
    pub fn auth_scheme(&self, interface: Interface) -> AuthScheme {
        self.auth
            .unwrap_or_else(|| AuthScheme::default_for(self.protocol_for(interface)))
    }

    /// 该上游实际使用的出站代理：上游配置优先，空字符串表示直连。
    pub fn effective_proxy<'a>(&'a self, global: Option<&'a str>) -> Option<&'a str> {
        match self.proxy_url.as_deref() {
            Some("") => None,
            Some(url) => Some(url),
            None => global.filter(|url| !url.is_empty()),
        }
    }
}

/// 配置校验失败的原因，逐条列出，便于 `cc-proxy check` 一次性展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrors(pub Vec<String>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, message) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("\n")?;
            }
            write!(f, "- {message}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

const PROXY_SCHEMES: [&str; 4] = ["http", "https", "socks5", "socks5h"];

impl RelayConfig {
    /// 校验配置；返回全部问题而不是遇到第一个就停。
    pub fn validate(&self) -> Result<(), ConfigErrors> {
        let mut errors = Vec::new();
        self.validate_server(&mut errors);
        self.validate_limits(&mut errors);
        for interface in Interface::ALL {
            validate_upstreams(
                interface,
                self.upstreams.for_interface(interface),
                &mut errors,
            );
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors(errors))
        }
    }

    fn validate_server(&self, errors: &mut Vec<String>) {
        let server = &self.server;
        let loopback = server.listen.ip().is_loopback();

        if server.auth_tokens.iter().any(Secret::is_empty) {
            errors.push("server.auth_tokens 不能包含空字符串".to_string());
        }
        if server.auth_tokens.is_empty() {
            if !loopback {
                errors.push(format!(
                    "监听非回环地址 {} 时必须配置 server.auth_tokens",
                    server.listen
                ));
            } else if !server.allow_anonymous {
                errors.push(
                    "未配置 server.auth_tokens；如确需匿名访问，请显式设置 server.allow_anonymous = true（仅限回环地址）"
                        .to_string(),
                );
            }
        }
        if server.allow_anonymous && !loopback {
            errors.push(format!(
                "server.allow_anonymous 只能与回环地址一起使用，当前监听 {}",
                server.listen
            ));
        }
        if server.max_body_bytes == 0 {
            errors.push("server.max_body_bytes 必须大于 0".to_string());
        }
        if let Some(proxy) = server.proxy_url.as_deref().filter(|p| !p.is_empty()) {
            if let Err(message) = validate_proxy_url(proxy) {
                errors.push(format!("server.proxy_url: {message}"));
            }
        }
    }

    fn validate_limits(&self, errors: &mut Vec<String>) {
        let timeouts = &self.timeouts;
        for (name, value) in [
            ("timeouts.connect_secs", timeouts.connect_secs),
            ("timeouts.response_head_secs", timeouts.response_head_secs),
            ("timeouts.idle_secs", timeouts.idle_secs),
        ] {
            if value == 0 {
                errors.push(format!("{name} 必须大于 0"));
            }
        }

        let failover = &self.failover;
        for status in &failover.retry_on_status {
            if !(100..=599).contains(status) {
                errors.push(format!("failover.retry_on_status 包含无效状态码 {status}"));
            }
        }
        if failover.max_attempts == Some(0) {
            errors.push("failover.max_attempts 必须大于 0".to_string());
        }

        if self.breaker.failure_threshold == 0 {
            errors.push("breaker.failure_threshold 必须大于 0".to_string());
        }
        if self.breaker.open_secs == 0 {
            errors.push("breaker.open_secs 必须大于 0".to_string());
        }
    }
}

fn validate_upstreams(
    interface: Interface,
    upstreams: &[UpstreamConfig],
    errors: &mut Vec<String>,
) {
    let mut seen = HashSet::new();
    for (index, upstream) in upstreams.iter().enumerate() {
        let label = if upstream.id.is_empty() {
            format!("upstreams.{}[{index}]", interface.as_str())
        } else {
            format!("upstreams.{}[{}]", interface.as_str(), upstream.id)
        };

        if upstream.id.trim().is_empty() {
            errors.push(format!("{label}: id 不能为空"));
        } else if !seen.insert(upstream.id.as_str()) {
            errors.push(format!("{label}: id 重复"));
        }

        match validate_base_url(&upstream.base_url) {
            Ok(url) if looks_like_endpoint_path(&url) => errors.push(format!(
                "{label}: base_url 不应包含端点路径（如 /v1/messages、/chat/completions），只填写到服务根路径"
            )),
            Ok(_) => {}
            Err(message) => errors.push(format!("{label}: base_url {message}")),
        }

        if upstream.auth_scheme(interface) != AuthScheme::None {
            if upstream.api_key.is_empty() {
                errors.push(format!(
                    "{label}: api_key 不能为空（或设置 auth = \"none\"）"
                ));
            } else if upstream
                .api_key
                .expose()
                .bytes()
                .any(|b| b.is_ascii_control())
            {
                errors.push(format!("{label}: api_key 包含控制字符"));
            }
        }

        if let Some(prefix) = upstream.strip_prefix.as_deref() {
            if !is_valid_strip_prefix(prefix) {
                errors.push(format!(
                    "{label}: strip_prefix 必须以 / 开头、不以 / 结尾，例如 \"/v1\""
                ));
            }
        }

        if let Some(proxy) = upstream.proxy_url.as_deref().filter(|p| !p.is_empty()) {
            if let Err(message) = validate_proxy_url(proxy) {
                errors.push(format!("{label}: proxy_url {message}"));
            }
        }

        if upstream
            .model_map
            .iter()
            .any(|(from, to)| from.is_empty() || to.is_empty())
        {
            errors.push(format!("{label}: model_map 的键与值都不能为空字符串"));
        }
        if upstream.default_max_output_tokens == Some(0) {
            errors.push(format!("{label}: default_max_output_tokens 必须大于 0"));
        }
        if upstream.converts(interface) && !CONVERTIBLE_CLIENTS.contains(&interface) {
            errors.push(format!(
                "{label}: {} 接口的协议转换 / model_map 本期不支持（仅 claude、openai_responses 接口可配置 protocol 与 model_map）",
                interface.as_str()
            ));
        }
    }
}

/// 本期可作为转换客户端协议的接口
pub const CONVERTIBLE_CLIENTS: [Interface; 2] = [Interface::Claude, Interface::OpenaiResponses];

/// 解析并校验 `base_url`：只允许 http / https，必须有 host，不允许 userinfo 与 fragment。
pub fn validate_base_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("无法解析: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("只支持 http / https，当前为 {}", url.scheme()));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("缺少 host".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("不能包含用户名或密码".to_string());
    }
    if url.fragment().is_some() {
        return Err("不能包含 #fragment".to_string());
    }
    Ok(url)
}

/// 解析并校验出站代理地址。
pub fn validate_proxy_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("无法解析: {e}"))?;
    if !PROXY_SCHEMES.contains(&url.scheme()) {
        return Err(format!(
            "只支持 http / https / socks5 / socks5h，当前为 {}",
            url.scheme()
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("缺少 host".to_string());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(id: &str, base_url: &str) -> UpstreamConfig {
        UpstreamConfig {
            id: id.to_string(),
            base_url: base_url.to_string(),
            api_key: Secret::new("sk-test"),
            auth: None,
            strip_prefix: None,
            proxy_url: None,
            protocol: None,
            model_map: ModelMap::new(),
            default_max_output_tokens: None,
        }
    }

    fn valid_config() -> RelayConfig {
        let mut config = RelayConfig::default();
        config.server.auth_tokens = vec![Secret::new("relay-token")];
        config.upstreams.claude = vec![upstream("anthropic", "https://api.anthropic.com")];
        config
    }

    fn errors_of(config: &RelayConfig) -> Vec<String> {
        config.validate().err().map(|e| e.0).unwrap_or_default()
    }

    #[test]
    fn defaults_match_design() {
        let config = RelayConfig::default();
        assert_eq!(config.server.listen.to_string(), DEFAULT_LISTEN);
        assert!(!config.server.allow_anonymous);
        assert!(config.server.preserve_header_case);
        assert_eq!(config.server.max_body_bytes, 200 * 1024 * 1024);
        assert!(config.tls.native_roots);
        assert_eq!(config.timeouts.response_head_secs, 120);
        assert_eq!(config.timeouts.idle_secs, 300);
        assert_eq!(
            config.failover.retry_on_status,
            vec![429, 500, 502, 503, 504, 529]
        );
        assert_eq!(config.failover.retry_body_bytes, 1024 * 1024);
        assert_eq!(config.breaker.failure_threshold, 5);
        assert_eq!(config.breaker.open_secs, 60);
    }

    #[test]
    fn valid_config_passes() {
        assert_eq!(valid_config().validate(), Ok(()));
    }

    #[test]
    fn anonymous_is_rejected_by_default() {
        let mut config = valid_config();
        config.server.auth_tokens.clear();
        assert!(errors_of(&config)[0].contains("allow_anonymous"));

        config.server.allow_anonymous = true;
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn non_loopback_requires_tokens_even_with_allow_anonymous() {
        let mut config = valid_config();
        config.server.listen = "0.0.0.0:15721".parse().unwrap();
        assert_eq!(config.validate(), Ok(()));

        config.server.auth_tokens.clear();
        config.server.allow_anonymous = true;
        let errors = errors_of(&config);
        assert!(errors
            .iter()
            .any(|e| e.contains("必须配置 server.auth_tokens")));
        assert!(errors.iter().any(|e| e.contains("只能与回环地址")));
    }

    #[test]
    fn empty_token_is_rejected() {
        let mut config = valid_config();
        config.server.auth_tokens.push(Secret::new(""));
        assert!(errors_of(&config)[0].contains("空字符串"));
    }

    #[test]
    fn upstream_rules() {
        let mut config = valid_config();
        let mut no_key = upstream("no-key", "https://example.com");
        no_key.api_key = Secret::default();
        let mut keyless = upstream("keyless", "https://example.com");
        keyless.api_key = Secret::default();
        keyless.auth = Some(AuthScheme::None);
        let mut bad_prefix = upstream("bad-prefix", "https://example.com");
        bad_prefix.strip_prefix = Some("v1/".to_string());
        let mut bad_proxy = upstream("bad-proxy", "https://example.com");
        bad_proxy.proxy_url = Some("ftp://proxy".to_string());
        let mut direct = upstream("direct", "https://example.com");
        direct.proxy_url = Some(String::new());

        config.upstreams.openai_chat = vec![
            upstream("dup", "https://a.example.com"),
            upstream("dup", "https://b.example.com"),
            upstream("", "https://c.example.com"),
            upstream("pasted", "https://api.deepseek.com/v1/chat/completions"),
            upstream("ftp", "ftp://example.com"),
            upstream("creds", "https://user:pass@example.com"),
            upstream("frag", "https://example.com/#x"),
            no_key,
            keyless,
            bad_prefix,
            bad_proxy,
            direct,
        ];

        let errors = errors_of(&config).join("\n");
        assert!(errors.contains("[dup]: id 重复"), "{errors}");
        assert!(errors.contains("openai_chat[2]: id 不能为空"), "{errors}");
        assert!(
            errors.contains("[pasted]: base_url 不应包含端点路径"),
            "{errors}"
        );
        assert!(
            errors.contains("[ftp]: base_url 只支持 http / https"),
            "{errors}"
        );
        assert!(
            errors.contains("[creds]: base_url 不能包含用户名或密码"),
            "{errors}"
        );
        assert!(
            errors.contains("[frag]: base_url 不能包含 #fragment"),
            "{errors}"
        );
        assert!(errors.contains("[no-key]: api_key 不能为空"), "{errors}");
        assert!(!errors.contains("[keyless]"), "{errors}");
        assert!(errors.contains("[bad-prefix]: strip_prefix"), "{errors}");
        assert!(errors.contains("[bad-proxy]: proxy_url 只支持"), "{errors}");
        assert!(!errors.contains("[direct]"), "{errors}");
    }

    #[test]
    fn same_id_in_different_interfaces_is_allowed() {
        let mut config = valid_config();
        config.upstreams.openai_chat = vec![upstream("anthropic", "https://example.com")];
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn limits_must_be_positive() {
        let mut config = valid_config();
        config.timeouts.idle_secs = 0;
        config.failover.max_attempts = Some(0);
        config.failover.retry_on_status.push(700);
        config.breaker.failure_threshold = 0;
        config.server.max_body_bytes = 0;
        let errors = errors_of(&config).join("\n");
        assert!(errors.contains("timeouts.idle_secs"));
        assert!(errors.contains("failover.max_attempts"));
        assert!(errors.contains("无效状态码 700"));
        assert!(errors.contains("breaker.failure_threshold"));
        assert!(errors.contains("server.max_body_bytes"));
    }

    #[test]
    fn auth_scheme_defaults_and_override() {
        let mut up = upstream("u", "https://example.com");
        assert_eq!(up.auth_scheme(Interface::Claude), AuthScheme::XApiKey);
        assert_eq!(up.auth_scheme(Interface::OpenaiChat), AuthScheme::Bearer);
        assert_eq!(
            up.auth_scheme(Interface::OpenaiResponses),
            AuthScheme::Bearer
        );
        assert_eq!(up.auth_scheme(Interface::Gemini), AuthScheme::XGoogApiKey);
        up.auth = Some(AuthScheme::Bearer);
        assert_eq!(up.auth_scheme(Interface::Claude), AuthScheme::Bearer);
    }

    #[test]
    fn auth_default_follows_upstream_protocol() {
        let mut up = upstream("u", "https://example.com");
        up.protocol = Some(Interface::OpenaiChat);
        assert!(up.converts(Interface::Claude));
        assert_eq!(up.auth_scheme(Interface::Claude), AuthScheme::Bearer);
        up.protocol = Some(Interface::Gemini);
        assert_eq!(
            up.auth_scheme(Interface::OpenaiResponses),
            AuthScheme::XGoogApiKey
        );
        up.auth = Some(AuthScheme::XApiKey);
        assert_eq!(
            up.auth_scheme(Interface::OpenaiResponses),
            AuthScheme::XApiKey
        );
    }

    #[test]
    fn conversion_rules() {
        let mut same = upstream("same", "https://example.com");
        assert!(!same.converts(Interface::Claude));
        same.protocol = Some(Interface::Claude);
        assert!(
            !same.converts(Interface::Claude),
            "explicit same protocol is passthrough"
        );
        same.model_map.insert("a".into(), "b".into());
        assert!(
            same.converts(Interface::Claude),
            "model_map forces identity conversion"
        );

        let mut config = valid_config();
        let mut chat = upstream("chat", "https://example.com");
        chat.protocol = Some(Interface::OpenaiChat);
        let mut bad_map = upstream("bad-map", "https://example.com");
        bad_map.model_map.insert("".into(), "x".into());
        bad_map.default_max_output_tokens = Some(0);
        config.upstreams.claude.push(chat.clone());
        config.upstreams.claude.push(bad_map);
        let mut from_chat = upstream("from-chat", "https://example.com");
        from_chat.protocol = Some(Interface::Claude);
        config.upstreams.openai_chat = vec![from_chat];
        let mut gemini_map = upstream("gemini-map", "https://example.com");
        gemini_map.model_map.insert("a".into(), "b".into());
        config.upstreams.gemini = vec![gemini_map];
        config.upstreams.openai_responses = vec![chat];

        let errors = errors_of(&config).join("\n");
        assert!(errors.contains("[bad-map]: model_map"), "{errors}");
        assert!(
            errors.contains("[bad-map]: default_max_output_tokens"),
            "{errors}"
        );
        assert!(
            errors.contains("openai_chat[from-chat]: openai_chat 接口的协议转换"),
            "{errors}"
        );
        assert!(
            errors.contains("gemini[gemini-map]: gemini 接口的协议转换"),
            "{errors}"
        );
        assert!(!errors.contains("claude[chat]"), "{errors}");
        assert!(!errors.contains("openai_responses[chat]"), "{errors}");
    }

    #[test]
    fn protocol_and_model_map_deserialize() {
        let json = serde_json::json!({
            "server": { "auth_tokens": ["t"] },
            "upstreams": { "claude": [{
                "id": "c", "base_url": "https://api.deepseek.com", "api_key": "k",
                "protocol": "openai_chat",
                "model_map": { "claude-sonnet-5": "deepseek-chat", "*": "deepseek-chat" }
            }] }
        });
        let config: RelayConfig = serde_json::from_value(json).unwrap();
        let up = &config.upstreams.claude[0];
        assert_eq!(up.protocol, Some(Interface::OpenaiChat));
        assert_eq!(
            up.model_map.get("*").map(String::as_str),
            Some("deepseek-chat")
        );
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn effective_proxy_resolution() {
        let mut up = upstream("u", "https://example.com");
        assert_eq!(
            up.effective_proxy(Some("socks5h://p:1080")),
            Some("socks5h://p:1080")
        );
        assert_eq!(up.effective_proxy(Some("")), None);
        assert_eq!(up.effective_proxy(None), None);
        up.proxy_url = Some(String::new());
        assert_eq!(up.effective_proxy(Some("socks5h://p:1080")), None);
        up.proxy_url = Some("http://q:8080".to_string());
        assert_eq!(up.effective_proxy(None), Some("http://q:8080"));
    }

    #[test]
    fn secret_is_redacted_in_debug() {
        let config = valid_config();
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-test"));
        assert!(!debug.contains("relay-token"));
        assert!(debug.contains("Secret(***)"));
    }

    #[test]
    fn parses_design_example_shape() {
        let json = serde_json::json!({
            "server": { "listen": "127.0.0.1:15721", "auth_tokens": ["t"] },
            "failover": { "retry_on_status": [429, 503] },
            "upstreams": {
                "claude": [{ "id": "a", "base_url": "https://api.anthropic.com", "api_key": "k", "auth": "bearer" }],
                "gemini": [{ "id": "g", "base_url": "https://generativelanguage.googleapis.com", "api_key": "k", "auth": "x-goog-api-key" }]
            }
        });
        let config: RelayConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.failover.retry_on_status, vec![429, 503]);
        assert_eq!(config.timeouts.idle_secs, 300);
        assert_eq!(config.upstreams.claude[0].auth, Some(AuthScheme::Bearer));
        assert_eq!(config.validate(), Ok(()));

        let unknown = serde_json::json!({ "server": { "listen_addr": "x" } });
        assert!(serde_json::from_value::<RelayConfig>(unknown).is_err());
    }
}
