//! 端到端：Claude 客户端经中转访问 OpenAI Chat 上游、以及同协议恒等转换
//! （设计文档 §10 X5、X13）。上游与客户端都用原始 TCP 字节。

mod support;

use cc_proxy_core::Interface;
use serde_json::{json, Value};
use support::*;

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

    let (status, _, _) = post(
        addr,
        "/v1/messages/count_tokens",
        &[("x-api-key", TOKEN)],
        &json!({"model": "claude-sonnet-5", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200);
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
