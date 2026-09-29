//! 单上游透传集成测试（设计文档 §11：T1–T5、T9 鉴权部分、T13）。

mod support;

use std::time::{Duration, Instant};

use cc_proxy_core::config::RelayConfig;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// body 里放转义字符、非 ASCII、多余空白：任何重新序列化都会改变这些字节
const BODY: &[u8] =
    b"{\"model\":\"m\",  \"text\":\"h\xc3\xa9llo \\u0000 \\\"q\\\"\",\"n\":1.50}\n  ";

fn ok_json() -> Vec<Segment> {
    vec![Segment::now(raw_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        b"{\"ok\":true}",
    ))]
}

async fn relay_with(configure: impl FnOnce(&mut RelayConfig)) -> RunningRelay {
    let mut config = base_config();
    configure(&mut config);
    start_relay(config).await
}

// ---------- T1：四个接口的 method / path / query / body 逐字节不变 ----------

#[tokio::test]
async fn t1_claude_messages_passthrough() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/messages?beta=true",
            &[("Host", "relay"), ("x-api-key", RELAY_TOKEN)],
            BODY,
        ),
    )
    .await;
    assert_eq!(resp.status, 200);

    let got = up.next_request().await;
    assert_eq!(got.method, "POST");
    assert_eq!(got.target, "/v1/messages?beta=true");
    assert_eq!(got.body, BODY);
}

#[tokio::test]
async fn t1_openai_chat_passthrough() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.openai_chat = vec![upstream("a", &up.base_url())]).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/chat/completions",
            &[
                ("Host", "relay"),
                ("Authorization", &format!("Bearer {RELAY_TOKEN}")),
            ],
            BODY,
        ),
    )
    .await;
    assert_eq!(resp.status, 200);

    let got = up.next_request().await;
    assert_eq!(
        (got.method.as_str(), got.target.as_str()),
        ("POST", "/v1/chat/completions")
    );
    assert_eq!(got.body, BODY);
}

#[tokio::test]
async fn t1_openai_responses_passthrough() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay =
        relay_with(|c| c.upstreams.openai_responses = vec![upstream("a", &up.base_url())]).await;

    let resp = request(
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
    .await;
    assert_eq!(resp.status, 200);

    let got = up.next_request().await;
    assert_eq!(
        (got.method.as_str(), got.target.as_str()),
        ("POST", "/v1/responses")
    );
    assert_eq!(got.body, BODY);
}

#[tokio::test]
async fn t1_gemini_passthrough_removes_only_key_param() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.gemini = vec![upstream("a", &up.base_url())]).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            &format!(
                "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse&key={RELAY_TOKEN}&x=a%20b+c"
            ),
            &[("Host", "relay")],
            BODY,
        ),
    )
    .await;
    assert_eq!(resp.status, 200);

    let got = up.next_request().await;
    assert_eq!(
        got.target,
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse&x=a%20b+c"
    );
    assert_eq!(got.body, BODY);
    assert_eq!(got.header_str("x-goog-api-key"), Some(UPSTREAM_KEY));
    assert!(find(&got.raw, RELAY_TOKEN.as_bytes()).is_none());
}

// ---------- T2：请求头 ----------

#[tokio::test]
async fn t2_request_headers_follow_passthrough_boundary() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/messages",
            &[
                ("Host", "relay.local:15721"),
                ("User-Agent", "claude-cli/2.1.0"),
                ("X-Api-Key", RELAY_TOKEN),
                ("Anthropic-Version", "2023-06-01"),
                ("anthropic-beta", "one"),
                ("Accept-Encoding", "gzip, br"),
                ("anthropic-beta", "two"),
                ("Proxy-Connection", "keep-alive"),
                ("Keep-Alive", "timeout=5"),
                ("TE", "trailers"),
                ("X-Custom-Header", "v"),
                ("Content-Type", "application/json"),
            ],
            BODY,
        ),
    )
    .await;

    let got = up.next_request().await;
    let names = got.header_names();

    // 头名原始大小写保留
    for name in [
        "User-Agent",
        "Anthropic-Version",
        "Accept-Encoding",
        "X-Custom-Header",
        "Content-Type",
    ] {
        assert!(names.contains(&name), "missing {name} in {names:?}");
    }
    // 不同名头按首次出现位置保持相对顺序；同名头聚合且相对顺序不变
    let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
    assert!(pos("User-Agent") < pos("Anthropic-Version"));
    assert!(pos("Anthropic-Version") < pos("Accept-Encoding"));
    assert!(pos("Accept-Encoding") < pos("X-Custom-Header"));
    assert!(pos("X-Custom-Header") < pos("Content-Type"));
    let betas: Vec<&[u8]> = got
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| v.as_slice())
        .collect();
    assert_eq!(betas, vec![&b"one"[..], b"two"]);

    // host 在原位置替换为上游地址
    assert!(names[0].eq_ignore_ascii_case("host"), "{names:?}");
    assert_eq!(got.header_str("host"), Some(up.addr.to_string().as_str()));

    // 其余值不变
    assert_eq!(got.header_str("accept-encoding"), Some("gzip, br"));
    assert_eq!(got.header_str("user-agent"), Some("claude-cli/2.1.0"));

    // 移除：hop-by-hop、入站凭据；写入上游 Key
    for removed in ["proxy-connection", "keep-alive", "te", "connection"] {
        assert!(!got.has_header(removed), "{removed} should be removed");
    }
    assert_eq!(got.header_str("x-api-key"), Some(UPSTREAM_KEY));
    assert!(!got.has_header("authorization"));
    assert!(find(&got.raw, RELAY_TOKEN.as_bytes()).is_none());
}

#[tokio::test]
async fn t2_header_case_is_lowercased_when_disabled() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| {
        c.server.preserve_header_case = false;
        c.upstreams.claude = vec![upstream("a", &up.base_url())];
    })
    .await;

    request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/messages",
            &[
                ("Host", "relay"),
                ("X-Api-Key", RELAY_TOKEN),
                ("Anthropic-Version", "2023-06-01"),
            ],
            BODY,
        ),
    )
    .await;

    let got = up.next_request().await;
    for name in got.header_names() {
        assert_eq!(
            name,
            name.to_ascii_lowercase(),
            "{name} should be lowercase"
        );
    }
}

// ---------- T3：响应状态码、头、body ----------

#[tokio::test]
async fn t3_response_is_relayed_unchanged() {
    let body = b"{\"id\":\"msg_1\",  \"x\":\"\\u00e9\"}\n";
    let upstream_response = raw_response(
        "201 Created",
        &[
            ("Date", "Mon, 29 Sep 2026 00:00:00 GMT"),
            ("Content-Type", "application/json"),
            ("X-Request-Id", "req_1"),
            ("Set-Cookie", "a=1"),
            ("Set-Cookie", "b=2"),
            ("Retry-After", "3"),
        ],
        body,
    );
    let mut up = MockUpstream::start(vec![Segment::now(upstream_response)]).await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/messages",
            &[("Host", "relay"), ("x-api-key", RELAY_TOKEN)],
            BODY,
        ),
    )
    .await;
    up.next_request().await;

    assert_eq!(resp.status, 201);
    assert_eq!(resp.body, body);
    let names = resp.header_names();
    for name in [
        "Date",
        "Content-Type",
        "X-Request-Id",
        "Set-Cookie",
        "Retry-After",
        "Content-Length",
    ] {
        assert!(names.contains(&name), "missing {name} in {names:?}");
    }
    assert_eq!(
        resp.header_str("date"),
        Some("Mon, 29 Sep 2026 00:00:00 GMT")
    );
    assert_eq!(resp.header_str("x-request-id"), Some("req_1"));
    let cookies: Vec<_> = resp
        .headers
        .iter()
        .filter(|(n, _)| n == "Set-Cookie")
        .map(|(_, v)| v.clone())
        .collect();
    assert_eq!(cookies, vec![b"a=1".to_vec(), b"b=2".to_vec()]);
}

// ---------- T4：SSE 流式，逐段到达、字节不变 ----------

#[tokio::test]
async fn t4_sse_is_streamed_without_buffering() {
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    // 一个事件被拆在两个 chunk 里
    let part1 = b"event: message_start\ndata: {\"type\":\"mess";
    let part2 = b"age_start\"}\n\nevent: ping\ndata: {}\n\n";
    let part3 = b"data: [DONE]\n\n";
    let chunk = |data: &[u8]| {
        let mut out = format!("{:x}\r\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
        out
    };
    let mut first = head.to_vec();
    first.extend(chunk(part1));
    let mut last = chunk(part3);
    last.extend_from_slice(b"0\r\n\r\n");
    let mut up = MockUpstream::start(vec![
        Segment::now(first),
        Segment::after(Duration::from_millis(400), chunk(part2)),
        Segment::after(Duration::from_millis(100), last),
    ])
    .await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    let mut stream = TcpStream::connect(relay.addr).await.unwrap();
    let started = Instant::now();
    stream
        .write_all(&raw_request(
            "POST",
            "/v1/messages",
            &[
                ("Host", "relay"),
                ("x-api-key", RELAY_TOKEN),
                ("Accept", "text/event-stream"),
            ],
            BODY,
        ))
        .await
        .unwrap();

    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let first_data_at = loop {
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed before first event");
        raw.extend_from_slice(&buf[..n]);
        if find(&raw, part1).is_some() {
            break started.elapsed();
        }
    };
    stream.read_to_end(&mut raw).await.unwrap();
    up.next_request().await;

    assert!(
        first_data_at < Duration::from_millis(350),
        "first event arrived after {first_data_at:?}; response was buffered"
    );
    let resp = parse_response(&raw, false);
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header_str("content-type"), Some("text/event-stream"));
    let mut expected = part1.to_vec();
    expected.extend_from_slice(part2);
    expected.extend_from_slice(part3);
    assert_eq!(resp.body, expected);
}

// ---------- T5：压缩响应不解压 ----------

#[tokio::test]
async fn t5_compressed_body_is_not_decoded() {
    let gzip_bytes: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xab, 0xae, 0x05, 0x00, 0x43,
        0xbf, 0xa6, 0xa3, 0x02, 0x00, 0x00, 0x00,
    ];
    let mut up = MockUpstream::start(vec![Segment::now(raw_response(
        "200 OK",
        &[
            ("Content-Type", "application/json"),
            ("Content-Encoding", "gzip"),
        ],
        gzip_bytes,
    ))])
    .await;
    let relay =
        relay_with(|c| c.upstreams.openai_responses = vec![upstream("a", &up.base_url())]).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/responses",
            &[
                ("Host", "relay"),
                ("Authorization", &format!("Bearer {RELAY_TOKEN}")),
                ("Accept-Encoding", "gzip, deflate, br, zstd"),
            ],
            BODY,
        ),
    )
    .await;
    let got = up.next_request().await;

    assert_eq!(
        got.header_str("accept-encoding"),
        Some("gzip, deflate, br, zstd")
    );
    assert_eq!(resp.header_str("content-encoding"), Some("gzip"));
    assert_eq!(resp.body, gzip_bytes);
}

// ---------- T9（鉴权部分） ----------

#[tokio::test]
async fn t9_missing_or_wrong_token_is_rejected_before_upstream() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    for headers in [
        vec![("Host", "relay")],
        vec![("Host", "relay"), ("x-api-key", "wrong")],
        vec![
            ("Host", "relay"),
            ("Authorization", "Basic cmVsYXktdG9rZW4="),
        ],
    ] {
        let resp = request(
            relay.addr,
            &raw_request("POST", "/v1/messages", &headers, BODY),
        )
        .await;
        assert_eq!(resp.status, 401, "{headers:?}");
        assert_eq!(resp.json()["error"]["type"], "relay_unauthorized");
    }
    up.assert_no_request(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn t9_any_matching_carrier_passes_and_none_is_forwarded() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/messages",
            &[
                ("Host", "relay"),
                ("x-api-key", RELAY_TOKEN),
                ("Authorization", "Bearer wrong"),
                ("x-goog-api-key", "also-wrong"),
            ],
            BODY,
        ),
    )
    .await;
    assert_eq!(resp.status, 200);

    let got = up.next_request().await;
    assert_eq!(got.header_str("x-api-key"), Some(UPSTREAM_KEY));
    assert!(!got.has_header("authorization"));
    assert!(!got.has_header("x-goog-api-key"));
    assert!(find(&got.raw, b"wrong").is_none());
}

#[tokio::test]
async fn t9_anonymous_mode_forwards_without_token() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| {
        c.server.auth_tokens.clear();
        c.server.allow_anonymous = true;
        c.upstreams.claude = vec![upstream("a", &up.base_url())];
    })
    .await;

    let resp = request(
        relay.addr,
        &raw_request("POST", "/v1/messages", &[("Host", "relay")], BODY),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        up.next_request().await.header_str("x-api-key"),
        Some(UPSTREAM_KEY)
    );
}

// ---------- T13：边界 ----------

#[tokio::test]
async fn t13_chunked_upload_becomes_content_length() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    let mut raw = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: relay\r\nx-api-key: {RELAY_TOKEN}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    raw.extend_from_slice(b"5\r\n{\"a\":\r\n3\r\n12}\r\n0\r\n\r\n");
    let resp = request(relay.addr, &raw).await;
    assert_eq!(resp.status, 200);

    let got = up.next_request().await;
    assert_eq!(got.body, b"{\"a\":12}");
    assert_eq!(got.header_str("content-length"), Some("8"));
    assert!(!got.has_header("transfer-encoding"));
}

#[tokio::test]
async fn t13_expect_continue_is_answered_by_relay_and_not_forwarded() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    let mut stream = TcpStream::connect(relay.addr).await.unwrap();
    stream
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nHost: relay\r\nx-api-key: {RELAY_TOKEN}\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                BODY.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(
        buf[..n].starts_with(b"HTTP/1.1 100 Continue"),
        "{:?}",
        String::from_utf8_lossy(&buf[..n])
    );
    stream.write_all(BODY).await.unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    assert_eq!(parse_response(&rest, false).status, 200);

    let got = up.next_request().await;
    assert!(!got.has_header("expect"));
    assert_eq!(got.body, BODY);
}

#[tokio::test]
async fn t13_head_and_no_content_have_no_body() {
    let mut head_up = MockUpstream::start(vec![Segment::now(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n".to_vec(),
    )])
    .await;
    let relay =
        relay_with(|c| c.upstreams.openai_responses = vec![upstream("a", &head_up.base_url())])
            .await;
    let bearer = format!("Bearer {RELAY_TOKEN}");

    let resp = request(
        relay.addr,
        &raw_request(
            "HEAD",
            "/v1/responses/resp_1",
            &[("Host", "relay"), ("Authorization", &bearer)],
            b"",
        ),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header_str("content-length"), Some("11"));
    assert!(resp.body.is_empty());
    assert_eq!(head_up.next_request().await.method, "HEAD");

    let mut delete_up = MockUpstream::start(vec![Segment::now(
        b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec(),
    )])
    .await;
    let relay =
        relay_with(|c| c.upstreams.openai_responses = vec![upstream("a", &delete_up.base_url())])
            .await;
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
    assert!(resp.body.is_empty());
    assert_eq!(delete_up.next_request().await.method, "DELETE");
}

#[tokio::test]
async fn t13_response_trailers_are_dropped() {
    let mut up = MockUpstream::start(vec![Segment::now(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTrailer: X-Checksum\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\nX-Checksum: abc\r\n\r\n".to_vec(),
    )])
    .await;
    let relay = relay_with(|c| c.upstreams.claude = vec![upstream("a", &up.base_url())]).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/messages",
            &[("Host", "relay"), ("x-api-key", RELAY_TOKEN)],
            BODY,
        ),
    )
    .await;
    up.next_request().await;
    assert_eq!(resp.body, b"hello");
    assert!(find(&resp.raw, b"X-Checksum").is_none());
    assert!(find(&resp.raw, b"abc").is_none());
}

#[tokio::test]
async fn t13_models_list_is_routed_by_anthropic_version() {
    let mut claude_up = MockUpstream::start(ok_json()).await;
    let mut openai_up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| {
        c.upstreams.claude = vec![upstream("claude", &claude_up.base_url())];
        c.upstreams.openai_responses = vec![upstream("openai", &openai_up.base_url())];
    })
    .await;

    let resp = request(
        relay.addr,
        &raw_request(
            "GET",
            "/v1/models?limit=1000",
            &[
                ("Host", "relay"),
                ("x-api-key", RELAY_TOKEN),
                ("anthropic-version", "2023-06-01"),
            ],
            b"",
        ),
    )
    .await;
    assert_eq!(resp.status, 200);
    let got = claude_up.next_request().await;
    assert_eq!(got.target, "/v1/models?limit=1000");
    assert_eq!(got.header_str("x-api-key"), Some(UPSTREAM_KEY));

    let resp = request(
        relay.addr,
        &raw_request(
            "GET",
            "/v1/models",
            &[
                ("Host", "relay"),
                ("Authorization", &format!("Bearer {RELAY_TOKEN}")),
            ],
            b"",
        ),
    )
    .await;
    assert_eq!(resp.status, 200);
    let got = openai_up.next_request().await;
    assert_eq!(got.target, "/v1/models");
    assert_eq!(
        got.header_str("authorization"),
        Some(format!("Bearer {UPSTREAM_KEY}").as_str())
    );
}

#[tokio::test]
async fn t13_gemini_ga_action_path_goes_to_gemini() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| c.upstreams.gemini = vec![upstream("g", &up.base_url())]).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/models/gemini-2.5-flash:generateContent",
            &[("Host", "relay"), ("x-goog-api-key", RELAY_TOKEN)],
            BODY,
        ),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        up.next_request().await.target,
        "/v1/models/gemini-2.5-flash:generateContent"
    );
}

// ---------- URL 拼接端到端（T16 中依赖网络的部分） ----------

#[tokio::test]
async fn base_path_and_strip_prefix_are_applied() {
    let mut up = MockUpstream::start(ok_json()).await;
    let relay = relay_with(|c| {
        let mut u = upstream("ark", &format!("{}/api/v3?api-version=1", up.base_url()));
        u.strip_prefix = Some("/v1".to_string());
        c.upstreams.openai_chat = vec![u];
    })
    .await;

    request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/chat/completions?x=1",
            &[
                ("Host", "relay"),
                ("Authorization", &format!("Bearer {RELAY_TOKEN}")),
            ],
            BODY,
        ),
    )
    .await;
    assert_eq!(
        up.next_request().await.target,
        "/api/v3/chat/completions?api-version=1&x=1"
    );
}

// ---------- 中转自身的错误响应 ----------

#[tokio::test]
async fn relay_errors() {
    let mut up = MockUpstream::start(ok_json()).await;
    let closed_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let relay = relay_with(|c| {
        c.server.max_body_bytes = 16;
        c.upstreams.claude = vec![upstream("a", &up.base_url())];
        c.upstreams.openai_chat =
            vec![upstream("dead", &format!("http://127.0.0.1:{closed_port}"))];
    })
    .await;
    let bearer = format!("Bearer {RELAY_TOKEN}");

    let cases: Vec<(Vec<u8>, u16, &str)> = vec![
        (
            raw_request(
                "POST",
                "/v1/embeddings",
                &[("Host", "r"), ("Authorization", &bearer)],
                b"{}",
            ),
            404,
            "relay_not_found",
        ),
        (
            raw_request(
                "GET",
                "/",
                &[("Host", "r"), ("Authorization", &bearer)],
                b"",
            ),
            404,
            "relay_not_found",
        ),
        (
            raw_request(
                "GET",
                "/_relay/unknown",
                &[("Host", "r"), ("Authorization", &bearer)],
                b"",
            ),
            404,
            "relay_not_found",
        ),
        (
            raw_request(
                "POST",
                "/v1/responses",
                &[("Host", "r"), ("Authorization", &bearer)],
                b"{}",
            ),
            503,
            "relay_no_upstream",
        ),
        (
            raw_request(
                "POST",
                "/v1/messages",
                &[("Host", "r"), ("x-api-key", RELAY_TOKEN)],
                &[b'x'; 64],
            ),
            413,
            "relay_payload_too_large",
        ),
        (
            raw_request(
                "POST",
                "/v1/chat/completions",
                &[("Host", "r"), ("Authorization", &bearer)],
                b"{}",
            ),
            502,
            "relay_bad_gateway",
        ),
    ];
    for (raw, status, kind) in cases {
        let resp = request(relay.addr, &raw).await;
        assert_eq!(resp.status, status, "{}", String::from_utf8_lossy(&raw));
        assert_eq!(resp.json()["error"]["type"], kind);
        assert_eq!(resp.header_str("content-type"), Some("application/json"));
    }
    up.assert_no_request(Duration::from_millis(200)).await;
}
