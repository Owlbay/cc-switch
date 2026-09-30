//! 端到端：Codex（Responses）客户端经中转访问 Chat / Anthropic / Responses 上游，Claude 客户端
//! 访问 Responses 上游，以及签名跨协议往返（设计文档 §10 X5、X6、X11、X13、X14）。

mod support;

use cc_proxy_core::Interface;
use serde_json::{json, Value};
use support::*;

fn codex_request(stream: bool) -> Value {
    json!({
        "model": "gpt-5-codex",
        "instructions": "You are Codex.",
        "stream": stream,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "session-1",
        "reasoning": {"effort": "high", "summary": "auto"},
        "tools": [
            {"type": "function", "name": "shell", "description": "run", "parameters": {"type": "object", "properties": {"command": {"type": "array", "items": {"type": "string"}}}}},
            {"type": "custom", "name": "apply_patch", "description": "patch", "format": {"type": "grammar"}}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Be brief."}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix the bug"}]}
        ]
    })
}

fn sse_of(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect()
}

fn output_types(response: &Value) -> Vec<String> {
    response["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap().to_string())
        .collect()
}

// ---------- Codex → Chat ----------

#[tokio::test]
async fn codex_to_chat_restores_custom_tool_calls() {
    let mut up = Upstream::start(json_response(
        "200 OK",
        &json!({
            "id": "chatcmpl-1", "model": "deepseek-chat",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "call_7", "type": "function", "function": {"name": "apply_patch", "arguments": "{\"input\":\"*** Begin Patch\"}"}}]
            }}],
            "usage": {"prompt_tokens": 50, "completion_tokens": 9, "prompt_tokens_details": {"cached_tokens": 40}}
        }),
    ))
    .await;
    let (addr, _stop) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "chat",
            &up.url(),
            Some(Interface::OpenaiChat),
            &[("*", "deepseek-chat")],
        )],
    )
    .await;

    let (status, headers, body) = send_responses(addr, &codex_request(false)).await;
    let (target, up_headers, sent) = up.next().await;
    assert_eq!(target, "/v1/chat/completions");
    assert_eq!(
        header(&up_headers, "authorization"),
        Some("Bearer sk-upstream")
    );
    assert!(header(&up_headers, "openai-beta").is_none());
    assert_eq!(header(&up_headers, "originator"), Some("codex_cli_rs"));
    assert_eq!(sent["model"], "deepseek-chat");
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["max_tokens"], 16384, "default_max_output_tokens");
    let tool_names: Vec<&str> = sent["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, ["shell", "apply_patch"]);
    assert_eq!(
        sent["tools"][1]["function"]["parameters"]["properties"]["input"]["type"],
        "string"
    );

    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    let response = json_value(&body);
    assert_eq!(response["model"], "gpt-5-codex");
    assert_eq!(response["status"], "completed");
    assert_eq!(response["output"][0]["type"], "custom_tool_call");
    assert_eq!(response["output"][0]["call_id"], "call_7");
    assert_eq!(response["output"][0]["input"], "*** Begin Patch");
    assert_eq!(response["usage"]["input_tokens"], 50);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        40
    );
}

#[tokio::test]
async fn codex_to_chat_streaming() {
    let chunks = [
        r#"{"id":"c1","model":"deepseek-chat","choices":[{"index":0,"delta":{"role":"assistant","content":"Looking"}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{"content":" now."}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"command\":"}}]}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"[\"ls\"]}"}}]}}]}"#,
        r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":30,"completion_tokens":7}}"#,
    ];
    let events: String = chunks
        .iter()
        .map(|c| format!("data: {c}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let mut up = Upstream::start(sse_response(&events, 9)).await;
    let (addr, _stop) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "chat",
            &up.url(),
            Some(Interface::OpenaiChat),
            &[],
        )],
    )
    .await;

    let (status, headers, body) = send_responses(addr, &codex_request(true)).await;
    up.next().await;
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
    let (names, response) = parse_responses_stream(&body);
    assert_eq!(names.first().map(String::as_str), Some("response.created"));
    assert_eq!(names.last().map(String::as_str), Some("response.completed"));
    assert!(names.contains(&"response.output_text.delta".to_string()));
    assert!(names.contains(&"response.function_call_arguments.delta".to_string()));
    assert_eq!(output_types(&response), ["message", "function_call"]);
    assert_eq!(response["output"][0]["content"][0]["text"], "Looking now.");
    assert_eq!(response["output"][1]["arguments"], "{\"command\":[\"ls\"]}");
    assert_eq!(response["usage"]["input_tokens"], 30);
    assert_eq!(response["usage"]["output_tokens"], 7);
}

#[tokio::test]
async fn chat_errors_come_back_in_responses_format() {
    let mut up = Upstream::start(json_response(
        "429 Too Many Requests",
        &json!({"error": {"message": "slow down", "type": "rate_limit"}}),
    ))
    .await;
    let (addr, _stop) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "chat",
            &up.url(),
            Some(Interface::OpenaiChat),
            &[],
        )],
    )
    .await;
    let (status, _, body) = send_responses(addr, &codex_request(true)).await;
    up.next().await;
    assert_eq!(status, 429);
    let error = json_value(&body);
    assert_eq!(error["error"]["message"], "slow down");
    assert_eq!(error["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn stateful_requests_are_rejected_without_reaching_the_upstream() {
    let mut up = Upstream::start(json_response("200 OK", &json!({}))).await;
    let (addr, _stop) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "chat",
            &up.url(),
            Some(Interface::OpenaiChat),
            &[],
        )],
    )
    .await;
    let mut request = codex_request(false);
    request["previous_response_id"] = json!("resp_1");
    let (status, _, body) = send_responses(addr, &request).await;
    assert_eq!(status, 400);
    assert_eq!(
        json_value(&body)["error"]["type"],
        "relay_conversion_unsupported"
    );
    up.assert_idle().await;
}

// ---------- Codex → Anthropic，签名往返 ----------

#[tokio::test]
async fn codex_to_anthropic_streams_and_replays_thinking() {
    let stream = sse_of(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-sonnet-4-5", "usage": {"input_tokens": 20, "cache_read_input_tokens": 5}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "inspect"}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-abc"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":[\"ls\"]}"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 1}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 12}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let mut up = Upstream::start(sse_response(&stream, 11)).await;
    let (addr, _stop) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "anthropic",
            &up.url(),
            Some(Interface::Claude),
            &[("*", "claude-sonnet-4-5")],
        )],
    )
    .await;

    let (status, _, body) = send_responses(addr, &codex_request(true)).await;
    let (target, up_headers, sent) = up.next().await;
    assert_eq!(target, "/v1/messages");
    assert_eq!(header(&up_headers, "x-api-key"), Some(KEY));
    assert_eq!(header(&up_headers, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(sent["max_tokens"], 16384);
    assert_eq!(sent["system"][0]["text"], "You are Codex.");
    assert!(sent.get("store").is_none() && sent.get("include").is_none());

    assert_eq!(status, 200);
    let (_, response) = parse_responses_stream(&body);
    assert_eq!(output_types(&response), ["reasoning", "function_call"]);
    let reasoning = response["output"][0].clone();
    let envelope = reasoning["encrypted_content"].as_str().unwrap().to_string();
    assert!(envelope.starts_with("ccsw1.claude."), "{envelope}");
    assert_eq!(reasoning["summary"][0]["text"], "inspect");
    assert_eq!(response["usage"]["input_tokens"], 25);

    // 第二轮：Codex 原样带回 reasoning 项与工具结果，Anthropic 上游收到原始签名
    let mut up2 = Upstream::start(json_response(
        "200 OK",
        &json!({"id": "msg_2", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1}}),
    ))
    .await;
    let (addr2, _stop2) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "anthropic",
            &up2.url(),
            Some(Interface::Claude),
            &[("*", "claude-sonnet-4-5")],
        )],
    )
    .await;
    let mut second = codex_request(false);
    let input = second["input"].as_array_mut().unwrap();
    input.push(reasoning);
    input.push(response["output"][1].clone());
    input.push(json!({"type": "function_call_output", "call_id": "toolu_1", "output": "a.txt"}));
    let (status, _, body) = send_responses(addr2, &second).await;
    let (_, _, sent) = up2.next().await;
    assert_eq!(status, 200);
    let assistant = &sent["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(
        assistant["content"][0],
        json!({"type": "thinking", "thinking": "inspect", "signature": "sig-abc"})
    );
    assert_eq!(assistant["content"][1]["type"], "tool_use");
    assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(
        sent["thinking"]["type"], "enabled",
        "signed thinking keeps thinking on"
    );
    assert_eq!(json_value(&body)["output"][0]["content"][0]["text"], "done");
}

// ---------- Claude → Responses，签名往返 ----------

#[tokio::test]
async fn claude_to_responses_streams_and_replays_reasoning() {
    let reasoning_item = json!({"id": "rs_9", "type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}], "encrypted_content": "gAAAA-secret"});
    let stream = sse_of(&[
        (
            "response.created",
            json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5.1", "status": "in_progress", "output": []}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "rs_9", "type": "reasoning", "summary": []}}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "item_id": "rs_9", "summary_index": 0, "delta": "plan"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "msg_1", "type": "message", "role": "assistant", "content": []}}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "output_index": 1, "item_id": "msg_1", "content_index": 0, "delta": "你好"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"id": "msg_1", "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "你好"}]}}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_1", "model": "gpt-5.1", "status": "completed", "output": [], "usage": {"input_tokens": 40, "input_tokens_details": {"cached_tokens": 30}, "output_tokens": 5}}}),
        ),
    ]);
    let mut up = Upstream::start(sse_response(&stream, 5)).await;
    let (addr, _stop) = start_relay(vec![upstream_with(
        "openai",
        &up.url(),
        Some(Interface::OpenaiResponses),
        &[("*", "gpt-5.1")],
    )])
    .await;

    let request = json!({
        "model": "claude-sonnet-5", "max_tokens": 8000, "stream": true,
        "system": [{"type": "text", "text": "x-anthropic-billing-header: cch=1\n\nYou are Claude Code."}],
        "thinking": {"type": "enabled", "budget_tokens": 4000},
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (status, _, body) = send(addr, &request).await;
    let (target, up_headers, sent) = up.next().await;
    assert_eq!(target, "/v1/responses");
    assert_eq!(
        header(&up_headers, "authorization"),
        Some("Bearer sk-upstream")
    );
    assert!(header(&up_headers, "anthropic-version").is_none());
    assert_eq!(sent["store"], false);
    assert_eq!(sent["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(sent["instructions"], "You are Claude Code.");
    assert_eq!(sent["max_output_tokens"], 8000);
    assert!(sent["reasoning"]["effort"].is_string());

    assert_eq!(status, 200);
    let (names, blocks, message_delta) = rebuild_anthropic_stream(&body);
    assert_eq!(names.last().map(String::as_str), Some("message_stop"));
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["thinking"], "plan");
    assert_eq!(blocks[1], json!({"type": "text", "text": "你好"}));
    assert_eq!(message_delta["usage"]["input_tokens"], 10);
    assert_eq!(message_delta["usage"]["cache_read_input_tokens"], 30);

    // 签名在 signature_delta 中：取出后作为第二轮历史带回
    let text = String::from_utf8(body).unwrap();
    let signature = text
        .split("\n\n")
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("data: ")))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find_map(|v| {
            (v["delta"]["type"] == "signature_delta")
                .then(|| v["delta"]["signature"].as_str().unwrap().to_string())
        })
        .expect("signature_delta");
    assert!(
        signature.starts_with("ccsw1.openai_responses."),
        "{signature}"
    );

    let mut up2 = Upstream::start(json_response(
        "200 OK",
        &json!({"id": "resp_2", "model": "gpt-5.1", "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1}}),
    ))
    .await;
    let (addr2, _stop2) = start_relay(vec![upstream_with(
        "openai",
        &up2.url(),
        Some(Interface::OpenaiResponses),
        &[("*", "gpt-5.1")],
    )])
    .await;
    let second = json!({
        "model": "claude-sonnet-5", "max_tokens": 8000,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan", "signature": signature},
                {"type": "text", "text": "你好"}
            ]},
            {"role": "user", "content": "again"}
        ]
    });
    let (status, _, body) = send(addr2, &second).await;
    let (_, _, sent) = up2.next().await;
    assert_eq!(status, 200);
    let input = sent["input"].as_array().unwrap();
    let replayed = input
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("reasoning item replayed");
    assert_eq!(replayed["encrypted_content"], "gAAAA-secret");
    assert_eq!(replayed["id"], "rs_9");
    assert_eq!(json_value(&body)["content"][0]["text"], "ok");
}

// ---------- Responses 恒等 ----------

#[tokio::test]
async fn responses_identity_through_the_relay() {
    let stream = sse_of(&[
        (
            "response.created",
            json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5.1-codex"}}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": "hi"}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_1", "model": "gpt-5.1-codex", "status": "completed"}}),
        ),
    ]);
    let mut up = Upstream::start(sse_response(&stream, 13)).await;
    let (addr, _stop) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "openai",
            &up.url(),
            None,
            &[("gpt-5-codex", "gpt-5.1-codex")],
        )],
    )
    .await;

    let request = codex_request(true);
    let (status, _, body) = send_responses(addr, &request).await;
    let (target, headers, sent) = up.next().await;
    assert_eq!(target, "/v1/responses");
    assert_eq!(
        header(&headers, "openai-beta"),
        Some("responses=experimental")
    );
    let mut expected = request.clone();
    expected["model"] = json!("gpt-5.1-codex");
    assert_eq!(sent, expected, "only the model may change");
    assert_eq!(status, 200);
    let text = String::from_utf8(body).unwrap();
    assert_eq!(
        text.matches("\"model\":\"gpt-5-codex\"").count(),
        2,
        "{text}"
    );
}

#[tokio::test]
async fn codex_streaming_request_answered_with_json_becomes_a_responses_stream() {
    let mut up = Upstream::start(json_response(
        "200 OK",
        &json!({"id": "chatcmpl-9", "choices": [{"index": 0, "finish_reason": "length",
            "message": {"role": "assistant", "content": "partial"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}}),
    ))
    .await;
    let (addr, _stop) = start_relay_for(
        Interface::OpenaiResponses,
        vec![upstream_with(
            "chat",
            &up.url(),
            Some(Interface::OpenaiChat),
            &[],
        )],
    )
    .await;
    let (status, headers, body) = send_responses(addr, &codex_request(true)).await;
    up.next().await;
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
    let (names, response) = parse_responses_stream(&body);
    assert_eq!(names.last().map(String::as_str), Some("response.completed"));
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    assert_eq!(response["output"][0]["content"][0]["text"], "partial");
}
