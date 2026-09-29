//! 优雅退出（设计文档 §9）：停止接受新连接，进行中的请求处理完，空闲连接立即关闭。

mod support;

use std::time::Duration;

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";

fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

#[tokio::test]
async fn in_flight_stream_finishes_before_serve_returns() {
    let mut first = SSE_HEAD.to_vec();
    first.extend(chunk(b"data: 1\n\n"));
    let mut rest = chunk(b"data: 2\n\n");
    rest.extend_from_slice(b"0\r\n\r\n");
    let mut up = MockUpstream::start(vec![
        Segment::now(first),
        Segment::after(Duration::from_millis(500), rest),
    ])
    .await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let (addr, stop, handle) = spawn_relay(config).await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&raw_request(
            "POST",
            "/v1/messages",
            &[("Host", "relay"), ("x-api-key", RELAY_TOKEN)],
            b"{}",
        ))
        .await
        .unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 512];
    while find(&raw, b"data: 1").is_none() {
        let n = client.read(&mut buf).await.unwrap();
        assert!(n > 0);
        raw.extend_from_slice(&buf[..n]);
    }
    up.next_request().await;

    stop.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !handle.is_finished(),
        "serve must wait for the in-flight stream"
    );
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "no new connections after shutdown"
    );

    client.read_to_end(&mut raw).await.unwrap();
    let resp = parse_response(&raw, false);
    assert_eq!(resp.body, b"data: 1\n\ndata: 2\n\n");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("serve must return after the stream ends")
        .unwrap();
}

#[tokio::test]
async fn idle_connections_do_not_block_shutdown() {
    let mut up = MockUpstream::start(vec![Segment::now(raw_response("200 OK", &[], b"{}"))]).await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let (addr, stop, handle) = spawn_relay(config).await;

    // 连上但从未发送请求
    let _silent = TcpStream::connect(addr).await.unwrap();
    // 完成一个 keep-alive 请求后保持连接
    let mut kept = TcpStream::connect(addr).await.unwrap();
    kept.write_all(
        format!(
            "POST /v1/messages HTTP/1.1\r\nHost: relay\r\nx-api-key: {RELAY_TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 1024];
    let n = kept.read(&mut buf).await.unwrap();
    assert!(buf[..n].starts_with(b"HTTP/1.1 200"));
    up.next_request().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("idle connections must be closed on shutdown")
        .unwrap();
}
