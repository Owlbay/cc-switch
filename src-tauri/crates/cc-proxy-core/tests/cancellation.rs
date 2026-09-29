//! 取消语义与提交点（设计文档 §5.2、§5.4、§11 T7、T12）。
//!
//! 转发路径不 spawn：客户端断开时 hyper drop 请求 future，上游请求随之取消；
//! 提交点（响应头已交给客户端）之后上游中断只会中断客户端连接，不再重试。

mod support;

use std::time::Duration;

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const BODY: &[u8] = b"{\"model\":\"m\",\"stream\":true}";

const SSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";

fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

fn claude_request() -> Vec<u8> {
    raw_request(
        "POST",
        "/v1/messages",
        &[("Host", "relay"), ("x-api-key", RELAY_TOKEN)],
        BODY,
    )
}

fn ok() -> Vec<Segment> {
    vec![Segment::now(raw_response("200 OK", &[], b"{}"))]
}

// ---------- T12 ----------

#[tokio::test]
async fn t12_client_disconnect_before_head_closes_upstream_and_skips_fallback() {
    let mut a = MockUpstream::start(vec![Segment::after(
        Duration::from_secs(10),
        raw_response("200 OK", &[], b"{}"),
    )])
    .await;
    let mut b = MockUpstream::start(ok()).await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    let relay = start_relay(config).await;

    let mut client = TcpStream::connect(relay.addr).await.unwrap();
    client.write_all(&claude_request()).await.unwrap();
    a.next_request().await;
    drop(client);

    assert!(
        a.closed_within(Duration::from_secs(2)).await,
        "upstream connection must be closed when the client goes away"
    );
    b.assert_no_request(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn t12_client_disconnect_during_upload_never_reaches_upstream() {
    let mut a = MockUpstream::start(ok()).await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &a.base_url())];
    let relay = start_relay(config).await;

    let mut client = TcpStream::connect(relay.addr).await.unwrap();
    client
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nHost: relay\r\nx-api-key: {RELAY_TOKEN}\r\nContent-Length: 100\r\n\r\n{{\"model\":"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(client);

    a.assert_no_request(Duration::from_millis(500)).await;
}

#[tokio::test]
async fn t12_client_disconnect_mid_stream_closes_upstream() {
    let mut first = SSE_HEAD.to_vec();
    first.extend(chunk(b"data: {\"n\":1}\n\n"));
    let mut rest = chunk(b"data: {\"n\":2}\n\n");
    rest.extend_from_slice(b"0\r\n\r\n");
    let mut a = MockUpstream::start(vec![
        Segment::now(first),
        Segment::after(Duration::from_secs(10), rest),
    ])
    .await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &a.base_url())];
    let relay = start_relay(config).await;

    let mut client = TcpStream::connect(relay.addr).await.unwrap();
    client.write_all(&claude_request()).await.unwrap();
    let mut received = Vec::new();
    let mut buf = [0u8; 1024];
    while find(&received, b"data: {\"n\":1}").is_none() {
        let n = client.read(&mut buf).await.unwrap();
        assert!(n > 0);
        received.extend_from_slice(&buf[..n]);
    }
    a.next_request().await;
    drop(client);

    assert!(
        a.closed_within(Duration::from_secs(2)).await,
        "upstream stream must be closed when the client goes away"
    );
}

// ---------- T7 ----------

#[tokio::test]
async fn t7_upstream_failure_after_commit_cuts_client_without_retry() {
    // 发出响应头与第一个事件后直接关闭连接，没有 chunked 结束标记
    let mut first = SSE_HEAD.to_vec();
    first.extend(chunk(b"data: {\"n\":1}\n\n"));
    let mut a = MockUpstream::start(vec![Segment::now(first)]).await;
    let mut b = MockUpstream::start(ok()).await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &a.base_url()), upstream("b", &b.base_url())];
    let relay = start_relay(config).await;

    let raw = send_raw(relay.addr, &claude_request()).await;
    a.next_request().await;

    assert!(raw.starts_with(b"HTTP/1.1 200 OK"));
    assert!(find(&raw, b"data: {\"n\":1}").is_some());
    assert!(
        find(&raw, b"0\r\n\r\n").is_none(),
        "truncated upstream stream must not be completed by the relay"
    );
    b.assert_no_request(Duration::from_millis(300)).await;
}
