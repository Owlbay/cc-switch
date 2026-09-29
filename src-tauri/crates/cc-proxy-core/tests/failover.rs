//! 故障转移与熔断集成测试（设计文档 §5.2、§5.5、§11 T6、T8）。

mod support;

use std::time::{Duration, Instant};

use cc_proxy_core::config::RelayConfig;
use support::*;

const BODY: &[u8] =
    b"{\"model\":\"m\",  \"messages\":[{\"role\":\"user\",\"content\":\"h\xc3\xa9\"}]}\n";

fn ok(body: &str) -> Vec<Segment> {
    vec![Segment::now(raw_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    ))]
}

fn status(line: &str, headers: &[(&str, &str)], body: &str) -> Vec<Segment> {
    vec![Segment::now(raw_response(line, headers, body.as_bytes()))]
}

fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

async fn relay_with(configure: impl FnOnce(&mut RelayConfig)) -> RunningRelay {
    let mut config = base_config();
    configure(&mut config);
    start_relay(config).await
}

async fn send_messages(relay: &RunningRelay) -> RawResponse {
    request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/messages?beta=true",
            &[
                ("Host", "relay"),
                ("x-api-key", RELAY_TOKEN),
                ("Anthropic-Version", "2023-06-01"),
            ],
            BODY,
        ),
    )
    .await
}

// ---------- T6 ----------

#[tokio::test]
async fn t6_retryable_status_fails_over_and_resends_the_same_request() {
    let mut a = MockUpstream::start(status(
        "503 Service Unavailable",
        &[("Retry-After", "1")],
        "{\"error\":\"busy\"}",
    ))
    .await;
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    })
    .await;

    let resp = send_messages(&relay).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"{\"from\":\"b\"}");

    let got_a = a.next_request().await;
    let got_b = b.next_request().await;
    assert_eq!(got_a.target, "/v1/messages?beta=true");
    assert_eq!(got_b.target, got_a.target);
    assert_eq!(got_a.body, BODY);
    assert_eq!(got_b.body, BODY);
    // 除 host 外，两次发送的头完全一致（含原始大小写与顺序）
    let without_host = |r: &RecordedRequest| {
        r.headers
            .iter()
            .filter(|(n, _)| !n.eq_ignore_ascii_case("host"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(without_host(&got_a), without_host(&got_b));
}

#[tokio::test]
async fn t6_connection_failure_fails_over() {
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.upstreams.claude = vec![
            upstream("dead", &closed_port_url()),
            upstream("b", &b.base_url()),
        ];
    })
    .await;

    let resp = send_messages(&relay).await;
    assert_eq!(resp.status, 200);
    assert_eq!(b.next_request().await.body, BODY);
}

#[tokio::test]
async fn t6_response_head_timeout_fails_over() {
    let mut slow = MockUpstream::start(vec![Segment::after(
        Duration::from_secs(10),
        raw_response("200 OK", &[], b"{\"from\":\"slow\"}"),
    )])
    .await;
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.timeouts.response_head_secs = 1;
        c.upstreams.claude = vec![
            upstream("slow", &slow.base_url()),
            upstream("b", &b.base_url()),
        ];
    })
    .await;

    let started = Instant::now();
    let resp = send_messages(&relay).await;
    let elapsed = started.elapsed();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"{\"from\":\"b\"}");
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5),
        "{elapsed:?}"
    );
    assert_eq!(slow.next_request().await.body, BODY);
    assert_eq!(b.next_request().await.body, BODY);
}

#[tokio::test]
async fn t6_non_retryable_status_is_relayed_without_failover() {
    let mut a = MockUpstream::start(status(
        "400 Bad Request",
        &[("Content-Type", "application/json")],
        "{\"error\":\"bad\"}",
    ))
    .await;
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    })
    .await;

    let resp = send_messages(&relay).await;
    assert_eq!(resp.status, 400);
    assert_eq!(resp.body, b"{\"error\":\"bad\"}");
    a.next_request().await;
    b.assert_no_request(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn t6_last_upstream_retryable_response_is_streamed_as_is() {
    let mut a =
        MockUpstream::start(status("503 Service Unavailable", &[], "{\"from\":\"a\"}")).await;
    let mut b = MockUpstream::start(status(
        "529 Site Overloaded",
        &[("Retry-After", "7"), ("X-Request-Id", "req_b")],
        "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}",
    ))
    .await;
    let relay = relay_with(|c| {
        c.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    })
    .await;

    let resp = send_messages(&relay).await;
    assert_eq!(resp.status, 529);
    assert_eq!(resp.header_str("retry-after"), Some("7"));
    assert_eq!(resp.header_str("x-request-id"), Some("req_b"));
    assert_eq!(
        resp.body,
        b"{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}"
    );
    a.next_request().await;
    b.next_request().await;
}

#[tokio::test]
async fn t6_buffered_response_is_replayed_when_a_later_attempt_has_no_response() {
    let mut a = MockUpstream::start(status(
        "429 Too Many Requests",
        &[("Retry-After", "30"), ("Content-Type", "application/json")],
        "{\"error\":\"rate limited\"}",
    ))
    .await;
    let relay = relay_with(|c| {
        c.upstreams.claude = vec![
            upstream("a", &a.base_url()),
            upstream("dead", &closed_port_url()),
        ];
    })
    .await;

    let resp = send_messages(&relay).await;
    assert_eq!(resp.status, 429);
    assert_eq!(resp.header_str("retry-after"), Some("30"));
    assert_eq!(resp.body, b"{\"error\":\"rate limited\"}");
    a.next_request().await;
}

#[tokio::test]
async fn t6_oversized_error_body_is_not_replayed() {
    let big = "x".repeat(4096);
    let mut a = MockUpstream::start(status("503 Service Unavailable", &[], &big)).await;
    let relay = relay_with(|c| {
        c.failover.retry_body_bytes = 1024;
        c.upstreams.claude = vec![
            upstream("a", &a.base_url()),
            upstream("dead", &closed_port_url()),
        ];
    })
    .await;

    let resp = send_messages(&relay).await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.json()["error"]["type"], "relay_bad_gateway");
    a.next_request().await;
}

#[tokio::test]
async fn t6_all_network_failures_return_bad_gateway() {
    let relay = relay_with(|c| {
        c.upstreams.claude = vec![
            upstream("x", &closed_port_url()),
            upstream("y", &closed_port_url()),
        ];
    })
    .await;
    let resp = send_messages(&relay).await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.json()["error"]["type"], "relay_bad_gateway");
}

#[tokio::test]
async fn t6_max_attempts_limits_failover() {
    let mut a =
        MockUpstream::start(status("503 Service Unavailable", &[], "{\"from\":\"a\"}")).await;
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.failover.max_attempts = Some(1);
        c.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    })
    .await;

    let resp = send_messages(&relay).await;
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body, b"{\"from\":\"a\"}");
    a.next_request().await;
    b.assert_no_request(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn t6_stateful_responses_calls_stay_on_the_first_upstream() {
    let mut a =
        MockUpstream::start(status("503 Service Unavailable", &[], "{\"from\":\"a\"}")).await;
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.upstreams.openai_responses =
            vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    })
    .await;
    let bearer = format!("Bearer {RELAY_TOKEN}");

    let resp = request(
        relay.addr,
        &raw_request(
            "GET",
            "/v1/responses/resp_1",
            &[("Host", "relay"), ("Authorization", &bearer)],
            b"",
        ),
    )
    .await;
    assert_eq!(resp.status, 503);
    assert_eq!(a.next_request().await.target, "/v1/responses/resp_1");
    b.assert_no_request(Duration::from_millis(200)).await;
}

// ---------- T8 ----------

#[tokio::test]
async fn t8_breaker_skips_a_failing_upstream() {
    let mut a = MockUpstream::start(status("503 Service Unavailable", &[], "{}")).await;
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.breaker.failure_threshold = 2;
        // 打开期足够长，慢机上也不会在断言前进入半开
        c.breaker.open_secs = 60;
        c.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    })
    .await;

    for _ in 0..2 {
        assert_eq!(send_messages(&relay).await.status, 200);
        a.next_request().await;
        b.next_request().await;
    }

    // 阈值已达到：a 被跳过
    assert_eq!(send_messages(&relay).await.status, 200);
    b.next_request().await;
    a.assert_no_request(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn t8_breaker_probes_after_open_period() {
    let mut a = MockUpstream::start(status("503 Service Unavailable", &[], "{}")).await;
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.breaker.failure_threshold = 2;
        c.breaker.open_secs = 1;
        c.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    })
    .await;

    for _ in 0..2 {
        assert_eq!(send_messages(&relay).await.status, 200);
        a.next_request().await;
        b.next_request().await;
    }

    // 打开期（1 秒）过后放行一次探测；等待更久只会更早进入半开，不影响断言
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(send_messages(&relay).await.status, 200);
    a.next_request().await;
    b.next_request().await;
}

#[tokio::test]
async fn t8_all_upstreams_open_returns_no_upstream_without_contacting_them() {
    let mut a =
        MockUpstream::start(status("500 Internal Server Error", &[], "{\"from\":\"a\"}")).await;
    let relay = relay_with(|c| {
        c.breaker.failure_threshold = 1;
        c.breaker.open_secs = 60;
        c.upstreams.claude = vec![upstream("a", &a.base_url())];
    })
    .await;

    let first = send_messages(&relay).await;
    assert_eq!(first.status, 500);
    assert_eq!(first.body, b"{\"from\":\"a\"}");
    a.next_request().await;

    let second = send_messages(&relay).await;
    assert_eq!(second.status, 503);
    assert_eq!(second.json()["error"]["type"], "relay_no_upstream");
    a.assert_no_request(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn t8_breakers_are_counted_per_upstream() {
    let mut a = MockUpstream::start(status("503 Service Unavailable", &[], "{}")).await;
    let mut good = MockUpstream::start(ok("{\"from\":\"good\"}")).await;
    let mut b = MockUpstream::start(ok("{\"from\":\"b\"}")).await;
    let relay = relay_with(|c| {
        c.breaker.failure_threshold = 2;
        c.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
        c.upstreams.openai_chat = vec![upstream("good", &good.base_url())];
    })
    .await;

    // 每个上游独立计数：chat 接口的成功不影响 claude 接口的 a
    let bearer = format!("Bearer {RELAY_TOKEN}");
    assert_eq!(send_messages(&relay).await.status, 200);
    a.next_request().await;
    b.next_request().await;
    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/chat/completions",
            &[("Host", "relay"), ("Authorization", &bearer)],
            BODY,
        ),
    )
    .await;
    assert_eq!(resp.status, 200);
    good.next_request().await;
    assert_eq!(send_messages(&relay).await.status, 200);
    a.next_request().await;
    b.next_request().await;
    // a 连续失败 2 次，现在被跳过
    assert_eq!(send_messages(&relay).await.status, 200);
    b.next_request().await;
    a.assert_no_request(Duration::from_millis(200)).await;
}
