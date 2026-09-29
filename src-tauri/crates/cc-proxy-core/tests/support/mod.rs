//! 原始 socket 测试基建（设计文档 §11）。
//!
//! mock 上游与测试客户端都直接读写 TCP 字节，不经过 hyper / axum 的重新解析与分帧，
//! 因此能看到头名的原始大小写、顺序与 body 的原始字节。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cc_proxy_core::config::{RelayConfig, Secret, UpstreamConfig};
use cc_proxy_core::{serve, Relay};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

pub const RELAY_TOKEN: &str = "relay-token";
pub const UPSTREAM_KEY: &str = "sk-upstream";

/// mock 上游收到的一次请求
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    /// 请求行中的 target（path + query）
    pub target: String,
    /// (原始大小写的头名, 值字节)，按 wire 顺序
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
    /// 完整原始字节
    pub raw: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.header(name).map(|v| std::str::from_utf8(v).unwrap())
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    /// 头名（原始大小写）按 wire 顺序
    pub fn header_names(&self) -> Vec<&str> {
        self.headers.iter().map(|(n, _)| n.as_str()).collect()
    }
}

/// 一段响应字节及写出前的等待时间
#[derive(Debug, Clone)]
pub struct Segment {
    pub delay: Duration,
    pub bytes: Vec<u8>,
}

impl Segment {
    pub fn now(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            delay: Duration::ZERO,
            bytes: bytes.into(),
        }
    }

    pub fn after(delay: Duration, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            delay,
            bytes: bytes.into(),
        }
    }
}

/// 原始字节 mock 上游：每个连接读取一个请求，按脚本写出响应后关闭连接。
pub struct MockUpstream {
    pub addr: SocketAddr,
    requests: mpsc::UnboundedReceiver<RecordedRequest>,
    /// 等待写出下一段响应期间观察到对端关闭连接
    closed: mpsc::UnboundedReceiver<()>,
}

impl MockUpstream {
    /// 对每个请求返回同一份响应脚本
    pub async fn start(response: Vec<Segment>) -> Self {
        Self::start_inner(response, None).await
    }

    /// HTTPS 版本：用给定的 TLS acceptor 握手后再按脚本应答
    pub async fn start_tls(response: Vec<Segment>, acceptor: tokio_rustls::TlsAcceptor) -> Self {
        Self::start_inner(response, Some(acceptor)).await
    }

    async fn start_inner(
        response: Vec<Segment>,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                let closed_tx = closed_tx.clone();
                let response = response.clone();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        None => handle_upstream_connection(stream, tx, closed_tx, response).await,
                        Some(acceptor) => {
                            // 握手失败（客户端不信任证书）时直接结束
                            if let Ok(tls) = acceptor.accept(stream).await {
                                handle_upstream_connection(tls, tx, closed_tx, response).await;
                            }
                        }
                    }
                });
            }
        });
        Self {
            addr,
            requests: rx,
            closed: closed_rx,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 等待下一次请求（最多 5 秒）
    pub async fn next_request(&mut self) -> RecordedRequest {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("upstream did not receive a request in time")
            .expect("upstream channel closed")
    }

    /// 等待对端（中转）关闭连接，返回是否在 `within` 内观察到
    pub async fn closed_within(&mut self, within: Duration) -> bool {
        matches!(
            tokio::time::timeout(within, self.closed.recv()).await,
            Ok(Some(()))
        )
    }

    /// 在给定时间内没有收到请求
    pub async fn assert_no_request(&mut self, within: Duration) {
        if let Ok(Some(request)) = tokio::time::timeout(within, self.requests.recv()).await {
            panic!(
                "unexpected upstream request: {} {}",
                request.method, request.target
            );
        }
    }
}

async fn handle_upstream_connection<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    tx: mpsc::UnboundedSender<RecordedRequest>,
    closed_tx: mpsc::UnboundedSender<()>,
    response: Vec<Segment>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let _ = tx.send(request);
    for segment in response {
        if !segment.delay.is_zero() {
            // 等待期间同时读 socket：读到 EOF 或出错说明对端（中转）已关闭连接
            let sleep = tokio::time::sleep(segment.delay);
            tokio::pin!(sleep);
            let mut scratch = [0u8; 1024];
            loop {
                tokio::select! {
                    () = &mut sleep => break,
                    read = stream.read(&mut scratch) => match read {
                        Ok(0) | Err(_) => {
                            let _ = closed_tx.send(());
                            return;
                        }
                        Ok(_) => {}
                    },
                }
            }
        }
        if stream.write_all(&segment.bytes).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
    }
    let _ = stream.shutdown().await;
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_len = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 128];
        let mut request = httparse::Request::new(&mut headers);
        if let httparse::Status::Complete(len) = request.parse(&buf).unwrap() {
            break len;
        }
    };

    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Request::new(&mut headers);
    parsed.parse(&buf).unwrap();
    let method = parsed.method.unwrap().to_string();
    let target = parsed.path.unwrap().to_string();
    let header_pairs: Vec<(String, Vec<u8>)> = parsed
        .headers
        .iter()
        .map(|h| (h.name.to_string(), h.value.to_vec()))
        .collect();

    let content_length = header_pairs
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| {
            std::str::from_utf8(v)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap()
        });
    let chunked = header_pairs.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding")
            && String::from_utf8_lossy(v)
                .to_ascii_lowercase()
                .contains("chunked")
    });
    assert!(
        !chunked,
        "relay must send buffered bodies with content-length"
    );

    let body_len = content_length.unwrap_or(0);
    while buf.len() < head_len + body_len {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[head_len..head_len + body_len].to_vec();

    Some(RecordedRequest {
        method,
        target,
        headers: header_pairs,
        body,
        raw: buf,
    })
}

/// 客户端收到的响应（已去分帧）
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
    pub raw: Vec<u8>,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.header(name).map(|v| std::str::from_utf8(v).unwrap())
    }

    pub fn header_names(&self) -> Vec<&str> {
        self.headers.iter().map(|(n, _)| n.as_str()).collect()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// 解析一段完整的原始响应字节；`head_request` 为 true 时不读取 body。
pub fn parse_response(raw: &[u8], head_request: bool) -> RawResponse {
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Response::new(&mut headers);
    let head_len = match parsed.parse(raw).unwrap() {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => panic!(
            "incomplete response head: {:?}",
            String::from_utf8_lossy(raw)
        ),
    };
    let status = parsed.code.unwrap();
    let header_pairs: Vec<(String, Vec<u8>)> = parsed
        .headers
        .iter()
        .map(|h| (h.name.to_string(), h.value.to_vec()))
        .collect();
    let rest = &raw[head_len..];
    let chunked = header_pairs.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding")
            && String::from_utf8_lossy(v)
                .to_ascii_lowercase()
                .contains("chunked")
    });
    let body = if head_request || status == 204 || status == 304 {
        Vec::new()
    } else if chunked {
        dechunk(rest)
    } else {
        rest.to_vec()
    };
    RawResponse {
        status,
        headers: header_pairs,
        body,
        raw: raw.to_vec(),
    }
}

/// 去掉 chunked 分帧（忽略 trailer）
pub fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = find(data, b"\r\n").expect("chunk size line");
        let size_str = std::str::from_utf8(&data[..line_end]).unwrap();
        let size = usize::from_str_radix(size_str.split(';').next().unwrap().trim(), 16).unwrap();
        data = &data[line_end + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 发送原始请求字节并读取到连接关闭为止（请求应带 `Connection: close`）。
pub async fn send_raw(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut out))
        .await
        .expect("relay response timed out")
        .unwrap();
    out
}

pub async fn request(addr: SocketAddr, raw_request: &[u8]) -> RawResponse {
    let head_request = raw_request.starts_with(b"HEAD ");
    parse_response(&send_raw(addr, raw_request).await, head_request)
}

/// 组装原始请求：`headers` 原样写出（含大小写），自动追加 `Connection: close`，
/// 有 body 时追加 `Content-Length`。
pub fn raw_request(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {target} HTTP/1.1\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    if !body.is_empty() {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    out
}

/// 组装固定长度的原始响应（自动追加 `Content-Length` 与 `Connection: close`）
pub fn raw_response(status_line: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status_line}\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(body);
    out
}

pub fn upstream(id: &str, base_url: &str) -> UpstreamConfig {
    UpstreamConfig {
        id: id.to_string(),
        base_url: base_url.to_string(),
        api_key: Secret::new(UPSTREAM_KEY),
        auth: None,
        strip_prefix: None,
        proxy_url: None,
    }
}

/// 基础配置：鉴权 token 为 [`RELAY_TOKEN`]，不加载系统根证书（测试上游都是明文 http）。
pub fn base_config() -> RelayConfig {
    let mut config = RelayConfig::default();
    config.server.auth_tokens = vec![Secret::new(RELAY_TOKEN)];
    config.tls.native_roots = false;
    config
}

/// 正在运行的中转
pub struct RunningRelay {
    pub addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for RunningRelay {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub async fn start_relay(config: RelayConfig) -> RunningRelay {
    let relay = Arc::new(Relay::new(&config).expect("relay config"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve(listener, relay, async {
        let _ = rx.await;
    }));
    RunningRelay {
        addr,
        shutdown: Some(tx),
    }
}
