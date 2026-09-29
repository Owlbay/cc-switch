//! HTTPS 上游与自定义 CA（设计文档 §5.6、§11 T15）。

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use support::*;
use tokio_rustls::TlsAcceptor;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

fn acceptor() -> TlsAcceptor {
    let certs = CertificateDer::pem_file_iter(fixture("localhost.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("localhost.key")).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

fn ok_json() -> Vec<Segment> {
    vec![Segment::now(raw_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        b"{\"ok\":true}",
    ))]
}

const BODY: &[u8] = b"{\"model\":\"m\",\"input\":\"hi\"}";

async fn send(relay: &RunningRelay) -> RawResponse {
    request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/responses",
            &[
                ("Host", "relay"),
                ("Authorization", &format!("Bearer {RELAY_TOKEN}")),
            ],
            BODY,
        ),
    )
    .await
}

#[tokio::test]
async fn trusted_extra_ca_allows_https_upstream() {
    let mut up = MockUpstream::start_tls(ok_json(), acceptor()).await;
    let mut config = base_config();
    config.tls.extra_ca_file = Some(fixture("ca.pem"));
    config.upstreams.openai_responses = vec![upstream(
        "tls",
        &format!("https://localhost:{}", up.addr.port()),
    )];
    let relay = start_relay(config).await;

    let resp = send(&relay).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"{\"ok\":true}");

    let got = up.next_request().await;
    assert_eq!(got.target, "/v1/responses");
    assert_eq!(got.body, BODY);
    assert_eq!(
        got.header_str("host"),
        Some(format!("localhost:{}", up.addr.port()).as_str())
    );
    assert_eq!(
        got.header_str("authorization"),
        Some(format!("Bearer {UPSTREAM_KEY}").as_str())
    );
}

#[tokio::test]
async fn untrusted_certificate_is_a_bad_gateway() {
    let mut up = MockUpstream::start_tls(ok_json(), acceptor()).await;
    let mut config = base_config();
    // 只有内置根证书，不信任测试 CA
    config.upstreams.openai_responses = vec![upstream(
        "tls",
        &format!("https://localhost:{}", up.addr.port()),
    )];
    let relay = start_relay(config).await;

    let resp = send(&relay).await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.json()["error"]["type"], "relay_bad_gateway");
    up.assert_no_request(Duration::from_millis(200)).await;
}
