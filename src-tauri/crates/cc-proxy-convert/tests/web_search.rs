//! 端到端：内置搜索的响应方向映射（设计文档 §5.4、§5.6）。
//!
//! - Claude 客户端 + Responses 上游：`web_search_call` → `server_tool_use` + `web_search_tool_result`；
//! - Codex 客户端 + Anthropic 上游：`server_tool_use` + `web_search_tool_result` → `web_search_call`。

mod support;

use cc_proxy_core::Interface;
use serde_json::{json, Value};
use support::*;

fn sse_of(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect()
}

// ---------- Claude → Responses ----------

fn claude_request(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-5",
        "max_tokens": 4000,
        "stream": stream,
        "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 3}],
        "messages": [{"role": "user", "content": "rust 最新版本？"}]
    })
}

fn web_search_item() -> Value {
    json!({"id": "ws_1", "type": "web_search_call", "status": "completed",
        "action": {"type": "search", "query": "rust latest version"}})
}

fn answer_item() -> Value {
    json!({"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
    "content": [{"type": "output_text", "text": "Rust 1.90。", "annotations": [
        {"type": "url_citation", "start_index": 0, "end_index": 4, "url": "https://blog.rust-lang.org", "title": "Rust Blog"}
    ]}]})
}

fn assert_claude_search_blocks(blocks: &[Value]) {
    assert_eq!(blocks.len(), 3, "{blocks:?}");
    assert_eq!(
        blocks[0],
        json!({"type": "server_tool_use", "id": "ws_1", "name": "web_search", "input": {"query": "rust latest version"}})
    );
    assert_eq!(
        blocks[1],
        json!({"type": "web_search_tool_result", "tool_use_id": "ws_1", "content": []})
    );
    assert_eq!(blocks[2], json!({"type": "text", "text": "Rust 1.90。"}));
}

async fn claude_relay_to_responses(
    response: Vec<Vec<u8>>,
) -> (
    Upstream,
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
) {
    let up = Upstream::start(response).await;
    let (addr, stop) = start_relay(vec![upstream_with(
        "openai",
        &up.url(),
        Some(Interface::OpenaiResponses),
        &[("*", "gpt-5.1")],
    )])
    .await;
    (up, addr, stop)
}

#[tokio::test]
async fn claude_to_responses_maps_web_search_calls() {
    let (mut up, addr, _stop) = claude_relay_to_responses(json_response(
        "200 OK",
        &json!({"id": "resp_1", "model": "gpt-5.1", "status": "completed",
            "output": [web_search_item(), answer_item()],
            "usage": {"input_tokens": 30, "output_tokens": 8}}),
    ))
    .await;

    let (status, _, body) = send(addr, &claude_request(false)).await;
    let (_, _, sent) = up.next().await;
    assert_eq!(sent["tools"], json!([{"type": "web_search"}]));
    assert_eq!(sent["max_tool_calls"], 3);

    assert_eq!(status, 200);
    let response = json_value(&body);
    assert_claude_search_blocks(response["content"].as_array().unwrap());
    assert_eq!(response["stop_reason"], "end_turn");
    assert_eq!(
        response["usage"]["server_tool_use"],
        json!({"web_search_requests": 1})
    );
    assert_eq!(response["usage"]["input_tokens"], 30);
}

#[tokio::test]
async fn claude_to_responses_streams_web_search_calls() {
    let stream = sse_of(&[
        (
            "response.created",
            json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5.1", "status": "in_progress", "output": []}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "ws_1", "type": "web_search_call", "status": "in_progress"}}),
        ),
        (
            "response.web_search_call.in_progress",
            json!({"type": "response.web_search_call.in_progress", "output_index": 0, "item_id": "ws_1"}),
        ),
        (
            "response.web_search_call.searching",
            json!({"type": "response.web_search_call.searching", "output_index": 0, "item_id": "ws_1"}),
        ),
        (
            "response.web_search_call.completed",
            json!({"type": "response.web_search_call.completed", "output_index": 0, "item_id": "ws_1"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 0, "item": web_search_item()}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "msg_1", "type": "message", "role": "assistant", "content": []}}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "output_index": 1, "item_id": "msg_1", "content_index": 0, "delta": "Rust "}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "output_index": 1, "item_id": "msg_1", "content_index": 0, "delta": "1.90。"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 1, "item": answer_item()}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_1", "model": "gpt-5.1", "status": "completed",
                "output": [web_search_item(), answer_item()], "usage": {"input_tokens": 30, "output_tokens": 8}}}),
        ),
    ]);
    let (mut up, addr, _stop) = claude_relay_to_responses(sse_response(&stream, 7)).await;

    let (status, _, body) = send(addr, &claude_request(true)).await;
    up.next().await;
    assert_eq!(status, 200);
    let (names, blocks, message_delta) = rebuild_anthropic_stream(&body);
    assert_eq!(names.last().map(String::as_str), Some("message_stop"));
    assert_claude_search_blocks(&blocks);
    assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(
        message_delta["usage"]["server_tool_use"],
        json!({"web_search_requests": 1})
    );
}

// ---------- Codex → Anthropic ----------

fn codex_request(stream: bool) -> Value {
    json!({
        "model": "gpt-5-codex",
        "instructions": "You are Codex.",
        "stream": stream,
        "store": false,
        "tools": [{"type": "web_search"}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "earlier"}]},
            // 历史中的搜索项没有结果内容，无法构造合法的 server_tool_use + 结果配对：丢弃
            {"type": "web_search_call", "id": "ws_old", "status": "completed", "action": {"type": "search", "query": "old"}},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "earlier answer"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "rust 最新版本？"}]}
        ]
    })
}

fn search_results() -> Value {
    json!([{"type": "web_search_result", "url": "https://blog.rust-lang.org", "title": "Rust Blog", "encrypted_content": "enc", "page_age": "2 days ago"}])
}

fn assert_sent_to_anthropic(sent: &Value) {
    assert_eq!(
        sent["tools"],
        json!([{"type": "web_search_20250305", "name": "web_search"}])
    );
    let text = sent["messages"].to_string();
    assert!(
        !text.contains("server_tool_use") && !text.contains("ws_old"),
        "history search items must not produce orphan server_tool_use: {text}"
    );
    assert_eq!(sent["messages"][1]["content"][0]["text"], "earlier answer");
}

fn assert_codex_output(response: &Value) {
    let output = response["output"].as_array().unwrap();
    assert_eq!(output.len(), 2, "{output:?}");
    assert_eq!(
        output[0],
        json!({"id": "ws_01abc", "type": "web_search_call", "status": "completed",
            "action": {"type": "search", "query": "rust latest version"}})
    );
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "Rust 1.90。");
    assert_eq!(response["status"], "completed");
}

async fn codex_relay_to_anthropic(
    response: Vec<Vec<u8>>,
) -> (
    Upstream,
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
) {
    let up = Upstream::start(response).await;
    let (addr, stop) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "anthropic",
            &up.url(),
            Some(Interface::Claude),
            &[("*", "claude-sonnet-4-5")],
        )],
    )
    .await;
    (up, addr, stop)
}

#[tokio::test]
async fn codex_to_anthropic_maps_server_tool_use() {
    let (mut up, addr, _stop) = codex_relay_to_anthropic(json_response(
        "200 OK",
        &json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
            "content": [
                {"type": "server_tool_use", "id": "srvtoolu_01abc", "name": "web_search", "input": {"query": "rust latest version"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_01abc", "content": search_results()},
                {"type": "text", "text": "Rust 1.90。"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 30, "output_tokens": 8, "server_tool_use": {"web_search_requests": 1}}}),
    ))
    .await;

    let (status, _, body) = send_responses(addr, &codex_request(false)).await;
    let (_, _, sent) = up.next().await;
    assert_sent_to_anthropic(&sent);
    assert_eq!(status, 200);
    assert_codex_output(&json_value(&body));
}

#[tokio::test]
async fn codex_to_anthropic_streams_server_tool_use() {
    let stream = sse_of(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-sonnet-4-5", "usage": {"input_tokens": 30}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_01abc", "name": "web_search", "input": {}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": \"rust "}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "latest version\"}"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_01abc", "content": search_results()}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 1}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "Rust 1.90。"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 2}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 8, "server_tool_use": {"web_search_requests": 1}}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let (mut up, addr, _stop) = codex_relay_to_anthropic(sse_response(&stream, 9)).await;

    let (status, _, body) = send_responses(addr, &codex_request(true)).await;
    let (_, _, sent) = up.next().await;
    assert_sent_to_anthropic(&sent);
    assert_eq!(status, 200);
    let (names, response) = parse_responses_stream(&body);
    let search_events: Vec<&str> = names
        .iter()
        .filter(|name| name.starts_with("response.web_search_call."))
        .map(String::as_str)
        .collect();
    assert_eq!(
        search_events,
        [
            "response.web_search_call.in_progress",
            "response.web_search_call.searching",
            "response.web_search_call.completed"
        ]
    );
    assert_codex_output(&response);
}
