//! 端到端测试共用设施：原始 TCP 字节的 mock 上游、客户端与流重建。
#![allow(dead_code)]

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

pub const TOKEN: &str = "relay-token";
pub const KEY: &str = "sk-upstream";

// ---------- 基建 ----------

/// 上游收到的请求：（target, 小写头, body）
pub type Captured = (String, Vec<(String, String)>, Vec<u8>);

pub struct Upstream {
    pub addr: SocketAddr,
    requests: mpsc::UnboundedReceiver<Captured>,
}

impl Upstream {
    /// 每个连接读一个请求，按片段依次写出 `response`（片段间隔 5ms），然后关闭
    pub async fn start(response: Vec<Vec<u8>>) -> Self {
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

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn next(&mut self) -> (String, Vec<(String, String)>, Value) {
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

    pub async fn assert_idle(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(200), self.requests.recv())
                .await
                .is_err()
        );
    }
}

pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

pub fn json_response(status: &str, body: &Value) -> Vec<Vec<u8>> {
    let body = body.to_string();
    vec![format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()]
}

/// SSE 响应：把整段事件文本切成 `piece` 字节一片的 chunked 片段
pub fn sse_response(events: &str, piece: usize) -> Vec<Vec<u8>> {
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

pub fn upstream(id: &str, url: &str, protocol: Option<Interface>) -> UpstreamConfig {
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

pub async fn start_relay(claude: Vec<UpstreamConfig>) -> (SocketAddr, oneshot::Sender<()>) {
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
pub async fn send(addr: SocketAddr, body: &Value) -> (u16, Vec<(String, String)>, Vec<u8>) {
    post(
        addr,
        "/v1/messages?beta=true",
        &[
            ("x-api-key", TOKEN),
            ("anthropic-version", "2023-06-01"),
            ("X-Stainless-Lang", "js"),
        ],
        body,
    )
    .await
}

/// 以给定 path 与头 POST 一个 JSON body
pub async fn post(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let body = body.to_string();
    let mut request = format!("POST {path} HTTP/1.1\r\nHost: relay\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    send_raw(addr, request.as_bytes()).await
}

/// 发送原始请求字节，返回（状态码, 小写头, 去分帧后的 body）
pub async fn send_raw(addr: SocketAddr, request: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
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

pub fn dechunk(mut data: &[u8]) -> Vec<u8> {
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
pub fn rebuild_anthropic_stream(body: &[u8]) -> (Vec<String>, Vec<Value>, Value) {
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
                    "signature_delta" => blocks[index]["signature"] = delta["signature"].clone(),
                    other => panic!("unexpected delta {other}"),
                }
            }
            "content_block_stop" => {
                let index = data["index"].as_u64().unwrap() as usize;
                let kind = &blocks[index]["type"];
                if (kind == "tool_use" || kind == "server_tool_use") && !partial[index].is_empty() {
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

/// 通用上游配置：`model_map` 由调用方给出
pub fn upstream_with(
    id: &str,
    url: &str,
    protocol: Option<Interface>,
    model_map: &[(&str, &str)],
) -> UpstreamConfig {
    UpstreamConfig {
        id: id.into(),
        base_url: url.into(),
        api_key: Secret::new(KEY),
        auth: None,
        strip_prefix: None,
        proxy_url: None,
        protocol,
        model_map: model_map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        default_max_output_tokens: None,
    }
}

/// 在指定接口下启动带转换器的中转
pub async fn start_relay_for(
    interface: Interface,
    upstreams: Vec<UpstreamConfig>,
) -> (SocketAddr, oneshot::Sender<()>) {
    let mut config = RelayConfig::default();
    config.server.auth_tokens = vec![Secret::new(TOKEN)];
    config.tls.native_roots = false;
    match interface {
        Interface::Claude => config.upstreams.claude = upstreams,
        Interface::OpenaiResponses => config.upstreams.openai_responses = upstreams,
        Interface::OpenaiChat => config.upstreams.openai_chat = upstreams,
        Interface::Gemini => config.upstreams.gemini = upstreams,
    }
    let relay = Arc::new(Relay::with_converter(&config, Arc::new(IrConverter)).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve(listener, relay, async {
        let _ = rx.await;
    }));
    (addr, tx)
}

/// 发送 Codex（Responses）请求
pub async fn send_responses(
    addr: SocketAddr,
    body: &Value,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let auth = format!("Bearer {TOKEN}");
    post(
        addr,
        "/v1/responses",
        &[
            ("Authorization", auth.as_str()),
            ("OpenAI-Beta", "responses=experimental"),
            ("originator", "codex_cli_rs"),
        ],
        body,
    )
    .await
}

/// 解析 Responses SSE：返回（事件类型序列, 最后的 response 对象）。
/// 同时校验 sequence_number 严格递增、每个 output_item.added 都有 done
pub fn parse_responses_stream(body: &[u8]) -> (Vec<String>, Value) {
    let text = String::from_utf8(body.to_vec()).unwrap();
    let mut names = Vec::new();
    let mut last_sequence: Option<u64> = None;
    let mut open_items = std::collections::BTreeSet::new();
    let mut final_response = Value::Null;
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
        assert_eq!(data["type"], name.as_str());
        if let Some(sequence) = data["sequence_number"].as_u64() {
            if let Some(last) = last_sequence {
                assert!(sequence > last, "sequence_number must increase");
            }
            last_sequence = Some(sequence);
        }
        match name.as_str() {
            "response.output_item.added" => {
                assert!(open_items.insert(data["output_index"].as_u64().unwrap()));
            }
            "response.output_item.done" => {
                assert!(open_items.remove(&data["output_index"].as_u64().unwrap()));
            }
            "response.completed" | "response.failed" | "response.incomplete" => {
                final_response = data["response"].clone();
            }
            _ => {}
        }
        names.push(name);
    }
    assert!(open_items.is_empty(), "every added item is done");
    (names, final_response)
}

pub fn json_value(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|_| json!(String::from_utf8_lossy(body)))
}
