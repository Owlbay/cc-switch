//! 快照测试（golden）：固定输入经每个转换方向后的**完整输出**，与 `tests/golden/` 下的快照逐字节比较。
//!
//! `fixtures_replay.rs` 守护语义等价（与分片无关），这里守护输出形态：字段、键序、事件序列、
//! 头与错误格式。转换逻辑的任何输出变化都会表现为快照 diff，在评审中直接可见。
//!
//! 有意修改转换逻辑后重新生成快照，再审查 `git diff tests/golden`：
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p cc-proxy-convert --test golden
//! ```
//!
//! 快照布局（方向按数据流向命名）：
//! - `request/<客户端>-to-<上游>.json`：客户端请求 → 上游请求（method、path、query、头、body、meta）
//! - `response/<上游>-to-<客户端>.json`：上游非流式 2xx 响应 → 客户端响应
//! - `stream/<上游>-to-<客户端>.json`：上游 SSE（`fixtures/*.sse`）→ 客户端 SSE 事件序列
//! - `error/<上游>-to-<客户端>.json`：上游 429 错误体 → 客户端错误体
//!
//! 规范化（使快照稳定、可读）：
//! - 转换器生成的 32 位十六进制 id 按首次出现编号为 `<id-N>`（同一快照内保持对应关系）；
//! - `resp_relay<时间>` → `resp_relay<ts>`；`created` / `created_at` 数值 → 0；
//! - `ccsw1.<src>.<base64url>` 签名封套解码为 `ccsw1.<src>.decoded:<JSON>`，封套内容同样规范化。

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bytes::Bytes;
use cc_proxy_convert::IrConverter;
use cc_proxy_core::convert::{
    ConversionContext, Converter, InboundRequest, ModelMap, OutboundMeta,
};
use cc_proxy_core::Interface;
use http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde_json::{json, Map, Value};

const CLIENTS: [Interface; 2] = [Interface::Claude, Interface::OpenaiResponses];

// ---------- 输入 ----------

fn envelope(src: &str, payload: Value) -> String {
    format!(
        "ccsw1.{src}.{}",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

/// Claude Code 形态的请求：计费头、缓存标记、thinking、函数工具 + WebSearch、图片，
/// 以及分别来自 Responses 上游（thinking 签名）与 Gemini 上游（调用签名）的两轮历史
fn claude_request() -> Value {
    let responses_reasoning = envelope(
        "openai_responses",
        json!({"item": {"id": "rs_prev", "type": "reasoning", "encrypted_content": "gAAAAABoPrevEncrypted",
            "summary": [{"type": "summary_text", "text": "先列目录"}]}}),
    );
    let gemini_call = envelope(
        "gemini",
        json!({"sig": "CiQBPrevThoughtSignature", "call_id": "toolu_2"}),
    );
    json!({
        "model": "claude-sonnet-5",
        "max_tokens": 8192,
        "stream": true,
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.0; cch=9;"},
            {"type": "text", "text": "You are Claude Code.", "cache_control": {"type": "ephemeral"}}
        ],
        "metadata": {"user_id": "user_abc_session_1"},
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "tools": [
            {"name": "Bash", "description": "Run a shell command", "input_schema": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            }},
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 5}
        ],
        "tool_choice": {"type": "auto"},
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "列出文件并看一下这张图"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "先列目录", "signature": responses_reasoning},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "a.txt\nb.png"}]}
            ]},
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": gemini_call},
                {"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {"command": "cat a.txt"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_2", "content": "hello"},
                {"type": "text", "text": "继续", "cache_control": {"type": "ephemeral"}}
            ]}
        ]
    })
}

fn claude_headers() -> HeaderMap {
    headers(&[
        ("content-type", "application/json"),
        ("anthropic-version", "2023-06-01"),
        (
            "anthropic-beta",
            "claude-code-20250219,interleaved-thinking-2025-05-14",
        ),
        ("user-agent", "claude-cli/2.1.0 (external, cli)"),
        ("x-claude-code-session-id", "session-1"),
        ("accept-encoding", "gzip, deflate"),
    ])
}

/// Codex CLI 形态的请求：instructions、`store: false` + `include`、function / custom / web_search 工具，
/// 以及来自 Anthropic 上游的推理封套、函数调用与 custom 工具调用历史
fn codex_request() -> Value {
    let claude_thinking = envelope(
        "claude",
        json!({"block": {"type": "thinking", "thinking": "需要先看文件", "signature": "EqQBPrevClaudeSignature=="}}),
    );
    json!({
        "model": "gpt-5-codex",
        "instructions": "You are Codex.",
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "session-1",
        "reasoning": {"effort": "high", "summary": "auto"},
        "tools": [
            {"type": "function", "name": "shell", "description": "Run a command", "strict": false, "parameters": {
                "type": "object",
                "properties": {"command": {"type": "array", "items": {"type": "string"}}},
                "required": ["command"],
                "additionalProperties": false
            }},
            {"type": "custom", "name": "apply_patch", "description": "Apply a patch", "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}},
            {"type": "web_search"}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Be brief."}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "修复 bug"}]},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "需要先看文件"}], "encrypted_content": claude_thinking},
            {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "a.txt"},
            {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch"},
            {"type": "custom_tool_call_output", "call_id": "call_2", "output": "Done!"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "继续"}]}
        ]
    })
}

fn codex_headers() -> HeaderMap {
    headers(&[
        ("content-type", "application/json"),
        ("accept", "text/event-stream"),
        ("openai-beta", "responses=experimental"),
        ("originator", "codex_cli_rs"),
        ("session_id", "session-1"),
        ("user-agent", "codex_cli_rs/0.50.0"),
    ])
}

fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    map
}

/// 四种上游各一份同一段对话（推理 → 文本 → 一次 Bash 调用）的非流式响应
fn response_fixture(upstream: Interface) -> &'static [u8] {
    match upstream {
        Interface::Claude => include_bytes!("fixtures/anthropic_thinking_tool.json"),
        Interface::OpenaiChat => include_bytes!("fixtures/chat_reasoning_tool.json"),
        Interface::OpenaiResponses => include_bytes!("fixtures/responses_reasoning_tool.json"),
        Interface::Gemini => include_bytes!("fixtures/gemini_thought_tool.json"),
    }
}

fn stream_fixture(upstream: Interface) -> &'static [u8] {
    match upstream {
        Interface::Claude => include_bytes!("fixtures/anthropic_thinking_tool.sse"),
        Interface::OpenaiChat => include_bytes!("fixtures/chat_reasoning_tool.sse"),
        Interface::OpenaiResponses => include_bytes!("fixtures/responses_reasoning_tool.sse"),
        Interface::Gemini => include_bytes!("fixtures/gemini_thought_tool.sse"),
    }
}

/// 各上游 429 错误体的典型形态
fn error_fixture(upstream: Interface) -> Value {
    match upstream {
        Interface::Claude => {
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}})
        }
        Interface::OpenaiChat => {
            json!({"error": {"message": "slow down", "type": "rate_limit_exceeded", "code": "rate_limit"}})
        }
        Interface::OpenaiResponses => {
            json!({"error": {"message": "slow down", "type": "rate_limit_error", "code": null}})
        }
        Interface::Gemini => {
            json!({"error": {"code": 429, "message": "slow down", "status": "RESOURCE_EXHAUSTED"}})
        }
    }
}

// ---------- 执行转换 ----------

fn model_map() -> ModelMap {
    ModelMap::from([("*".to_string(), "upstream-model".to_string())])
}

fn context(client: Interface, upstream: Interface, map: &ModelMap) -> ConversionContext<'_> {
    ConversionContext {
        client,
        upstream,
        model_map: map,
        default_max_output_tokens: 16384,
    }
}

fn response_meta(stream: bool) -> OutboundMeta {
    OutboundMeta {
        stream,
        client_model: "client-model".into(),
        upstream_model: "upstream-model".into(),
        tool_names: vec!["Bash".into()],
        ..Default::default()
    }
}

fn request_snapshot(client: Interface, upstream: Interface) -> Value {
    let (path, query, headers, body) = match client {
        Interface::Claude => (
            "/v1/messages",
            Some("beta=true"),
            claude_headers(),
            claude_request(),
        ),
        _ => ("/v1/responses", None, codex_headers(), codex_request()),
    };
    let body = Bytes::from(body.to_string());
    let method = Method::POST;
    let map = model_map();
    let out = IrConverter
        .convert_request(
            &context(client, upstream, &map),
            &InboundRequest {
                method: &method,
                path,
                query,
                headers: &headers,
                body: &body,
            },
        )
        .unwrap_or_else(|e| panic!("{} -> {}: {e}", client.as_str(), upstream.as_str()));
    json!({
        "method": out.method.as_str(),
        "path": out.path,
        "query": out.query,
        "headers": headers_json(&out.headers),
        "body": serde_json::from_slice::<Value>(&out.body).expect("request body is JSON"),
        "meta": {
            "stream": out.meta.stream,
            "client_model": out.meta.client_model,
            "upstream_model": out.meta.upstream_model,
            "tool_names": out.meta.tool_names,
            "custom_tool_names": out.meta.custom_tool_names,
            "hosted_web_search": out.meta.hosted_web_search,
        }
    })
}

fn full_snapshot(client: Interface, upstream: Interface, status: StatusCode, body: &[u8]) -> Value {
    let map = model_map();
    let mut converter =
        IrConverter.response_converter(&context(client, upstream, &map), &response_meta(false));
    let head = converter.convert_head(status, &json_headers());
    let out = converter
        .convert_full(status, body)
        .unwrap_or_else(|e| panic!("{} -> {}: {e}", upstream.as_str(), client.as_str()));
    json!({
        "status": status.as_u16(),
        "headers": headers_json(&head),
        "body": serde_json::from_slice::<Value>(&out).expect("converted body is JSON"),
    })
}

fn stream_snapshot(client: Interface, upstream: Interface) -> Value {
    let map = model_map();
    let mut converter =
        IrConverter.response_converter(&context(client, upstream, &map), &response_meta(true));
    let mut upstream_headers = HeaderMap::new();
    upstream_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    let head = converter.convert_head(StatusCode::OK, &upstream_headers);
    let mut out = converter.feed(stream_fixture(upstream)).unwrap();
    out.extend(converter.finish(None));
    let text: String = out
        .iter()
        .map(|b| String::from_utf8(b.to_vec()).expect("SSE output is UTF-8"))
        .collect();
    json!({
        "headers": headers_json(&head),
        "events": sse_events(&text),
    })
}

fn json_headers() -> HeaderMap {
    headers(&[("content-type", "application/json")])
}

/// 头按名字排序后输出，同名头保持相对顺序
fn headers_json(headers: &HeaderMap) -> Value {
    let mut sorted: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for (name, value) in headers {
        sorted
            .entry(name.as_str())
            .or_default()
            .push(json!(value.to_str().expect("ASCII header value")));
    }
    Value::Array(
        sorted
            .into_iter()
            .flat_map(|(name, values)| values.into_iter().map(move |v| json!([name, v])))
            .collect(),
    )
}

/// 解析编码器输出的 SSE：每个事件必须是 `event: <name>\ndata: <json>` 成帧
fn sse_events(text: &str) -> Value {
    assert!(text.ends_with("\n\n"), "SSE output ends with a blank line");
    Value::Array(
        text.split("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(|frame| {
                let mut event = None;
                let mut data = Vec::new();
                for line in frame.lines() {
                    if let Some(name) = line.strip_prefix("event: ") {
                        assert!(event.replace(name).is_none(), "one event name per frame");
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push(d);
                    } else {
                        panic!("unexpected SSE line {line:?}");
                    }
                }
                let data = data.join("\n");
                json!({
                    "event": event,
                    "data": serde_json::from_str::<Value>(&data)
                        .unwrap_or_else(|_| panic!("SSE data is JSON: {data}")),
                })
            })
            .collect(),
    )
}

// ---------- 规范化 ----------

#[derive(Default)]
struct Normalizer {
    ids: BTreeMap<String, usize>,
}

impl Normalizer {
    fn value(&mut self, value: Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.string(&s)),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| self.value(v)).collect()),
            Value::Object(object) => {
                let mut out = Map::new();
                for (key, v) in object {
                    let v = match (key.as_str(), &v) {
                        ("created" | "created_at", Value::Number(_)) => json!(0),
                        _ => self.value(v),
                    };
                    out.insert(key, v);
                }
                Value::Object(out)
            }
            other => other,
        }
    }

    fn string(&mut self, s: &str) -> String {
        if let Some(decoded) = self.envelope(s) {
            return decoded;
        }
        // 字符串里可能嵌着 JSON（如工具参数），逐字符替换即可覆盖
        let s = self.generated_ids(s);
        replace_hex_suffix(&s, "resp_relay", "<ts>")
    }

    fn envelope(&mut self, s: &str) -> Option<String> {
        let rest = s.strip_prefix("ccsw1.")?;
        let (src, body) = rest.split_once('.')?;
        let payload = URL_SAFE_NO_PAD.decode(body).ok()?;
        let payload: Value = serde_json::from_slice(&payload).ok()?;
        let payload = self.value(payload);
        Some(format!("ccsw1.{src}.decoded:{payload}"))
    }

    /// 恰好 32 位小写十六进制的片段视为生成的 id（`gemini::random_hex`）
    fn generated_ids(&mut self, s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < bytes.len() {
            let run = bytes[i..]
                .iter()
                .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                .count();
            let boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if run == 32 && boundary {
                let next = self.ids.len() + 1;
                let n = *self.ids.entry(s[i..i + 32].to_string()).or_insert(next);
                write!(out, "<id-{n}>").unwrap();
                i += 32;
            } else {
                let step = run.max(1);
                // run 只覆盖 ASCII；非 ASCII 字符按完整 UTF-8 字符前进
                let step = if run == 0 {
                    s[i..].chars().next().map_or(1, char::len_utf8)
                } else {
                    step
                };
                out.push_str(&s[i..i + step]);
                i += step;
            }
        }
        out
    }
}

fn replace_hex_suffix(s: &str, prefix: &str, placeholder: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(prefix) {
        let after = &rest[at + prefix.len()..];
        let run = after.bytes().take_while(u8::is_ascii_hexdigit).count();
        out.push_str(&rest[..at + prefix.len()]);
        if run > 0 {
            out.push_str(placeholder);
        }
        rest = &after[run..];
    }
    out.push_str(rest);
    out
}

// ---------- 快照比较 ----------

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// 与快照比较；`UPDATE_GOLDEN=1` 时改为写入。返回不一致的描述
fn check(name: &str, snapshot: Value) -> Option<String> {
    let snapshot = Normalizer::default().value(snapshot);
    let actual = serde_json::to_string_pretty(&snapshot).unwrap() + "\n";
    let path = golden_dir().join(format!("{name}.json"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return None;
    }
    let Ok(expected) = std::fs::read_to_string(&path) else {
        return Some(format!("{name}: 快照不存在"));
    };
    if expected == actual {
        return None;
    }
    let (line, want, got) = expected
        .lines()
        .zip(actual.lines())
        .enumerate()
        .find(|(_, (want, got))| want != got)
        .map(|(i, (want, got))| (i + 1, want.to_string(), got.to_string()))
        .unwrap_or_else(|| {
            let line = expected.lines().count().min(actual.lines().count()) + 1;
            (line, "<行数不同>".into(), "<行数不同>".into())
        });
    Some(format!(
        "{name}: 第 {line} 行不同\n    快照: {want}\n    实际: {got}"
    ))
}

fn assert_all(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} 个快照与输出不一致（有意修改时运行 `UPDATE_GOLDEN=1 cargo test -p cc-proxy-convert --test golden` 并审查 diff）：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ---------- 用例 ----------

#[test]
fn request_conversion_matches_golden() {
    let mut failures = Vec::new();
    for client in CLIENTS {
        for upstream in Interface::ALL {
            let name = format!("request/{}-to-{}", client.as_str(), upstream.as_str());
            failures.extend(check(&name, request_snapshot(client, upstream)));
        }
    }
    assert_all(failures);
}

#[test]
fn response_conversion_matches_golden() {
    let mut failures = Vec::new();
    for client in CLIENTS {
        for upstream in Interface::ALL {
            let name = format!("response/{}-to-{}", upstream.as_str(), client.as_str());
            let snapshot =
                full_snapshot(client, upstream, StatusCode::OK, response_fixture(upstream));
            failures.extend(check(&name, snapshot));
        }
    }
    assert_all(failures);
}

#[test]
fn stream_conversion_matches_golden() {
    let mut failures = Vec::new();
    for client in CLIENTS {
        for upstream in Interface::ALL {
            let name = format!("stream/{}-to-{}", upstream.as_str(), client.as_str());
            failures.extend(check(&name, stream_snapshot(client, upstream)));
        }
    }
    assert_all(failures);
}

#[test]
fn error_conversion_matches_golden() {
    let mut failures = Vec::new();
    for client in CLIENTS {
        for upstream in Interface::ALL {
            let name = format!("error/{}-to-{}", upstream.as_str(), client.as_str());
            let body = error_fixture(upstream).to_string();
            let snapshot = full_snapshot(
                client,
                upstream,
                StatusCode::TOO_MANY_REQUESTS,
                body.as_bytes(),
            );
            failures.extend(check(&name, snapshot));
        }
    }
    assert_all(failures);
}

#[test]
fn normalizer_is_stable_and_keeps_references() {
    let id = "0123456789abcdef0123456789abcdef";
    let mut n = Normalizer::default();
    let out = n.value(json!({
        "a": format!("call_{id}"),
        "b": format!("fc_call_{id}"),
        "c": "call_00_x1",
        "d": "resp_relay18a2b3c4d5e6f",
        "created_at": 1759200000,
        "e": envelope("gemini", json!({"sig": "s", "call_id": format!("call_{id}")})),
        "f": "我先看一下",
    }));
    assert_eq!(
        out,
        json!({
            "a": "call_<id-1>",
            "b": "fc_call_<id-1>",
            "c": "call_00_x1",
            "d": "resp_relay<ts>",
            "created_at": 0,
            "e": "ccsw1.gemini.decoded:{\"sig\":\"s\",\"call_id\":\"call_<id-1>\"}",
            "f": "我先看一下",
        })
    );
}
