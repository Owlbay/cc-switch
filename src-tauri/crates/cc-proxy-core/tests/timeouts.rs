//! 空闲超时集成测试（设计文档 §5.4）。响应头超时见 `failover.rs`。

mod support;

use std::time::{Duration, Instant};

use support::*;

const BODY: &[u8] = b"{\"model\":\"m\",\"stream\":true}";

fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

const SSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";

fn claude_request() -> Vec<u8> {
    raw_request(
        "POST",
        "/v1/messages",
        &[("Host", "relay"), ("x-api-key", RELAY_TOKEN)],
        BODY,
    )
}

#[tokio::test]
async fn stalled_stream_is_cut_after_idle_timeout() {
    let mut first = SSE_HEAD.to_vec();
    first.extend(chunk(b"data: {\"n\":1}\n\n"));
    let mut rest = chunk(b"data: {\"n\":2}\n\n");
    rest.extend_from_slice(b"0\r\n\r\n");
    let mut up = MockUpstream::start(vec![
        Segment::now(first),
        Segment::after(Duration::from_secs(10), rest),
    ])
    .await;
    let mut config = base_config();
    config.timeouts.idle_secs = 1;
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    let started = Instant::now();
    let raw = send_raw(relay.addr, &claude_request()).await;
    let elapsed = started.elapsed();
    up.next_request().await;

    assert!(
        find(&raw, b"data: {\"n\":1}").is_some(),
        "first event must arrive"
    );
    assert!(find(&raw, b"data: {\"n\":2}").is_none());
    // 连接被中断：没有 chunked 结束标记，客户端能识别出响应不完整
    assert!(find(&raw, b"0\r\n\r\n").is_none());
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5),
        "{elapsed:?}"
    );
}

#[tokio::test]
async fn steady_stream_is_not_cut() {
    let mut segments = vec![Segment::now(SSE_HEAD.to_vec())];
    for n in 1..=3 {
        segments.push(Segment::after(
            Duration::from_millis(600),
            chunk(format!("data: {{\"n\":{n}}}\n\n").as_bytes()),
        ));
    }
    segments.push(Segment::now(b"0\r\n\r\n".to_vec()));
    let mut up = MockUpstream::start(segments).await;
    let mut config = base_config();
    config.timeouts.idle_secs = 1;
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    let resp = request(relay.addr, &claude_request()).await;
    up.next_request().await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.body,
        b"data: {\"n\":1}\n\ndata: {\"n\":2}\n\ndata: {\"n\":3}\n\n"
    );
}

#[tokio::test]
async fn stalled_error_body_counts_as_a_failure() {
    let mut a = MockUpstream::start(vec![
        Segment::now(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial"
                .to_vec(),
        ),
        Segment::after(Duration::from_secs(10), vec![b'x'; 93]),
    ])
    .await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let mut config = base_config();
    config.timeouts.idle_secs = 1;
    config.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("dead", &dead)];
    let relay = start_relay(config).await;

    let started = Instant::now();
    let resp = request(relay.addr, &claude_request()).await;
    let elapsed = started.elapsed();
    a.next_request().await;

    assert_eq!(resp.status, 502);
    assert_eq!(resp.json()["error"]["type"], "relay_bad_gateway");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}
