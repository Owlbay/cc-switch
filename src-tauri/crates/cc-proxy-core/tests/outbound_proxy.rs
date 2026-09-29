//! 出站代理集成测试（设计文档 §5.6、§11 T14）。

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// mock 代理记录到的一次连接
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProxyRecord {
    /// HTTP CONNECT：请求目标与 Proxy-Authorization
    Connect {
        target: String,
        proxy_authorization: Option<String>,
    },
    /// SOCKS5：目标（IP 或域名）、端口、认证信息
    Socks {
        target: SocksTarget,
        port: u16,
        credentials: Option<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SocksTarget {
    Ip(std::net::IpAddr),
    Domain(String),
}

struct MockProxy {
    addr: SocketAddr,
    records: mpsc::UnboundedReceiver<ProxyRecord>,
}

impl MockProxy {
    async fn next(&mut self) -> ProxyRecord {
        tokio::time::timeout(Duration::from_secs(5), self.records.recv())
            .await
            .expect("proxy was not used")
            .unwrap()
    }
}

/// 最小 HTTP CONNECT 代理：回 200 后双向转发
async fn start_connect_proxy() -> MockProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                while find(&buf, b"\r\n\r\n").is_none() {
                    if client.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    buf.push(byte[0]);
                }
                let head = String::from_utf8(buf).unwrap();
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap();
                let mut parts = request_line.split(' ');
                assert_eq!(parts.next(), Some("CONNECT"));
                let target = parts.next().unwrap().to_string();
                let proxy_authorization = lines
                    .filter_map(|l| l.split_once(':'))
                    .find(|(n, _)| n.eq_ignore_ascii_case("proxy-authorization"))
                    .map(|(_, v)| v.trim().to_string());
                let _ = tx.send(ProxyRecord::Connect {
                    target: target.clone(),
                    proxy_authorization,
                });
                let Ok(mut upstream) = TcpStream::connect(&target).await else {
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    MockProxy { addr, records: rx }
}

/// 最小 SOCKS5 代理：支持无认证与用户名密码认证；域名 `localhost` 解析为 127.0.0.1
async fn start_socks_proxy(require_auth: bool) -> MockProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                // 问候：VER NMETHODS METHODS...
                let mut head = [0u8; 2];
                client.read_exact(&mut head).await.unwrap();
                assert_eq!(head[0], 5);
                let mut methods = vec![0u8; head[1] as usize];
                client.read_exact(&mut methods).await.unwrap();

                let mut credentials = None;
                if require_auth {
                    assert!(methods.contains(&2), "client must offer user/pass auth");
                    client.write_all(&[5, 2]).await.unwrap();
                    let mut ver_ulen = [0u8; 2];
                    client.read_exact(&mut ver_ulen).await.unwrap();
                    let mut user = vec![0u8; ver_ulen[1] as usize];
                    client.read_exact(&mut user).await.unwrap();
                    let mut plen = [0u8; 1];
                    client.read_exact(&mut plen).await.unwrap();
                    let mut pass = vec![0u8; plen[0] as usize];
                    client.read_exact(&mut pass).await.unwrap();
                    credentials = Some((
                        String::from_utf8(user).unwrap(),
                        String::from_utf8(pass).unwrap(),
                    ));
                    client.write_all(&[1, 0]).await.unwrap();
                } else {
                    assert!(methods.contains(&0));
                    client.write_all(&[5, 0]).await.unwrap();
                }

                // 请求：VER CMD RSV ATYP DST.ADDR DST.PORT
                let mut req = [0u8; 4];
                client.read_exact(&mut req).await.unwrap();
                assert_eq!(&req[..3], &[5, 1, 0]);
                let target = match req[3] {
                    1 => {
                        let mut ip = [0u8; 4];
                        client.read_exact(&mut ip).await.unwrap();
                        SocksTarget::Ip(std::net::IpAddr::from(ip))
                    }
                    3 => {
                        let mut len = [0u8; 1];
                        client.read_exact(&mut len).await.unwrap();
                        let mut name = vec![0u8; len[0] as usize];
                        client.read_exact(&mut name).await.unwrap();
                        SocksTarget::Domain(String::from_utf8(name).unwrap())
                    }
                    4 => {
                        let mut ip = [0u8; 16];
                        client.read_exact(&mut ip).await.unwrap();
                        SocksTarget::Ip(std::net::IpAddr::from(ip))
                    }
                    other => panic!("unexpected atyp {other}"),
                };
                let mut port = [0u8; 2];
                client.read_exact(&mut port).await.unwrap();
                let port = u16::from_be_bytes(port);
                let _ = tx.send(ProxyRecord::Socks {
                    target: target.clone(),
                    port,
                    credentials,
                });

                let host = match &target {
                    // 本地解析 localhost 可能得到 ::1，mock 上游只监听 127.0.0.1
                    SocksTarget::Ip(ip) if ip.is_loopback() => "127.0.0.1".to_string(),
                    SocksTarget::Ip(ip) => ip.to_string(),
                    SocksTarget::Domain(d) if d == "localhost" => "127.0.0.1".to_string(),
                    SocksTarget::Domain(d) => d.clone(),
                };
                let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await else {
                    let _ = client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                    return;
                };
                client
                    .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    MockProxy { addr, records: rx }
}

fn ok_json() -> Vec<Segment> {
    vec![Segment::now(raw_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        b"{\"ok\":true}",
    ))]
}

const BODY: &[u8] = b"{\"model\":\"m\",\"messages\":[]}";

async fn send_claude(relay: &RunningRelay) -> RawResponse {
    request(
        relay.addr,
        &raw_request(
            "POST",
            "/v1/messages?beta=true",
            &[("Host", "relay"), ("x-api-key", RELAY_TOKEN)],
            BODY,
        ),
    )
    .await
}

fn assert_upstream_request(got: &RecordedRequest) {
    assert_eq!(got.method, "POST");
    assert_eq!(got.target, "/v1/messages?beta=true");
    assert_eq!(got.body, BODY);
    assert_eq!(got.header_str("x-api-key"), Some(UPSTREAM_KEY));
    assert!(!got.has_header("proxy-authorization"));
    assert!(find(&got.raw, b"secret").is_none());
}

#[tokio::test]
async fn http_connect_proxy_uses_explicit_port_and_basic_auth() {
    let mut up = MockUpstream::start(ok_json()).await;
    let mut proxy = start_connect_proxy().await;
    let mut config = base_config();
    config.server.proxy_url = Some(format!("http://user:secret@{}", proxy.addr));
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    let resp = send_claude(&relay).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"{\"ok\":true}");

    assert_eq!(
        proxy.next().await,
        ProxyRecord::Connect {
            // http 上游不能被 CONNECT 到 443
            target: up.addr.to_string(),
            proxy_authorization: Some("Basic dXNlcjpzZWNyZXQ=".to_string()),
        }
    );
    assert_upstream_request(&up.next_request().await);
}

#[tokio::test]
async fn socks5_resolves_locally() {
    let mut up = MockUpstream::start(ok_json()).await;
    let mut proxy = start_socks_proxy(false).await;
    let mut config = base_config();
    config.upstreams.claude = vec![{
        let mut u = upstream("a", &format!("http://localhost:{}", up.addr.port()));
        u.proxy_url = Some(format!("socks5://{}", proxy.addr));
        u
    }];
    let relay = start_relay(config).await;

    assert_eq!(send_claude(&relay).await.status, 200);
    // socks5：域名在本地解析，代理只看到 IP
    match proxy.next().await {
        ProxyRecord::Socks {
            target: SocksTarget::Ip(ip),
            port,
            credentials: None,
        } => {
            assert!(ip.is_loopback(), "{ip}");
            assert_eq!(port, up.addr.port());
        }
        other => panic!("expected an IP target, got {other:?}"),
    }
    assert_upstream_request(&up.next_request().await);
}

#[tokio::test]
async fn socks5h_sends_domain_and_credentials_to_proxy() {
    let mut up = MockUpstream::start(ok_json()).await;
    let mut proxy = start_socks_proxy(true).await;
    let mut config = base_config();
    config.upstreams.claude = vec![{
        let mut u = upstream("a", &format!("http://localhost:{}", up.addr.port()));
        u.proxy_url = Some(format!("socks5h://user:secret@{}", proxy.addr));
        u
    }];
    let relay = start_relay(config).await;

    assert_eq!(send_claude(&relay).await.status, 200);
    assert_eq!(
        proxy.next().await,
        ProxyRecord::Socks {
            target: SocksTarget::Domain("localhost".to_string()),
            port: up.addr.port(),
            credentials: Some(("user".to_string(), "secret".to_string())),
        }
    );
    let got = up.next_request().await;
    assert_upstream_request(&got);
    assert_eq!(
        got.header_str("host"),
        Some(format!("localhost:{}", up.addr.port()).as_str())
    );
}

#[tokio::test]
async fn upstream_can_opt_out_of_global_proxy() {
    let mut up = MockUpstream::start(ok_json()).await;
    let mut proxy = start_connect_proxy().await;
    let mut config = base_config();
    config.server.proxy_url = Some(format!("http://{}", proxy.addr));
    config.upstreams.claude = vec![{
        let mut u = upstream("a", &up.base_url());
        u.proxy_url = Some(String::new());
        u
    }];
    let relay = start_relay(config).await;

    assert_eq!(send_claude(&relay).await.status, 200);
    assert_upstream_request(&up.next_request().await);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), proxy.records.recv())
            .await
            .is_err(),
        "direct upstream must not use the global proxy"
    );
}

#[tokio::test]
async fn unreachable_proxy_is_a_bad_gateway() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let mut up = MockUpstream::start(ok_json()).await;
    let mut config = base_config();
    config.server.proxy_url = Some(format!("socks5h://{closed}"));
    config.upstreams.claude = vec![upstream("a", &up.base_url())];
    let relay = start_relay(config).await;

    let resp = send_claude(&relay).await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.json()["error"]["type"], "relay_bad_gateway");
    up.assert_no_request(Duration::from_millis(200)).await;
}
