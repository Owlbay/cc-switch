//! 上游 HTTP 客户端与出站代理（设计文档 §5.6）。
//!
//! 四种连接方式的 `Client` 类型各不相同，用 [`UpstreamClient`] 统一。相同出站代理设置的
//! 上游共享一个 `Client`（连接池按 `Client` 隔离）。只用 HTTP/1.1。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderValue, Request, Response, Uri};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::proxy::{SocksV5, Tunnel};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::{Builder, Client};
use hyper_util::rt::TokioExecutor;
use rustls::ClientConfig;
use url::Url;

use crate::config::{validate_base_url, validate_proxy_url, AuthScheme, RelayConfig, Secret};
use crate::interface::Interface;
use crate::tls::{self, TlsError};

/// 发往上游的请求 body：已完整缓冲，故障转移时可重放。
pub type UpstreamBody = Full<Bytes>;

type Direct = Client<HttpsConnector<HttpConnector>, UpstreamBody>;
type HttpProxy = Client<HttpsConnector<Tunnel<HttpConnector>>, UpstreamBody>;
type HttpsProxy = Client<HttpsConnector<Tunnel<HttpsConnector<HttpConnector>>>, UpstreamBody>;
type Socks = Client<HttpsConnector<SocksV5<HttpConnector>>, UpstreamBody>;

/// 统一四种连接方式的上游客户端。
pub enum UpstreamClient {
    Direct(Direct),
    HttpProxy(HttpProxy),
    HttpsProxy(HttpsProxy),
    Socks(Socks),
}

/// 构造客户端所需的公共选项
#[derive(Clone)]
pub struct ClientOptions {
    pub tls: Arc<ClientConfig>,
    pub connect_timeout: Duration,
    pub preserve_header_case: bool,
}

impl UpstreamClient {
    /// `proxy` 为 `None` 时直连。代理地址必须已通过 [`validate_proxy_url`]。
    pub fn new(proxy: Option<&Url>, options: &ClientOptions) -> Result<Self, BuildError> {
        let Some(proxy) = proxy else {
            return Ok(Self::Direct(
                builder(options).build(https(options, http_connector(options))),
            ));
        };
        let target = proxy_target(proxy)?;
        let credentials = proxy_credentials(proxy);
        let client = match proxy.scheme() {
            "http" | "https" => {
                let tunnel_auth = credentials
                    .as_ref()
                    .map(|(user, pass)| basic_auth(user, pass))
                    .transpose()?;
                if proxy.scheme() == "http" {
                    let mut tunnel = Tunnel::new(target, http_connector(options));
                    if let Some(auth) = tunnel_auth {
                        tunnel = tunnel.with_auth(auth);
                    }
                    Self::HttpProxy(builder(options).build(https(options, tunnel)))
                } else {
                    let mut tunnel = Tunnel::new(target, https(options, http_connector(options)));
                    if let Some(auth) = tunnel_auth {
                        tunnel = tunnel.with_auth(auth);
                    }
                    Self::HttpsProxy(builder(options).build(https(options, tunnel)))
                }
            }
            "socks5" | "socks5h" => {
                // socks5：本地解析域名；socks5h：交给代理解析
                let mut socks = SocksV5::new(target, http_connector(options))
                    .local_dns(proxy.scheme() == "socks5");
                if let Some((user, pass)) = credentials {
                    socks = socks.with_auth(user, pass);
                }
                Self::Socks(builder(options).build(https(options, socks)))
            }
            other => return Err(BuildError::ProxyScheme(other.to_string())),
        };
        Ok(client)
    }

    pub async fn request(
        &self,
        request: Request<UpstreamBody>,
    ) -> Result<Response<Incoming>, hyper_util::client::legacy::Error> {
        match self {
            Self::Direct(client) => client.request(request).await,
            Self::HttpProxy(client) => client.request(request).await,
            Self::HttpsProxy(client) => client.request(request).await,
            Self::Socks(client) => client.request(request).await,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Direct(_) => "direct",
            Self::HttpProxy(_) => "http-proxy",
            Self::HttpsProxy(_) => "https-proxy",
            Self::Socks(_) => "socks5",
        }
    }
}

fn builder(options: &ClientOptions) -> Builder {
    let mut builder = Client::builder(TokioExecutor::new());
    builder.http1_preserve_header_case(options.preserve_header_case);
    builder
}

fn http_connector(options: &ClientOptions) -> HttpConnector {
    let mut connector = HttpConnector::new();
    // 上游 scheme 由外层 HttpsConnector 处理
    connector.enforce_http(false);
    connector.set_connect_timeout(Some(options.connect_timeout));
    // SSE 多为小块写入，关闭 Nagle 降低延迟；不影响内容
    connector.set_nodelay(true);
    connector
}

fn https<C>(options: &ClientOptions, inner: C) -> HttpsConnector<C> {
    hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config((*options.tls).clone())
        .https_or_http()
        .enable_http1()
        .wrap_connector(inner)
}

/// 代理连接目标：只保留 scheme、host 与端口（去掉 userinfo 与 path）。
fn proxy_target(proxy: &Url) -> Result<Uri, BuildError> {
    let host = proxy
        .host_str()
        .ok_or_else(|| BuildError::ProxyUrl("缺少 host".to_string()))?;
    let port = proxy
        .port_or_known_default()
        .unwrap_or(if proxy.scheme().starts_with("socks") {
            1080
        } else {
            80
        });
    format!("{}://{host}:{port}", proxy.scheme())
        .parse()
        .map_err(|e: http::uri::InvalidUri| BuildError::ProxyUrl(e.to_string()))
}

/// 代理地址中的 `user:pass@`（百分号解码后）。
fn proxy_credentials(proxy: &Url) -> Option<(String, String)> {
    if proxy.username().is_empty() {
        return None;
    }
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .into_owned()
    };
    Some((
        decode(proxy.username()),
        decode(proxy.password().unwrap_or_default()),
    ))
}

fn basic_auth(user: &str, pass: &str) -> Result<HeaderValue, BuildError> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    let mut value = HeaderValue::from_str(&format!("Basic {encoded}"))
        .map_err(|e| BuildError::ProxyUrl(e.to_string()))?;
    value.set_sensitive(true);
    Ok(value)
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("配置无效:\n{0}")]
    Config(#[from] crate::config::ConfigErrors),
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("出站代理地址无效: {0}")]
    ProxyUrl(String),
    #[error("不支持的出站代理协议: {0}")]
    ProxyScheme(String),
}

/// 启动时解析好的上游
pub struct ResolvedUpstream {
    pub id: String,
    pub interface: Interface,
    pub base_url: Url,
    pub strip_prefix: Option<String>,
    pub auth: AuthScheme,
    pub api_key: Secret,
    pub client: Arc<UpstreamClient>,
}

/// 按接口分组的上游列表（保持配置顺序）
pub struct UpstreamSet {
    by_interface: HashMap<Interface, Vec<Arc<ResolvedUpstream>>>,
}

impl UpstreamSet {
    /// 校验配置并为每种出站代理设置构造一个客户端。
    pub fn build(config: &RelayConfig) -> Result<Self, BuildError> {
        config.validate()?;
        let options = ClientOptions {
            tls: tls::client_config(&config.tls)?,
            connect_timeout: Duration::from_secs(config.timeouts.connect_secs),
            preserve_header_case: config.server.preserve_header_case,
        };

        let global_proxy = config.server.proxy_url.as_deref();
        let mut clients: HashMap<Option<String>, Arc<UpstreamClient>> = HashMap::new();
        let mut by_interface = HashMap::new();

        for interface in Interface::ALL {
            let mut resolved = Vec::new();
            for upstream in config.upstreams.for_interface(interface) {
                let proxy = upstream.effective_proxy(global_proxy).map(str::to_string);
                let client = match clients.get(&proxy) {
                    Some(client) => Arc::clone(client),
                    None => {
                        let proxy_url = proxy
                            .as_deref()
                            .map(validate_proxy_url)
                            .transpose()
                            .map_err(BuildError::ProxyUrl)?;
                        let client = Arc::new(UpstreamClient::new(proxy_url.as_ref(), &options)?);
                        clients.insert(proxy, Arc::clone(&client));
                        client
                    }
                };
                resolved.push(Arc::new(ResolvedUpstream {
                    id: upstream.id.clone(),
                    interface,
                    base_url: validate_base_url(&upstream.base_url)
                        .map_err(BuildError::ProxyUrl)?,
                    strip_prefix: upstream.strip_prefix.clone(),
                    auth: upstream.auth_scheme(interface),
                    api_key: upstream.api_key.clone(),
                    client,
                }));
            }
            by_interface.insert(interface, resolved);
        }
        Ok(Self { by_interface })
    }

    pub fn for_interface(&self, interface: Interface) -> &[Arc<ResolvedUpstream>] {
        self.by_interface
            .get(&interface)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TlsConfig, UpstreamConfig};

    fn options() -> ClientOptions {
        ClientOptions {
            tls: tls::client_config(&TlsConfig {
                native_roots: false,
                extra_ca_file: None,
            })
            .unwrap(),
            connect_timeout: Duration::from_secs(1),
            preserve_header_case: true,
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[tokio::test]
    async fn builds_every_proxy_kind() {
        let options = options();
        for (proxy, kind) in [
            (None, "direct"),
            (Some("http://127.0.0.1:7890"), "http-proxy"),
            (Some("https://proxy.example.com"), "https-proxy"),
            (Some("socks5://127.0.0.1:1080"), "socks5"),
            (Some("socks5h://user:pa%40ss@127.0.0.1"), "socks5"),
        ] {
            let proxy = proxy.map(url);
            let client = UpstreamClient::new(proxy.as_ref(), &options).unwrap();
            assert_eq!(client.kind(), kind);
        }
    }

    #[test]
    fn proxy_target_has_explicit_port_and_no_userinfo() {
        assert_eq!(
            proxy_target(&url("http://u:p@127.0.0.1:7890/ignored")).unwrap(),
            "http://127.0.0.1:7890/"
        );
        assert_eq!(
            proxy_target(&url("https://proxy.example.com")).unwrap(),
            "https://proxy.example.com:443/"
        );
        assert_eq!(
            proxy_target(&url("socks5h://127.0.0.1")).unwrap(),
            "socks5h://127.0.0.1:1080/"
        );
    }

    #[test]
    fn proxy_credentials_are_percent_decoded() {
        assert_eq!(proxy_credentials(&url("http://127.0.0.1:1")), None);
        assert_eq!(
            proxy_credentials(&url("http://us%20er:pa%40ss+1@127.0.0.1:1")),
            Some(("us er".to_string(), "pa@ss+1".to_string()))
        );
        assert_eq!(
            proxy_credentials(&url("socks5://only@127.0.0.1:1")),
            Some(("only".to_string(), String::new()))
        );
    }

    #[test]
    fn basic_auth_header() {
        let value = basic_auth("user", "pass").unwrap();
        assert_eq!(value.to_str().unwrap(), "Basic dXNlcjpwYXNz");
        assert!(value.is_sensitive());
    }

    fn upstream(id: &str, proxy_url: Option<&str>) -> UpstreamConfig {
        UpstreamConfig {
            id: id.to_string(),
            base_url: "https://example.com".to_string(),
            api_key: Secret::new("k"),
            auth: None,
            strip_prefix: None,
            proxy_url: proxy_url.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn upstreams_with_same_proxy_share_a_client() {
        let mut config = RelayConfig::default();
        config.server.auth_tokens = vec![Secret::new("t")];
        config.server.proxy_url = Some("http://127.0.0.1:7890".to_string());
        config.tls.native_roots = false;
        config.upstreams.claude = vec![upstream("a", None), upstream("b", Some(""))];
        config.upstreams.openai_chat = vec![upstream("c", None)];

        let set = UpstreamSet::build(&config).unwrap();
        let claude = set.for_interface(Interface::Claude);
        let chat = set.for_interface(Interface::OpenaiChat);
        assert_eq!(claude.len(), 2);
        assert_eq!(claude[0].client.kind(), "http-proxy");
        assert_eq!(claude[1].client.kind(), "direct");
        assert!(Arc::ptr_eq(&claude[0].client, &chat[0].client));
        assert_eq!(claude[0].auth, AuthScheme::XApiKey);
        assert_eq!(chat[0].auth, AuthScheme::Bearer);
        assert!(set.for_interface(Interface::Gemini).is_empty());
    }

    #[test]
    fn invalid_config_is_rejected() {
        let config = RelayConfig::default();
        assert!(matches!(
            UpstreamSet::build(&config),
            Err(BuildError::Config(_))
        ));
    }
}
