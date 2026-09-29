//! 中转自身端点与启动校验（设计文档 §6.2、§8、§11 T9）。

mod support;

use std::time::Duration;

use cc_proxy_core::config::Secret;
use cc_proxy_core::upstream::BuildError;
use cc_proxy_core::Relay;
use support::*;

#[tokio::test]
async fn health_needs_no_token_and_never_reaches_upstream() {
    let mut up = MockUpstream::start(vec![]).await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    let resp = request(
        relay.addr,
        &raw_request("GET", "/_relay/health", &[("Host", "relay")], b""),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header_str("content-type"), Some("application/json"));
    assert_eq!(resp.json(), serde_json::json!({ "status": "ok" }));

    let head = request(
        relay.addr,
        &raw_request("HEAD", "/_relay/health", &[("Host", "relay")], b""),
    )
    .await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());

    up.assert_no_request(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn unknown_relay_endpoints_are_not_found() {
    let relay = start_relay(base_config()).await;
    for (method, path) in [
        ("POST", "/_relay/health"),
        ("GET", "/_relay/"),
        ("GET", "/_relay/config"),
        ("GET", "/_relay/health/extra"),
    ] {
        let resp = request(
            relay.addr,
            &raw_request(method, path, &[("Host", "relay")], b""),
        )
        .await;
        assert_eq!(resp.status, 404, "{method} {path}");
        assert_eq!(resp.json()["error"]["type"], "relay_not_found");
    }
}

#[test]
fn startup_rejects_non_loopback_listener_without_tokens() {
    let mut config = base_config();
    config.server.listen = "0.0.0.0:15721".parse().unwrap();
    config.server.auth_tokens.clear();
    config.server.allow_anonymous = true;
    let error = Relay::new(&config).err().expect("must refuse to start");
    let BuildError::Config(errors) = error else {
        panic!("unexpected error: {error}");
    };
    let text = errors.to_string();
    assert!(text.contains("必须配置 server.auth_tokens"), "{text}");
    assert!(text.contains("只能与回环地址"), "{text}");
}

#[test]
fn startup_rejects_missing_tokens_without_allow_anonymous() {
    let mut config = base_config();
    config.server.auth_tokens.clear();
    let error = Relay::new(&config).err().expect("must refuse to start");
    assert!(matches!(error, BuildError::Config(_)), "{error}");
    assert!(error.to_string().contains("allow_anonymous"), "{error}");
}

#[test]
fn startup_accepts_tokens_on_any_address() {
    let mut config = base_config();
    config.server.listen = "0.0.0.0:15721".parse().unwrap();
    config.server.auth_tokens = vec![Secret::new(RELAY_TOKEN)];
    assert!(Relay::new(&config).is_ok());
}
