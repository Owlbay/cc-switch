//! 端到端：Claude / Codex 客户端经中转访问 Gemini 上游（设计文档 §10 X5、X11、X14）。

mod support;

use cc_proxy_core::Interface;
use serde_json::{json, Value};
use support::*;

fn gemini_upstream(url: &str) -> cc_proxy_core::config::UpstreamConfig {
    upstream_with(
        "google",
        url,
        Some(Interface::Gemini),
        &[("*", "gemini-3-pro")],
    )
}

fn data_stream(chunks: &[Value]) -> String {
    chunks.iter().map(|c| format!("data: {c}\n\n")).collect()
}

#[tokio::test]
async fn claude_to_gemini_streams_and_replays_the_call_signature() {
    let stream = data_stream(&[
        json!({"responseId": "r1", "modelVersion": "gemini-3-pro", "candidates": [{"content": {"role": "model", "parts": [{"text": "look", "thought": true}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Checking."}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"functionCall": {"name": "Bash", "args": {"command": "ls"}}, "thoughtSignature": "SIG-1"}]}, "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 60, "candidatesTokenCount": 10, "thoughtsTokenCount": 5, "totalTokenCount": 115}}),
    ]);
    let mut up = Upstream::start(sse_response(&stream, 17)).await;
    let (addr, _stop) = start_relay(vec![gemini_upstream(&up.url())]).await;

    let request = json!({
        "model": "claude-sonnet-5", "max_tokens": 4096, "stream": true,
        "system": "You are Claude Code.",
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "tools": [{"name": "Bash", "description": "run", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}}],
        "messages": [{"role": "user", "content": "list files"}]
    });
    let (status, _, body) = send(addr, &request).await;
    let (target, headers, sent) = up.next().await;
    assert_eq!(
        target,
        "/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse"
    );
    assert_eq!(header(&headers, "x-goog-api-key"), Some(KEY));
    assert!(header(&headers, "anthropic-version").is_none());
    assert!(sent.get("model").is_none());
    assert_eq!(
        sent["systemInstruction"]["parts"][0]["text"],
        "You are Claude Code."
    );
    assert_eq!(sent["generationConfig"]["maxOutputTokens"], 4096);
    assert_eq!(
        sent["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        2048
    );
    assert_eq!(sent["tools"][0]["functionDeclarations"][0]["name"], "Bash");

    assert_eq!(status, 200);
    let (names, blocks, message_delta) = rebuild_anthropic_stream(&body);
    assert_eq!(names.last().map(String::as_str), Some("message_stop"));
    let kinds: Vec<&str> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["thinking", "text", "redacted_thinking", "tool_use"]);
    assert_eq!(blocks[0]["thinking"], "look");
    assert_eq!(blocks[1]["text"], "Checking.");
    let envelope = blocks[2]["data"].as_str().unwrap().to_string();
    assert!(envelope.starts_with("ccsw1.gemini."), "{envelope}");
    assert_eq!(blocks[3]["name"], "Bash");
    assert_eq!(blocks[3]["input"], json!({"command": "ls"}));
    assert_eq!(message_delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(message_delta["usage"]["input_tokens"], 40);
    assert_eq!(message_delta["usage"]["cache_read_input_tokens"], 60);
    assert_eq!(message_delta["usage"]["output_tokens"], 15);

    // 第二轮：客户端带回 redacted_thinking 封套与工具结果，签名回到 functionCall part
    let mut up2 = Upstream::start(json_response(
        "200 OK",
        &json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "a.txt"}]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "totalTokenCount": 7}}),
    ))
    .await;
    let (addr2, _stop2) = start_relay(vec![gemini_upstream(&up2.url())]).await;
    let tool_use = blocks[3].clone();
    let second = json!({
        "model": "claude-sonnet-5", "max_tokens": 4096,
        "tools": request["tools"].clone(),
        "messages": [
            {"role": "user", "content": "list files"},
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": envelope},
                {"type": "text", "text": "Checking."},
                tool_use
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use["id"].clone(), "content": "a.txt"}]}
        ]
    });
    let (status, _, body) = send(addr2, &second).await;
    let (target, _, sent) = up2.next().await;
    assert_eq!(target, "/v1beta/models/gemini-3-pro:generateContent");
    assert_eq!(status, 200);
    let model_turn = &sent["contents"][1];
    assert_eq!(model_turn["role"], "model");
    let call = model_turn["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p.get("functionCall").is_some())
        .expect("functionCall part");
    assert_eq!(call["thoughtSignature"], "SIG-1");
    assert_eq!(call["functionCall"]["name"], "Bash");
    let response_part = &sent["contents"][2]["parts"][0]["functionResponse"];
    assert_eq!(response_part["name"], "Bash");
    assert_eq!(response_part["response"], json!({"content": "a.txt"}));
    assert_eq!(json_value(&body)["content"][0]["text"], "a.txt");
}

#[tokio::test]
async fn codex_to_gemini_non_streaming_with_custom_tool() {
    let mut up = Upstream::start(json_response(
        "200 OK",
        &json!({"responseId": "r9", "candidates": [{"content": {"role": "model", "parts": [
                    {"functionCall": {"name": "apply_patch", "args": {"input": "*** Begin Patch"}}}
                ]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 3, "totalTokenCount": 15}}),
    ))
    .await;
    let (addr, _stop) =
        start_relay_for(Interface::OpenaiResponses, vec![gemini_upstream(&up.url())]).await;
    let request = json!({
        "model": "gpt-5-codex", "stream": false, "store": false,
        "instructions": "You are Codex.",
        "tools": [{"type": "custom", "name": "apply_patch", "description": "patch"}],
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix"}]}]
    });
    let (status, _, body) = send_responses(addr, &request).await;
    let (target, _, sent) = up.next().await;
    assert_eq!(target, "/v1beta/models/gemini-3-pro:generateContent");
    assert_eq!(sent["generationConfig"]["maxOutputTokens"], 16384);
    assert_eq!(status, 200);
    let response = json_value(&body);
    assert_eq!(response["model"], "gpt-5-codex");
    assert_eq!(response["output"][0]["type"], "custom_tool_call");
    assert_eq!(response["output"][0]["input"], "*** Begin Patch");
    assert_eq!(response["usage"]["input_tokens"], 12);
}

#[tokio::test]
async fn gemini_errors_and_blocked_prompts() {
    let mut up = Upstream::start(json_response(
        "400 Bad Request",
        &json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": "bad schema"}}),
    ))
    .await;
    let (addr, _stop) = start_relay(vec![gemini_upstream(&up.url())]).await;
    let request = json!({"model": "claude-sonnet-5", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]});
    let (status, _, body) = send(addr, &request).await;
    up.next().await;
    assert_eq!(status, 400);
    assert_eq!(
        json_value(&body),
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad schema"}})
    );

    let mut blocked = Upstream::start(json_response(
        "200 OK",
        &json!({"promptFeedback": {"blockReason": "SAFETY"}, "usageMetadata": {"promptTokenCount": 4}}),
    ))
    .await;
    let (addr, _stop) = start_relay(vec![gemini_upstream(&blocked.url())]).await;
    let (status, _, body) = send(addr, &request).await;
    blocked.next().await;
    assert_eq!(status, 200);
    assert_eq!(json_value(&body)["stop_reason"], "refusal");
}

#[tokio::test]
async fn image_urls_skip_the_gemini_upstream() {
    let mut up = Upstream::start(json_response("200 OK", &json!({}))).await;
    let (addr, _stop) = start_relay(vec![gemini_upstream(&up.url())]).await;
    let request = json!({"model": "claude-sonnet-5", "max_tokens": 10, "messages": [{"role": "user", "content": [
        {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}}
    ]}]});
    let (status, _, body) = send(addr, &request).await;
    assert_eq!(status, 400);
    assert_eq!(
        json_value(&body)["error"]["type"],
        "relay_conversion_unsupported"
    );
    up.assert_idle().await;
    let _: Value = json_value(&body);
}
