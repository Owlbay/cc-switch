//! `cc-proxy` 可执行程序端到端测试。

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const RELAY_TOKEN: &str = "relay-token-e2e";
const UPSTREAM_KEY: &str = "sk-upstream-e2e";

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cc-proxy"));
    cmd.env_remove("CC_PROXY_CONFIG").env("RUST_LOG", "info");
    cmd
}

/// 写一个只属于当前测试的配置文件（权限 600）
fn write_config(contents: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "cc-proxy-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("run cc-proxy")
}

// ---------- check ----------

#[test]
fn check_prints_dry_run_urls_without_secrets() {
    let path = write_config(
        r#"
[server]
auth_tokens = ["${E2E_TOKEN}"]

[[upstreams.claude]]
id = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "${E2E_KEY}"

[[upstreams.openai_chat]]
id = "azure"
base_url = "https://x.openai.azure.com/openai?api-version=2024-10-21"
api_key = "${E2E_KEY}"
proxy_url = "socks5h://127.0.0.1:1080"
"#,
    );
    let out = run(bin()
        .args(["check", "--config"])
        .arg(&path)
        .env("E2E_TOKEN", RELAY_TOKEN)
        .env("E2E_KEY", UPSTREAM_KEY));
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(
            "anthropic -> https://api.anthropic.com:443/v1/messages  auth=x-api-key via=direct"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("azure -> https://x.openai.azure.com:443/openai/v1/chat/completions?api-version=***  auth=bearer via=socks5"), "{stdout}");
    assert!(stdout.contains("[gemini]\n  （未配置上游"), "{stdout}");
    for secret in [RELAY_TOKEN, UPSTREAM_KEY, "2024-10-21"] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "leaked {secret}"
        );
    }
    assert!(stderr.is_empty(), "600 config must not warn: {stderr}");
}

#[test]
fn check_reports_undefined_variable_by_location() {
    let path = write_config(
        r#"
[server]
auth_tokens = ["t"]

[[upstreams.claude]]
id = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "${E2E_UNDEFINED_KEY}"
"#,
    );
    let out = run(bin()
        .args(["check", "--config"])
        .arg(&path)
        .env_remove("E2E_UNDEFINED_KEY"));
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("upstreams.claude[0].api_key"), "{stderr}");
    assert!(stderr.contains("E2E_UNDEFINED_KEY"), "{stderr}");
}

#[test]
fn check_rejects_public_listener_without_tokens() {
    let path = write_config("[server]\nlisten = \"0.0.0.0:15721\"\nallow_anonymous = true\n");
    let out = run(bin().args(["check", "--config"]).arg(&path));
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("server.auth_tokens"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn missing_config_file_is_a_config_error() {
    let out = run(bin().args(["serve", "--config", "/nonexistent/cc-proxy.toml"]));
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("/nonexistent/cc-proxy.toml"));
}

// ---------- serve ----------

/// 测试结束时（含失败）确保子进程被结束
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 读取 stdout 第一行 `listening on ADDR`
fn read_listen_addr(child: &mut Child) -> String {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("cc-proxy did not print its listen address");
    line.trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("unexpected first stdout line: {line:?}"))
        .to_string()
}

/// 原始 socket mock 上游：读一个请求，发出 SSE 响应头与第一个事件，等待后发出剩余部分
async fn start_upstream() -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                if request.len() >= end + 4 + len {
                    break;
                }
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\ndata: 1\n\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        stream
            .write_all(b"9\r\ndata: 2\n\n\r\n0\r\n\r\n")
            .await
            .unwrap();
        request
    });
    (format!("http://{addr}"), handle)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn serve_relays_and_drains_on_sigterm() {
    let (upstream_url, upstream) = start_upstream().await;
    let path = write_config(&format!(
        r#"
[server]
auth_tokens = ["${{E2E_TOKEN}}"]

[tls]
native_roots = false

[[upstreams.claude]]
id = "mock"
base_url = "{upstream_url}"
api_key = "${{E2E_KEY}}"
"#
    ));
    let mut child = bin()
        .args(["serve", "--listen", "127.0.0.1:0", "--config"])
        .arg(&path)
        .env("E2E_TOKEN", RELAY_TOKEN)
        .env("E2E_KEY", UPSTREAM_KEY)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cc-proxy");
    let addr = read_listen_addr(&mut child);
    let mut guard = ChildGuard(child);

    let body = br#"{"model":"m","stream":true,"secret":"BODY-MARKER"}"#;
    let mut client = TcpStream::connect(&addr).await.unwrap();
    client
        .write_all(
            format!(
                "POST /v1/messages?q=QUERY-MARKER HTTP/1.1\r\nHost: relay\r\nx-api-key: {RELAY_TOKEN}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    client.write_all(body).await.unwrap();

    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    while !raw.windows(7).any(|w| w == b"data: 1") {
        let n = client.read(&mut buf).await.unwrap();
        assert!(n > 0, "relay closed before the first event");
        raw.extend_from_slice(&buf[..n]);
    }

    // 流进行中发送 SIGTERM：进行中的流仍应完整结束
    let status = Command::new("kill")
        .args(["-TERM", &guard.0.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    client.read_to_end(&mut raw).await.unwrap();
    let raw = text(&raw);
    assert!(raw.starts_with("HTTP/1.1 200 OK"), "{raw}");
    assert!(raw.contains("data: 2"), "stream was cut on shutdown: {raw}");
    assert!(raw.ends_with("0\r\n\r\n"), "{raw}");

    let forwarded = text(&upstream.await.unwrap());
    assert!(
        forwarded.starts_with("POST /v1/messages?q=QUERY-MARKER HTTP/1.1\r\n"),
        "{forwarded}"
    );
    assert!(
        forwarded.contains(&format!("x-api-key: {UPSTREAM_KEY}")),
        "{forwarded}"
    );
    assert!(!forwarded.contains(RELAY_TOKEN));
    assert!(forwarded.ends_with(std::str::from_utf8(body).unwrap()));

    let exit = tokio::task::spawn_blocking(move || {
        let status = guard.0.wait().unwrap();
        let mut stderr = String::new();
        guard
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        (status, stderr)
    });
    let (status, stderr) = tokio::time::timeout(Duration::from_secs(10), exit)
        .await
        .expect("cc-proxy did not exit after SIGTERM")
        .unwrap();
    assert!(status.success(), "{status:?}\n{stderr}");
    assert!(stderr.contains("cc_proxy_core::access"), "{stderr}");
    assert!(stderr.contains("outcome=\"complete\""), "{stderr}");
    assert!(stderr.contains("收到退出信号"), "{stderr}");
    for secret in [RELAY_TOKEN, UPSTREAM_KEY, "BODY-MARKER", "QUERY-MARKER"] {
        assert!(!stderr.contains(secret), "log leaked {secret}:\n{stderr}");
    }
}
