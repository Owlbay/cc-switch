//! 协议转换钩子（`docs/protocol-conversion-design-zh.md` §8、§10 X7、X8）。
//!
//! 用测试专用转换器验证 core 的管道：透传上游完全不经过转换器；转换上游的请求、
//! 流式 / 非流式 / 非 2xx 响应、故障转移、Unsupported 跳过与各类错误。

mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use cc_proxy_core::config::{RelayConfig, UpstreamConfig};
use cc_proxy_core::convert::{
    map_model, ConversionContext, ConvertError, Converter, InboundRequest, OutboundMeta,
    OutboundRequest, ResponseConverter,
};
use cc_proxy_core::upstream::BuildError;
use cc_proxy_core::{serve, Interface, Relay};
use http::{HeaderMap, HeaderValue, StatusCode};
use support::*;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// ---------- 测试转换器 ----------

/// 任何方法被调用即 panic：证明透传上游不经过转换器
struct PanicConverter;

impl Converter for PanicConverter {
    fn supports(&self, _: Interface, _: Interface) -> bool {
        true
    }
    fn convert_request(
        &self,
        _: &ConversionContext<'_>,
        _: &InboundRequest<'_>,
    ) -> Result<OutboundRequest, ConvertError> {
        panic!("passthrough upstream must not reach the converter");
    }
    fn response_converter(
        &self,
        _: &ConversionContext<'_>,
        _: &OutboundMeta,
    ) -> Box<dyn ResponseConverter> {
        panic!("passthrough upstream must not reach the converter");
    }
}

/// 可观察的标记转换：
/// - 请求：path 改为 `/converted`、query `from=<client>&to=<upstream>`，body 前加 `CONVERTED:`，
///   加 `x-converted` 头、`accept-encoding` 保留（验证 core 兜底去掉），模型名经 model_map；
///   body 含 `UNSUPPORTED` / `INTERNAL` 时返回对应错误；带 `x-test-stream` 头时按流式处理
/// - 响应：流式每块转大写，结束追加 `|END`，出错追加 `|ERR`；非流式为 `FULL:<大写>`
struct MarkConverter;

impl Converter for MarkConverter {
    fn supports(&self, client: Interface, _: Interface) -> bool {
        client == Interface::Claude || client == Interface::OpenaiResponses
    }

    fn convert_request(
        &self,
        ctx: &ConversionContext<'_>,
        request: &InboundRequest<'_>,
    ) -> Result<OutboundRequest, ConvertError> {
        let text = String::from_utf8_lossy(request.body);
        if text.contains("UNSUPPORTED") {
            return Err(ConvertError::Unsupported(
                "server tool cannot be expressed".into(),
            ));
        }
        if text.contains("INTERNAL") {
            return Err(ConvertError::Internal("boom".into()));
        }
        let mut headers = request.headers.clone();
        headers.insert(
            "x-converted",
            HeaderValue::from_str(&format!(
                "{}->{}",
                ctx.client.as_str(),
                ctx.upstream.as_str()
            ))
            .unwrap(),
        );
        headers.insert(
            "x-mapped-model",
            HeaderValue::from_str(map_model(ctx.model_map, "client-model")).unwrap(),
        );
        headers.insert(
            "x-default-max",
            HeaderValue::from_str(&ctx.default_max_output_tokens.to_string()).unwrap(),
        );
        let mut body = b"CONVERTED:".to_vec();
        body.extend_from_slice(request.body);
        Ok(OutboundRequest {
            method: http::Method::POST,
            path: "/converted".into(),
            query: Some(format!(
                "from={}&to={}",
                ctx.client.as_str(),
                ctx.upstream.as_str()
            )),
            headers,
            body: Bytes::from(body),
            meta: OutboundMeta {
                stream: request.headers.contains_key("x-test-stream"),
                ..Default::default()
            },
        })
    }

    fn response_converter(
        &self,
        _: &ConversionContext<'_>,
        _: &OutboundMeta,
    ) -> Box<dyn ResponseConverter> {
        Box::new(MarkResponse)
    }
}

struct MarkResponse;

impl ResponseConverter for MarkResponse {
    fn convert_head(&mut self, _: StatusCode, headers: &HeaderMap) -> HeaderMap {
        let mut out = headers.clone();
        out.insert("x-converted-response", HeaderValue::from_static("1"));
        out
    }
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ConvertError> {
        Ok(vec![Bytes::from(chunk.to_ascii_uppercase())])
    }
    fn finish(&mut self, error: Option<&(dyn std::error::Error + 'static)>) -> Vec<Bytes> {
        vec![Bytes::from_static(if error.is_some() {
            b"|ERR"
        } else {
            b"|END"
        })]
    }
    fn convert_full(&mut self, _: StatusCode, body: &[u8]) -> Result<Bytes, ConvertError> {
        let mut out = b"FULL:".to_vec();
        out.extend(body.to_ascii_uppercase());
        Ok(Bytes::from(out))
    }
}

// ---------- 基建 ----------

async fn start_with(config: RelayConfig, converter: Arc<dyn Converter>) -> RunningRelayAddr {
    let relay = Arc::new(Relay::with_converter(&config, converter).expect("relay config"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve(listener, relay, async {
        let _ = rx.await;
    }));
    RunningRelayAddr { addr, _stop: tx }
}

struct RunningRelayAddr {
    addr: std::net::SocketAddr,
    _stop: oneshot::Sender<()>,
}

fn converting(id: &str, base_url: &str, protocol: Interface) -> UpstreamConfig {
    let mut u = upstream(id, base_url);
    u.protocol = Some(protocol);
    u
}

fn ok_json(body: &str) -> Vec<Segment> {
    vec![Segment::now(raw_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    ))]
}

fn claude_request(body: &[u8], extra: &[(&str, &str)]) -> Vec<u8> {
    let mut headers = vec![
        ("Host", "relay"),
        ("x-api-key", RELAY_TOKEN),
        ("Accept-Encoding", "gzip, br"),
    ];
    headers.extend_from_slice(extra);
    raw_request("POST", "/v1/messages?beta=true", &headers, body)
}

// ---------- X8：透传不经过转换器 ----------

#[tokio::test]
async fn passthrough_upstreams_never_reach_the_converter() {
    let mut claude = MockUpstream::start(ok_json("{\"a\":1}")).await;
    let sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\ndata: \r\n7\r\nhello\n\n\r\n0\r\n\r\n".to_vec();
    let mut chat = MockUpstream::start(vec![Segment::now(sse)]).await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("c", &claude.base_url())];
    config.upstreams.openai_chat = vec![upstream("o", &chat.base_url())];
    let relay = start_with(config, Arc::new(PanicConverter)).await;

    let body = b"{\"model\":\"m\",  \"x\":\"\\u00e9\"}\n";
    let resp = request(relay.addr, &claude_request(body, &[])).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"{\"a\":1}");
    let got = claude.next_request().await;
    assert_eq!(got.target, "/v1/messages?beta=true");
    assert_eq!(got.body, body);
    assert_eq!(got.header_str("accept-encoding"), Some("gzip, br"));

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/chat/completions",
            &[
                ("Host", "relay"),
                ("Authorization", &format!("Bearer {RELAY_TOKEN}")),
            ],
            body,
        ),
    )
    .await;
    assert_eq!(resp.body, b"data: hello\n\n");
    assert_eq!(chat.next_request().await.body, body);
}

// ---------- 转换请求 ----------

#[tokio::test]
async fn converted_request_follows_boundary_rules() {
    let mut up = MockUpstream::start(ok_json("{}")).await;
    let mut config = base_config();
    let mut u = converting(
        "chat",
        &format!("{}/api", up.base_url()),
        Interface::OpenaiChat,
    );
    u.model_map.insert("*".into(), "upstream-model".into());
    u.default_max_output_tokens = Some(20000);
    config.upstreams.claude = vec![u];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(relay.addr, &claude_request(b"{\"q\":1}", &[])).await;
    assert_eq!(resp.status, 200);

    let got = up.next_request().await;
    assert_eq!(got.method, "POST");
    assert_eq!(got.target, "/api/converted?from=claude&to=openai_chat");
    assert_eq!(got.body, b"CONVERTED:{\"q\":1}");
    assert_eq!(got.header_str("x-converted"), Some("claude->openai_chat"));
    assert_eq!(got.header_str("x-mapped-model"), Some("upstream-model"));
    assert_eq!(got.header_str("x-default-max"), Some("20000"));
    // core 兜底：不带 accept-encoding，长度按新 body 计算，鉴权按上游协议（Chat → Bearer）
    assert!(!got.has_header("accept-encoding"));
    assert_eq!(got.header_str("content-length"), Some("17"));
    assert_eq!(
        got.header_str("authorization"),
        Some(format!("Bearer {UPSTREAM_KEY}").as_str())
    );
    assert!(!got.has_header("x-api-key"));
    assert!(find(&got.raw, RELAY_TOKEN.as_bytes()).is_none());
    assert_eq!(got.header_str("host"), Some(up.addr.to_string().as_str()));
}

// ---------- 转换响应 ----------

#[tokio::test]
async fn streaming_response_is_converted_chunk_by_chunk() {
    let sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 9\r\nConnection: close\r\n\r\ndata: ab\n".to_vec();
    let mut up = MockUpstream::start(vec![Segment::now(sse)]).await;
    let mut config = base_config();
    config.upstreams.claude = vec![converting("chat", &up.base_url(), Interface::OpenaiChat)];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(
        relay.addr,
        &claude_request(b"{}", &[("x-test-stream", "1")]),
    )
    .await;
    up.next_request().await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"DATA: AB\n|END");
    assert_eq!(resp.header_str("x-converted-response"), Some("1"));
    // 输出长度已变：不能沿用上游 content-length
    assert!(resp.header("content-length").is_none());
    assert_eq!(resp.header_str("transfer-encoding"), Some("chunked"));
}

#[tokio::test]
async fn buffered_response_is_converted_whole() {
    let mut up = MockUpstream::start(ok_json("{\"ok\":true}")).await;
    let mut config = base_config();
    config.upstreams.openai_responses =
        vec![converting("claude", &up.base_url(), Interface::Claude)];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/responses",
            &[
                ("Host", "relay"),
                ("Authorization", &format!("Bearer {RELAY_TOKEN}")),
            ],
            b"{}",
        ),
    )
    .await;
    let got = up.next_request().await;
    // Claude 上游缺省 x-api-key 鉴权
    assert_eq!(got.header_str("x-api-key"), Some(UPSTREAM_KEY));
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"FULL:{\"OK\":TRUE}");
    assert_eq!(resp.header_str("content-length"), Some("16"));
}

#[tokio::test]
async fn non_retryable_error_is_converted_without_failover() {
    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "400 Bad Request",
        &[("Content-Type", "application/json")],
        b"{\"error\":\"bad\"}",
    ))])
    .await;
    let mut b = MockUpstream::start(ok_json("{}")).await;
    let mut config = base_config();
    config.upstreams.claude = vec![
        converting("a", &a.base_url(), Interface::OpenaiChat),
        upstream("b", &b.base_url()),
    ];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(
        relay.addr,
        &claude_request(b"{}", &[("x-test-stream", "1")]),
    )
    .await;
    a.next_request().await;
    assert_eq!(resp.status, 400);
    // 流式请求的错误同样走缓冲改写
    assert_eq!(resp.body, b"FULL:{\"ERROR\":\"BAD\"}");
    b.assert_no_request(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn last_retryable_error_is_converted() {
    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "503 Service Unavailable",
        &[("Retry-After", "4")],
        b"busy",
    ))])
    .await;
    let mut config = base_config();
    config.upstreams.claude = vec![converting("a", &a.base_url(), Interface::Gemini)];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(relay.addr, &claude_request(b"{}", &[])).await;
    let got = a.next_request().await;
    // Gemini 上游缺省 x-goog-api-key 鉴权
    assert_eq!(got.header_str("x-goog-api-key"), Some(UPSTREAM_KEY));
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body, b"FULL:BUSY");
    assert_eq!(resp.header_str("retry-after"), Some("4"));
}

// ---------- 故障转移（X7） ----------

#[tokio::test]
async fn failover_from_converting_to_passthrough_sends_the_original_request() {
    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "503 Service Unavailable",
        &[],
        b"busy",
    ))])
    .await;
    let mut b = MockUpstream::start(ok_json("{\"from\":\"b\"}")).await;
    let mut config = base_config();
    config.upstreams.claude = vec![
        converting("a", &a.base_url(), Interface::OpenaiChat),
        upstream("b", &b.base_url()),
    ];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(relay.addr, &claude_request(b"{\"q\":1}", &[])).await;
    assert_eq!(a.next_request().await.body, b"CONVERTED:{\"q\":1}");
    let got_b = b.next_request().await;
    assert_eq!(got_b.target, "/v1/messages?beta=true");
    assert_eq!(got_b.body, b"{\"q\":1}");
    assert_eq!(got_b.header_str("accept-encoding"), Some("gzip, br"));
    assert!(!got_b.has_header("x-converted"));
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"{\"from\":\"b\"}");
}

#[tokio::test]
async fn failover_from_passthrough_to_converting_converts() {
    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "503 Service Unavailable",
        &[],
        b"busy",
    ))])
    .await;
    let mut b = MockUpstream::start(ok_json("{\"from\":\"b\"}")).await;
    let mut config = base_config();
    config.upstreams.claude = vec![
        upstream("a", &a.base_url()),
        converting("b", &b.base_url(), Interface::OpenaiChat),
    ];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(relay.addr, &claude_request(b"{\"q\":1}", &[])).await;
    assert_eq!(a.next_request().await.body, b"{\"q\":1}");
    assert_eq!(b.next_request().await.body, b"CONVERTED:{\"q\":1}");
    assert_eq!(resp.body, b"FULL:{\"FROM\":\"B\"}");
}

#[tokio::test]
async fn unsupported_upstream_is_skipped_without_counting_an_attempt() {
    let mut a = MockUpstream::start(ok_json("{}")).await;
    let mut b = MockUpstream::start(ok_json("{\"from\":\"b\"}")).await;
    let mut config = base_config();
    config.failover.max_attempts = Some(1);
    config.upstreams.claude = vec![
        converting("a", &a.base_url(), Interface::OpenaiChat),
        upstream("b", &b.base_url()),
    ];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(
        relay.addr,
        &claude_request(b"{\"tool\":\"UNSUPPORTED\"}", &[]),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"{\"from\":\"b\"}");
    a.assert_no_request(Duration::from_millis(200)).await;
    b.next_request().await;
}

#[tokio::test]
async fn all_unsupported_returns_400_without_contacting_upstreams() {
    let mut a = MockUpstream::start(ok_json("{}")).await;
    let mut config = base_config();
    config.upstreams.claude = vec![converting("a", &a.base_url(), Interface::OpenaiChat)];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(relay.addr, &claude_request(b"UNSUPPORTED", &[])).await;
    assert_eq!(resp.status, 400);
    assert_eq!(resp.json()["error"]["type"], "relay_conversion_unsupported");
    assert!(resp.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("server tool"));
    a.assert_no_request(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn internal_conversion_error_is_500() {
    let mut a = MockUpstream::start(ok_json("{}")).await;
    let mut b = MockUpstream::start(ok_json("{}")).await;
    let mut config = base_config();
    config.upstreams.claude = vec![
        converting("a", &a.base_url(), Interface::OpenaiChat),
        upstream("b", &b.base_url()),
    ];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(relay.addr, &claude_request(b"INTERNAL", &[])).await;
    assert_eq!(resp.status, 500);
    assert_eq!(resp.json()["error"]["type"], "relay_conversion_error");
    a.assert_no_request(Duration::from_millis(100)).await;
    b.assert_no_request(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn compressed_upstream_response_is_a_conversion_error() {
    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "200 OK",
        &[("Content-Encoding", "gzip")],
        &[0x1f, 0x8b, 0x08, 0x00],
    ))])
    .await;
    let mut b = MockUpstream::start(ok_json("{\"from\":\"b\"}")).await;
    let mut config = base_config();
    config.breaker.failure_threshold = 1;
    config.upstreams.claude = vec![
        converting("a", &a.base_url(), Interface::OpenaiChat),
        upstream("b", &b.base_url()),
    ];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(relay.addr, &claude_request(b"{}", &[])).await;
    a.next_request().await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.json()["error"]["type"], "relay_conversion_error");
    b.assert_no_request(Duration::from_millis(200)).await;

    // 不计熔断：阈值为 1，a 仍然可用
    request(relay.addr, &claude_request(b"{}", &[])).await;
    a.next_request().await;
}

#[tokio::test]
async fn stream_broken_mid_way_emits_error_frame_and_cuts_client() {
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n".to_vec();
    let mut up = MockUpstream::start(vec![Segment::now(head)]).await;
    let mut config = base_config();
    config.upstreams.claude = vec![converting("a", &up.base_url(), Interface::OpenaiChat)];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let raw = send_raw(
        relay.addr,
        &claude_request(b"{}", &[("x-test-stream", "1")]),
    )
    .await;
    up.next_request().await;
    let hello = find(&raw, b"HELLO").expect("converted data");
    let err = find(&raw, b"|ERR").unwrap_or_else(|| panic!("{}", String::from_utf8_lossy(&raw)));
    assert!(
        hello < err,
        "error frame must follow the data already relayed"
    );
    assert!(
        find(&raw, b"0\r\n\r\n").is_none(),
        "client connection must be cut"
    );
}

// ---------- 构造期校验 ----------

#[test]
fn relay_new_rejects_converting_upstreams() {
    let mut config = base_config();
    config.upstreams.claude = vec![converting(
        "a",
        "https://example.com",
        Interface::OpenaiChat,
    )];
    let error = Relay::new(&config).err().expect("must be rejected");
    assert!(matches!(error, BuildError::Config(_)));
    assert!(error.to_string().contains("未启用转换器"), "{error}");

    let mut config = base_config();
    let mut mapped = upstream("m", "https://example.com");
    mapped.model_map.insert("a".into(), "b".into());
    config.upstreams.claude = vec![mapped];
    assert!(
        Relay::new(&config).is_err(),
        "model_map needs a converter too"
    );
}

#[test]
fn with_converter_checks_supported_directions() {
    struct ClaudeOnly;
    impl Converter for ClaudeOnly {
        fn supports(&self, _: Interface, upstream: Interface) -> bool {
            upstream == Interface::OpenaiChat
        }
        fn convert_request(
            &self,
            _: &ConversionContext<'_>,
            _: &InboundRequest<'_>,
        ) -> Result<OutboundRequest, ConvertError> {
            unreachable!()
        }
        fn response_converter(
            &self,
            _: &ConversionContext<'_>,
            _: &OutboundMeta,
        ) -> Box<dyn ResponseConverter> {
            unreachable!()
        }
    }
    let mut config = base_config();
    config.upstreams.claude = vec![converting(
        "a",
        "https://example.com",
        Interface::OpenaiChat,
    )];
    assert!(Relay::with_converter(&config, Arc::new(ClaudeOnly)).is_ok());

    config.upstreams.claude = vec![converting("g", "https://example.com", Interface::Gemini)];
    let error = Relay::with_converter(&config, Arc::new(ClaudeOnly))
        .err()
        .expect("unsupported direction");
    assert!(error.to_string().contains("claude -> gemini"), "{error}");
}

// ---------- C1 复查补充 ----------

/// 统计 convert_request 调用次数
struct CountingConverter(Arc<std::sync::atomic::AtomicUsize>);

impl Converter for CountingConverter {
    fn supports(&self, client: Interface, upstream: Interface) -> bool {
        MarkConverter.supports(client, upstream)
    }
    fn convert_request(
        &self,
        ctx: &ConversionContext<'_>,
        request: &InboundRequest<'_>,
    ) -> Result<OutboundRequest, ConvertError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        MarkConverter.convert_request(ctx, request)
    }
    fn response_converter(
        &self,
        ctx: &ConversionContext<'_>,
        meta: &OutboundMeta,
    ) -> Box<dyn ResponseConverter> {
        MarkConverter.response_converter(ctx, meta)
    }
}

#[tokio::test]
async fn conversion_is_reused_across_attempts_with_the_same_target() {
    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "503 Service Unavailable",
        &[],
        b"busy",
    ))])
    .await;
    let mut b = MockUpstream::start(ok_json("{}")).await;
    let mut c = MockUpstream::start(ok_json("{}")).await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut config = base_config();
    let mut mapped = converting("c", &c.base_url(), Interface::OpenaiChat);
    mapped.model_map.insert("*".into(), "other".into());
    config.upstreams.claude = vec![
        converting("a", &a.base_url(), Interface::OpenaiChat),
        converting("b", &b.base_url(), Interface::OpenaiChat),
        mapped,
    ];
    let relay = start_with(config, Arc::new(CountingConverter(Arc::clone(&calls)))).await;

    request(relay.addr, &claude_request(b"{}", &[])).await;
    a.next_request().await;
    b.next_request().await;
    c.assert_no_request(Duration::from_millis(100)).await;
    // a、b 同协议同 model_map：只转换一次
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unsupported_skip_returns_the_half_open_probe() {
    let mut a = MockUpstream::start(vec![Segment::now(raw_response(
        "503 Service Unavailable",
        &[],
        b"busy",
    ))])
    .await;
    let mut b = MockUpstream::start(ok_json("{\"from\":\"b\"}")).await;
    let mut config = base_config();
    config.breaker.failure_threshold = 1;
    config.breaker.open_secs = 1;
    config.upstreams.claude = vec![
        converting("a", &a.base_url(), Interface::OpenaiChat),
        upstream("b", &b.base_url()),
    ];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    // a 失败一次 → 打开
    request(relay.addr, &claude_request(b"{}", &[])).await;
    a.next_request().await;
    b.next_request().await;
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // 半开：Unsupported 跳过 a 并归还探测名额
    let resp = request(relay.addr, &claude_request(b"UNSUPPORTED", &[])).await;
    assert_eq!(resp.status, 200);
    b.next_request().await;
    a.assert_no_request(Duration::from_millis(100)).await;

    // 探测名额仍可用：下一个正常请求探测 a
    request(relay.addr, &claude_request(b"{}", &[])).await;
    a.next_request().await;
}

#[tokio::test]
async fn buffered_bodies_over_their_limits_are_conversion_errors() {
    let big = "x".repeat(4096);
    let mut ok_up = MockUpstream::start(ok_json(&big)).await;
    let mut err_up = MockUpstream::start(vec![Segment::now(raw_response(
        "400 Bad Request",
        &[],
        big.as_bytes(),
    ))])
    .await;
    let mut config = base_config();
    config.server.max_body_bytes = 1024;
    config.failover.retry_body_bytes = 1024;
    config.upstreams.claude = vec![converting("ok", &ok_up.base_url(), Interface::OpenaiChat)];
    config.upstreams.openai_responses =
        vec![converting("err", &err_up.base_url(), Interface::Claude)];
    let relay = start_with(config, Arc::new(MarkConverter)).await;

    let resp = request(relay.addr, &claude_request(b"{}", &[])).await;
    ok_up.next_request().await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.json()["error"]["type"], "relay_conversion_error");

    let resp = request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/responses",
            &[
                ("Host", "relay"),
                ("Authorization", &format!("Bearer {RELAY_TOKEN}")),
            ],
            b"{}",
        ),
    )
    .await;
    err_up.next_request().await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.json()["error"]["type"], "relay_conversion_error");
}

#[tokio::test]
async fn passthrough_edge_cases_never_reach_the_converter() {
    let trailer = b"HTTP/1.1 200 OK\r\nTrailer: X-Checksum\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\nX-Checksum: abc\r\n\r\n".to_vec();
    let mut up = MockUpstream::start(vec![Segment::now(trailer)]).await;
    let mut config = base_config();
    config.upstreams.claude = vec![upstream("c", &up.base_url())];
    let relay = start_with(config, Arc::new(PanicConverter)).await;

    let mut raw = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: relay\r\nx-api-key: {RELAY_TOKEN}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    raw.extend_from_slice(b"5\r\n{\"a\":\r\n3\r\n12}\r\n0\r\n\r\n");
    let resp = request(relay.addr, &raw).await;
    assert_eq!(resp.body, b"hello");
    assert!(find(&resp.raw, b"X-Checksum").is_none());
    assert_eq!(up.next_request().await.body, b"{\"a\":12}");
}
