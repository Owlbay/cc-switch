//! 访问日志（设计文档 §8、§11 T11）。
//!
//! 用最小的 `tracing::Subscriber` 捕获全部事件字段：每个请求恰好一行访问日志，
//! 且任何日志（含失败时的 warn）都不包含 token、上游 Key、body 或 query。

mod support;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cc_proxy_core::access_log::ACCESS_TARGET;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

// ---------- 捕获用 Subscriber ----------

#[derive(Debug, Clone)]
struct Captured {
    target: String,
    fields: BTreeMap<String, String>,
}

impl Captured {
    fn field(&self, name: &str) -> &str {
        self.fields.get(name).map(String::as_str).unwrap_or("")
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Captured>>>);

impl Capture {
    fn all(&self) -> Vec<Captured> {
        self.0.lock().unwrap().clone()
    }

    fn access(&self) -> Vec<Captured> {
        self.all()
            .into_iter()
            .filter(|e| e.target == ACCESS_TARGET)
            .collect()
    }

    /// 轮询等待访问日志数量达到 `n`（连接任务里的 drop 可能稍晚发生）
    async fn wait_access(&self, n: usize) -> Vec<Captured> {
        for _ in 0..100 {
            let got = self.access();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.access()
    }

    fn assert_no_secret(&self, secrets: &[&str]) {
        for event in self.all() {
            for (name, value) in &event.fields {
                for secret in secrets {
                    assert!(
                        !value.contains(secret),
                        "log field {name}={value:?} (target {}) leaks {secret:?}",
                        event.target
                    );
                }
            }
        }
    }
}

struct Visitor<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Visitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = BTreeMap::new();
        event.record(&mut Visitor(&mut fields));
        self.0.lock().unwrap().push(Captured {
            target: event.metadata().target().to_string(),
            fields,
        });
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

// ---------- 用例 ----------

const BODY_MARKER: &str = "SECRET-BODY-MARKER";
const QUERY_MARKER: &str = "QUERY-MARKER";
const RESPONSE_MARKER: &str = "RESPONSE-BODY-MARKER";

fn secrets() -> Vec<&'static str> {
    vec![
        RELAY_TOKEN,
        UPSTREAM_KEY,
        BODY_MARKER,
        QUERY_MARKER,
        RESPONSE_MARKER,
    ]
}

fn body() -> Vec<u8> {
    format!("{{\"model\":\"m\",\"input\":\"{BODY_MARKER}\"}}").into_bytes()
}

fn ok() -> Vec<Segment> {
    vec![Segment::now(raw_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        format!("{{\"text\":\"{RESPONSE_MARKER}\"}}").as_bytes(),
    ))]
}

fn claude_request() -> Vec<u8> {
    raw_request(
        "POST",
        &format!("/v1/messages?session={QUERY_MARKER}"),
        &[("Host", "relay"), ("x-api-key", RELAY_TOKEN)],
        &body(),
    )
}

#[tokio::test]
async fn one_complete_line_per_request_without_secrets() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let mut up = MockUpstream::start(ok()).await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    let resp = request(relay.addr, &claude_request()).await;
    assert_eq!(resp.status, 200);
    up.next_request().await;

    let access = capture.wait_access(1).await;
    assert_eq!(access.len(), 1, "{access:?}");
    let line = &access[0];
    assert!(line.field("request_id").starts_with("r-"));
    assert_eq!(line.field("method"), "POST");
    assert_eq!(line.field("path"), "/v1/messages");
    assert_eq!(line.field("interface"), "claude");
    assert_eq!(line.field("upstream"), "a");
    assert_eq!(line.field("attempts"), "1");
    assert_eq!(line.field("status"), "200");
    assert_eq!(line.field("outcome"), "complete");
    assert_eq!(line.field("request_bytes"), body().len().to_string());
    assert_eq!(line.field("response_bytes"), resp.body.len().to_string());
    for name in ["head_ms", "total_ms", "error"] {
        assert!(line.fields.contains_key(name), "missing {name}");
    }
    capture.assert_no_secret(&secrets());
}

#[tokio::test]
async fn failover_logs_one_access_line_and_warnings_stay_clean() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "503 Service Unavailable",
        &[],
        format!("{{\"error\":\"{RESPONSE_MARKER}\"}}").as_bytes(),
    ))])
    .await;
    let mut b = MockUpstream::start(ok()).await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let mut config = base_config();
    config.upstreams.claude = vec![
        upstream("a", &a.base_url()),
        upstream("dead", &dead),
        upstream("b", &b.base_url()),
    ];
    let relay = start_relay(config).await;

    assert_eq!(request(relay.addr, &claude_request()).await.status, 200);
    a.next_request().await;
    b.next_request().await;

    let access = capture.wait_access(1).await;
    assert_eq!(access.len(), 1, "{access:?}");
    assert_eq!(access[0].field("attempts"), "3");
    assert_eq!(access[0].field("upstream"), "b");
    assert_eq!(access[0].field("status"), "200");
    // 失败尝试各有一行 warn，并带同一个 request_id
    let warnings: Vec<_> = capture
        .all()
        .into_iter()
        .filter(|e| e.target != ACCESS_TARGET && e.fields.contains_key("upstream"))
        .collect();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    for warning in &warnings {
        assert_eq!(warning.field("request_id"), access[0].field("request_id"));
    }
    capture.assert_no_secret(&secrets());
}

#[tokio::test]
async fn relay_generated_responses_are_logged() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let relay = start_relay(base_config()).await;

    let denied = request(
        relay.addr,
        &raw_request(
            "POST",
            &format!("/v1/messages?key={QUERY_MARKER}"),
            &[("Host", "relay"), ("x-api-key", "wrong")],
            &body(),
        ),
    )
    .await;
    assert_eq!(denied.status, 401);
    let health = request(
        relay.addr,
        &raw_request("GET", "/_relay/health", &[("Host", "relay")], b""),
    )
    .await;
    assert_eq!(health.status, 200);

    let access = capture.wait_access(2).await;
    assert_eq!(access.len(), 2, "{access:?}");
    assert_eq!(access[0].field("status"), "401");
    assert_eq!(access[0].field("interface"), "-");
    assert_eq!(access[0].field("outcome"), "complete");
    assert_eq!(
        access[0].field("response_bytes"),
        denied.body.len().to_string()
    );
    assert_eq!(access[1].field("path"), "/_relay/health");
    assert_eq!(access[1].field("status"), "200");
    capture.assert_no_secret(&secrets());
}

#[tokio::test]
async fn bodiless_responses_are_complete_not_aborted() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let mut up = MockUpstream::start(vec![Segment::now(
        b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec(),
    )])
    .await;
    let mut config = base_config();
    config.upstreams.openai_responses = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;
    let bearer = format!("Bearer {RELAY_TOKEN}");

    let resp = request(
        relay.addr,
        &raw_request(
            "DELETE",
            "/v1/responses/resp_1",
            &[("Host", "relay"), ("Authorization", &bearer)],
            b"",
        ),
    )
    .await;
    assert_eq!(resp.status, 204);
    up.next_request().await;

    let access = capture.wait_access(1).await;
    assert_eq!(access.len(), 1, "{access:?}");
    assert_eq!(access[0].field("status"), "204");
    assert_eq!(access[0].field("outcome"), "complete");
}

#[tokio::test]
async fn replayed_buffered_response_is_logged() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "429 Too Many Requests",
        &[("Retry-After", "5")],
        b"{\"error\":\"limited\"}",
    ))])
    .await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("dead", &dead)];
    let relay = start_relay(config).await;

    let resp = request(relay.addr, &claude_request()).await;
    assert_eq!(resp.status, 429);
    a.next_request().await;

    let access = capture.wait_access(1).await;
    assert_eq!(access.len(), 1, "{access:?}");
    assert_eq!(access[0].field("status"), "429");
    assert_eq!(access[0].field("attempts"), "2");
    assert_eq!(access[0].field("outcome"), "complete");
    assert_eq!(
        access[0].field("response_bytes"),
        resp.body.len().to_string()
    );
}

#[tokio::test]
async fn disconnect_before_head_is_cancelled() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let mut up = MockUpstream::start(vec![Segment::after(
        Duration::from_secs(10),
        raw_response("200 OK", &[], b"{}"),
    )])
    .await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    let mut client = TcpStream::connect(relay.addr).await.unwrap();
    client.write_all(&claude_request()).await.unwrap();
    up.next_request().await;
    drop(client);

    let access = capture.wait_access(1).await;
    assert_eq!(access.len(), 1, "{access:?}");
    assert_eq!(access[0].field("outcome"), "cancelled");
    assert_eq!(access[0].field("attempts"), "1");
}

const SSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";

fn first_event() -> Vec<u8> {
    let data = b"data: {\"n\":1}\n\n";
    let mut out = SSE_HEAD.to_vec();
    out.extend(format!("{:x}\r\n", data.len()).into_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

#[tokio::test]
async fn disconnect_mid_stream_is_aborted() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let mut up = MockUpstream::start(vec![
        Segment::now(first_event()),
        Segment::after(Duration::from_secs(10), b"0\r\n\r\n".to_vec()),
    ])
    .await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    let mut client = TcpStream::connect(relay.addr).await.unwrap();
    client.write_all(&claude_request()).await.unwrap();
    let mut received = Vec::new();
    let mut buf = [0u8; 512];
    while find(&received, b"data: {\"n\":1}").is_none() {
        let n = client.read(&mut buf).await.unwrap();
        assert!(n > 0);
        received.extend_from_slice(&buf[..n]);
    }
    up.next_request().await;
    drop(client);

    let access = capture.wait_access(1).await;
    assert_eq!(access.len(), 1, "{access:?}");
    assert_eq!(access[0].field("outcome"), "aborted");
    assert_eq!(access[0].field("status"), "200");
    assert_eq!(access[0].field("response_bytes"), "15");
}

#[tokio::test]
async fn idle_timeout_mid_stream_is_error() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let mut up = MockUpstream::start(vec![
        Segment::now(first_event()),
        Segment::after(Duration::from_secs(10), b"0\r\n\r\n".to_vec()),
    ])
    .await;
    let mut config = base_config();
    config.timeouts.idle_secs = 1;
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    send_raw(relay.addr, &claude_request()).await;
    up.next_request().await;

    let access = capture.wait_access(1).await;
    assert_eq!(access.len(), 1, "{access:?}");
    assert_eq!(access[0].field("outcome"), "error");
    assert!(access[0].field("error").contains("no data"), "{access:?}");
    capture.assert_no_secret(&secrets());
}
