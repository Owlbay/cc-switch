//! 端到端：Claude 客户端经中转访问 OpenAI Chat 上游、以及同协议恒等转换
//! （设计文档 §10 X5、X13）。上游与客户端都用原始 TCP 字节。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cc_proxy_convert::IrConverter;
use cc_proxy_core::config::{RelayConfig, Secret, UpstreamConfig};
use cc_proxy_core::{serve, Interface, Relay};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

const TOKEN: &str = "relay-token";
const KEY: &str = "sk-upstream";

// ---------- 基建 ----------

/// 上游收到的请求：（target, 小写头, body）
type Captured = (String, Vec<(String, String)>, Vec<u8>);

struct Upstream {
    addr: SocketAddr,
    requests: mpsc::UnboundedReceiver<Captured>,
}

impl Upstream {
    /// 每个连接读一个请求，按片段依次写出 `response`（片段间隔 5ms），然后关闭
    async fn start(response: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let tx = tx.clone();
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head_len, target, headers, body_len) = loop {
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let mut hs = [httparse::EMPTY_HEADER; 64];
                        let mut req = httparse::Request::new(&mut hs);
                        if let httparse::Status::Complete(len) = req.parse(&buf).unwrap() {
                            let headers: Vec<(String, String)> = req
                                .headers
                                .iter()
                                .map(|h| {
                                    (
                                        h.name.to_ascii_lowercase(),
                                        String::from_utf8_lossy(h.value).into_owned(),
                                    )
                                })
                                .collect();
                            let body_len = headers
                                .iter()
                                .find(|(n, _)| n == "content-length")
                                .map(|(_, v)| v.parse().unwrap())
                                .unwrap_or(0);
                            break (len, req.path.unwrap().to_string(), headers, body_len);
                        }
                    };
                    while buf.len() < head_len + body_len {
                        let n = stream.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let _ = tx.send((target, headers, buf[head_len..head_len + body_len].to_vec()));
                    for part in response {
                        if stream.write_all(&part).await.is_err() {
                            return;
                        }
                        let _ = stream.flush().await;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { addr, requests: rx }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn next(&mut self) -> (String, Vec<(String, String)>, Value) {
        let (target, headers, body) =
            tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
                .await
                .expect("upstream got no request")
                .unwrap();
        (
            target,
            headers,
            serde_json::from_slice(&body).unwrap_or(Value::Null),
        )
    }

    async fn assert_idle(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(200), self.requests.recv())
                .await
                .is_err()
        );
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn json_response(status: &str, body: &Value) -> Vec<Vec<u8>> {
    let body = body.to_string();
    vec![format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()]
}

/// SSE 响应：把整段事件文本切成 `piece` 字节一片的 chunked 片段
fn sse_response(events: &str, piece: usize) -> Vec<Vec<u8>> {
    let mut parts = vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec()];
    for chunk in events.as_bytes().chunks(piece) {
        let mut part = format!("{:x}\r\n", chunk.len()).into_bytes();
        part.extend_from_slice(chunk);
        part.extend_from_slice(b"\r\n");
        parts.push(part);
    }
    parts.push(b"0\r\n\r\n".to_vec());
    parts
}

fn upstream(id: &str, url: &str, protocol: Option<Interface>) -> UpstreamConfig {
    UpstreamConfig {
        id: id.into(),
        base_url: url.into(),
        api_key: Secret::new(KEY),
        auth: None,
        strip_prefix: None,
        proxy_url: None,
        protocol,
        model_map: [("*".to_string(), "deepseek-chat".to_string())]
            .into_iter()
            .filter(|_| protocol == Some(Interface::OpenaiChat))
            .collect(),
        default_max_output_tokens: None,
    }
}

async fn start_relay(claude: Vec<UpstreamConfig>) -> (SocketAddr, oneshot::Sender<()>) {
    let mut config = RelayConfig::default();
    config.server.auth_tokens = vec![Secret::new(TOKEN)];
    config.tls.native_roots = false;
    config.upstreams.claude = claude;
    let relay = Arc::new(Relay::with_converter(&config, Arc::new(IrConverter)).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve(listener, relay, async {
        let _ = rx.await;
    }));
    (addr, tx)
}

/// 发送 Claude 请求，返回（状态码, 头, 去分帧后的 body）
async fn send(addr: SocketAddr, body: &Value) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let body = body.to_string();
    let request = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: relay\r\nx-api-key: {TOKEN}\r\nanthropic-version: 2023-06-01\r\nX-Stainless-Lang: js\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
        .await
        .unwrap()
        .unwrap();
    let mut hs = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut hs);
    let head_len = match response.parse(&raw).unwrap() {
        httparse::Status::Complete(len) => len,
        _ => panic!("incomplete response"),
    };
    let headers: Vec<(String, String)> = response
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    let rest = &raw[head_len..];
    let body = if header(&headers, "transfer-encoding") == Some("chunked") {
        dechunk(rest)
    } else {
        rest.to_vec()
    };
    (response.code.unwrap(), headers, body)
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(end) = data.windows(2).position(|w| w == b"\r\n") {
        let size =
            usize::from_str_radix(std::str::from_utf8(&data[..end]).unwrap().trim(), 16).unwrap();
        data = &data[end + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
    out
}

/// 按 Claude 客户端的方式重建流：返回事件名序列与重建后的内容块
fn rebuild_anthropic_stream(body: &[u8]) -> (Vec<String>, Vec<Value>, Value) {
    let text = String::from_utf8(body.to_vec()).unwrap();
    let mut names = Vec::new();
    let mut blocks: Vec<Value> = Vec::new();
    let mut partial: Vec<String> = Vec::new();
    let mut message_delta = Value::Null;
    for event in text.split("\n\n").filter(|e| !e.trim().is_empty()) {
        let name = event
            .lines()
            .find_map(|l| l.strip_prefix("event: "))
            .unwrap()
            .to_string();
        let data: Value = serde_json::from_str(
            event
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        match name.as_str() {
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap() as usize;
                assert_eq!(index, blocks.len(), "indices must be consecutive");
                blocks.push(data["content_block"].clone());
                partial.push(String::new());
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap() as usize;
                let delta = &data["delta"];
                match delta["type"].as_str().unwrap() {
                    "text_delta" => {
                        let t = blocks[index]["text"].as_str().unwrap().to_string()
                            + delta["text"].as_str().unwrap();
                        blocks[index]["text"] = json!(t);
                    }
                    "thinking_delta" => {
                        let t = blocks[index]["thinking"].as_str().unwrap().to_string()
                            + delta["thinking"].as_str().unwrap();
                        blocks[index]["thinking"] = json!(t);
                    }
                    "input_json_delta" => {
                        partial[index].push_str(delta["partial_json"].as_str().unwrap())
                    }
                    other => panic!("unexpected delta {other}"),
                }
            }
            "content_block_stop" => {
                let index = data["index"].as_u64().unwrap() as usize;
                if blocks[index]["type"] == "tool_use" {
                    blocks[index]["input"] = serde_json::from_str(&partial[index]).unwrap();
                }
            }
            "message_delta" => message_delta = data,
            _ => {}
        }
        names.push(name);
    }
    (names, blocks, message_delta)
}

fn claude_request(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-5",
        "max_tokens": 4096,
        "stream": stream,
        "system": [{"type": "text", "text": "x-anthropic-billing-header: cch=9\n\nYou are Claude Code."}],
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "tools": [{"name": "Bash", "description": "run", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}}],
        "messages": [{"role": "user", "content": "list files"}]
    })
}

// ---------- 用例 ----------

#[tokio::test]
async fn non_streaming_claude_to_chat() {
    let mut up = Upstream::start(json_response(
        "200 OK",
        &json!({
            "id": "chatcmpl-1", "model": "deepseek-chat",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": "Let me check.", "reasoning_content": "need ls",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}}]
            }}],
            "usage": {"prompt_tokens": 120, "completion_tokens": 30, "prompt_tokens_details": {"cached_tokens": 100}}
        }),
    ))
    .await;
    let (addr, _stop) = start_relay(vec![upstream(
        "chat",
        &up.url(),
        Some(Interface::OpenaiChat),
    )])
    .await;

    let (status, headers, body) = send(addr, &claude_request(false)).await;
    let (target, up_headers, sent) = up.next().await;

    assert_eq!(target, "/v1/chat/completions");
    assert_eq!(
        header(&up_headers, "authorization"),
        Some("Bearer sk-upstream")
    );
    assert!(header(&up_headers, "anthropic-version").is_none());
    assert!(header(&up_headers, "x-api-key").is_none());
    assert!(header(&up_headers, "x-stainless-lang").is_none());
    assert_eq!(sent["model"], "deepseek-chat");
    assert_eq!(
        sent["messages"][0],
        json!({"role": "system", "content": "You are Claude Code."})
    );
    assert_eq!(sent["tools"][0]["function"]["name"], "Bash");

    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["model"], "claude-sonnet-5");
    assert_eq!(response["stop_reason"], "tool_use");
    assert_eq!(
        response["content"][0],
        json!({"type": "thinking", "thinking": "need ls"})
    );
    assert_eq!(
        response["content"][1],
        json!({"type": "text", "text": "Let me check."})
    );
    assert_eq!(
        response["content"][2],
        json!({"type": "tool_use", "id": "call_1", "name": "Bash", "input": {"command": "ls"}})
    );
    assert_eq!(response["usage"]["input_tokens"], 20);
    assert_eq!(response["usage"]["cache_read_input_tokens"], 100);
    assert_eq!(response["usage"]["output_tokens"], 30);
}

#[tokio::test]
async fn streaming_claude_to_chat_survives_tiny_chunks() {
    let events = [
        r#"{"id":"c1","model":"deepseek-chat","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":""}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":"需要 ls"}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"我来"}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"看看。"}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"comm"}}]}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":50,"completion_tokens":12}}"#,
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n";
    let mut up = Upstream::start(sse_response(&events, 7)).await;
    let (addr, _stop) = start_relay(vec![upstream(
        "chat",
        &up.url(),
        Some(Interface::OpenaiChat),
    )])
    .await;

    let (status, headers, body) = send(addr, &claude_request(true)).await;
    let (_, _, sent) = up.next().await;
    assert_eq!(sent["stream_options"], json!({"include_usage": true}));

    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
    let (names, blocks, message_delta) = rebuild_anthropic_stream(&body);
    assert_eq!(names.first().map(String::as_str), Some("message_start"));
    assert_eq!(names.last().map(String::as_str), Some("message_stop"));
    assert_eq!(
        blocks,
        vec![
            json!({"type": "thinking", "thinking": "需要 ls"}),
            json!({"type": "text", "text": "我来看看。"}),
            json!({"type": "tool_use", "id": "call_1", "name": "Bash", "input": {"command": "ls"}}),
        ]
    );
    assert_eq!(message_delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(message_delta["usage"]["input_tokens"], 50);
    assert_eq!(message_delta["usage"]["output_tokens"], 12);
}

#[tokio::test]
async fn chat_errors_come_back_in_anthropic_format() {
    let mut up = Upstream::start(json_response(
        "400 Bad Request",
        &json!({"error": {"message": "Invalid model", "type": "invalid_request_error"}}),
    ))
    .await;
    let (addr, _stop) = start_relay(vec![upstream(
        "chat",
        &up.url(),
        Some(Interface::OpenaiChat),
    )])
    .await;

    let (status, _, body) = send(addr, &claude_request(true)).await;
    up.next().await;
    assert_eq!(status, 400);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": "Invalid model"}})
    );
}

#[tokio::test]
async fn identity_keeps_every_field_but_the_model() {
    let reply = json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}});
    let mut up = Upstream::start(json_response("200 OK", &reply)).await;
    let mut identity = upstream("anthropic", &up.url(), None);
    identity
        .model_map
        .insert("claude-sonnet-5".into(), "claude-sonnet-4-5".into());
    let (addr, _stop) = start_relay(vec![identity]).await;

    let mut request = claude_request(false);
    request["system"][0]["cache_control"] = json!({"type": "ephemeral"});
    request["thinking"] = json!({"type": "adaptive"});
    request["output_config"] = json!({"effort": "high"});
    let (status, _, body) = send(addr, &request).await;
    let (target, headers, sent) = up.next().await;

    assert_eq!(target, "/v1/messages?beta=true");
    assert_eq!(header(&headers, "x-api-key"), Some(KEY));
    assert_eq!(header(&headers, "anthropic-version"), Some("2023-06-01"));
    let mut expected = request.clone();
    expected["model"] = json!("claude-sonnet-4-5");
    assert_eq!(sent, expected, "only the model name may change");

    assert_eq!(status, 200);
    let mut expected_reply = reply.clone();
    expected_reply["model"] = json!("claude-sonnet-5");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        expected_reply
    );
}

#[tokio::test]
async fn chat_failure_falls_back_to_a_passthrough_anthropic_upstream() {
    let mut chat = Upstream::start(json_response(
        "503 Service Unavailable",
        &json!({"error": {"message": "busy"}}),
    ))
    .await;
    let reply = json!({"id": "msg_1", "model": "claude-sonnet-5", "content": []});
    let mut anthropic = Upstream::start(json_response("200 OK", &reply)).await;
    let (addr, _stop) = start_relay(vec![
        upstream("chat", &chat.url(), Some(Interface::OpenaiChat)),
        upstream("anthropic", &anthropic.url(), None),
    ])
    .await;

    let request = claude_request(false);
    let (status, _, body) = send(addr, &request).await;
    chat.next().await;
    let (target, _, sent) = anthropic.next().await;
    assert_eq!(target, "/v1/messages?beta=true");
    assert_eq!(
        sent, request,
        "the passthrough upstream gets the original request"
    );
    assert_eq!(status, 200);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), reply);
}

#[tokio::test]
async fn count_tokens_skips_the_chat_upstream() {
    let mut chat = Upstream::start(json_response("200 OK", &json!({}))).await;
    let mut anthropic =
        Upstream::start(json_response("200 OK", &json!({"input_tokens": 12}))).await;
    let (addr, _stop) = start_relay(vec![
        upstream("chat", &chat.url(), Some(Interface::OpenaiChat)),
        upstream("anthropic", &anthropic.url(), None),
    ])
    .await;

    let body = json!({"model": "claude-sonnet-5", "messages": [{"role": "user", "content": "hi"}]})
        .to_string();
    let request = format!(
        "POST /v1/messages/count_tokens HTTP/1.1\r\nHost: relay\r\nx-api-key: {TOKEN}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    assert!(raw.starts_with(b"HTTP/1.1 200"));
    assert_eq!(anthropic.next().await.0, "/v1/messages/count_tokens");
    chat.assert_idle().await;
}

#[tokio::test]
async fn streaming_request_answered_with_json_becomes_an_anthropic_stream() {
    let mut up = Upstream::start(json_response(
        "200 OK",
        &json!({
            "id": "chatcmpl-1", "model": "deepseek-chat",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "hello"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1}
        }),
    ))
    .await;
    let (addr, _stop) = start_relay(vec![upstream(
        "chat",
        &up.url(),
        Some(Interface::OpenaiChat),
    )])
    .await;

    let (status, headers, body) = send(addr, &claude_request(true)).await;
    up.next().await;
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
    let (names, blocks, message_delta) = rebuild_anthropic_stream(&body);
    assert_eq!(names.last().map(String::as_str), Some("message_stop"));
    assert_eq!(blocks, vec![json!({"type": "text", "text": "hello"})]);
    assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
}

#[tokio::test]
async fn identity_streaming_rewrites_only_message_start() {
    let events = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5","usage":{"input_tokens":1}}}"#,
        "\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"model claude-sonnet-4-5"}}"#,
        "\n\nevent: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n"
    );
    let mut up = Upstream::start(sse_response(events, 5)).await;
    let mut identity = upstream("anthropic", &up.url(), None);
    identity
        .model_map
        .insert("claude-sonnet-5".into(), "claude-sonnet-4-5".into());
    let (addr, _stop) = start_relay(vec![identity]).await;

    let (status, _, body) = send(addr, &claude_request(true)).await;
    let (_, headers, sent) = up.next().await;
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(header(&headers, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        events.replacen(
            "\"model\":\"claude-sonnet-4-5\"",
            "\"model\":\"claude-sonnet-5\"",
            1
        )
    );
}

#[tokio::test]
async fn identity_streaming_request_answered_with_json_keeps_json() {
    let reply = json!({"id": "msg_1", "model": "claude-sonnet-4-5", "content": []});
    let mut up = Upstream::start(json_response("200 OK", &reply)).await;
    let mut identity = upstream("anthropic", &up.url(), None);
    identity
        .model_map
        .insert("claude-sonnet-5".into(), "claude-sonnet-4-5".into());
    let (addr, _stop) = start_relay(vec![identity]).await;

    let (status, headers, body) = send(addr, &claude_request(true)).await;
    up.next().await;
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    let mut expected = reply.clone();
    expected["model"] = json!("claude-sonnet-5");
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), expected);
}
