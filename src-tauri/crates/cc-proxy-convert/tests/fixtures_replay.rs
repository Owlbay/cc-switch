//! 录制回放（设计文档 §10 X3、X4）：四种上游各一段脱敏的真实形态流样本，表达同一段对话
//! （推理 → 文本 → 一次 Bash 工具调用），经两种客户端编码后，按客户端的方式重建出的语义
//! 必须相同，且与上游分片方式无关。

use std::collections::BTreeMap;

use bytes::Bytes;
use cc_proxy_convert::IrConverter;
use cc_proxy_core::convert::{ConversionContext, Converter, ModelMap, OutboundMeta};
use cc_proxy_core::Interface;
use http::{header, HeaderMap, HeaderValue, StatusCode};
use serde_json::{json, Value};

const THINKING: &str = "用户想看目录，先运行 ls。";
const TEXT: &str = "我先看一下当前目录。";

/// 客户端重建出的语义
#[derive(Debug, Clone, PartialEq)]
struct Summary {
    thinking: String,
    text: String,
    tools: Vec<(String, Value)>,
    stop: String,
    input_total: u64,
    cache_read: u64,
    output: u64,
    /// 推理块携带的签名 / encrypted_content（按出现顺序）
    signatures: Vec<String>,
}

fn fixture(upstream: Interface) -> &'static [u8] {
    match upstream {
        Interface::Claude => include_bytes!("fixtures/anthropic_thinking_tool.sse"),
        Interface::OpenaiChat => include_bytes!("fixtures/chat_reasoning_tool.sse"),
        Interface::OpenaiResponses => include_bytes!("fixtures/responses_reasoning_tool.sse"),
        Interface::Gemini => include_bytes!("fixtures/gemini_thought_tool.sse"),
    }
}

/// 把夹具按 `piece` 字节分片喂给响应转换器，返回客户端收到的全部字节
fn replay(client: Interface, upstream: Interface, piece: usize) -> String {
    let map = ModelMap::new();
    let ctx = ConversionContext {
        client,
        upstream,
        model_map: &map,
        default_max_output_tokens: 16384,
    };
    let meta = OutboundMeta {
        stream: true,
        client_model: "client-model".into(),
        upstream_model: "upstream-model".into(),
        tool_names: vec!["Bash".into()],
        ..Default::default()
    };
    let mut converter = IrConverter.response_converter(&ctx, &meta);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    converter.convert_head(StatusCode::OK, &headers);
    let mut out: Vec<Bytes> = Vec::new();
    for chunk in fixture(upstream).chunks(piece) {
        out.extend(converter.feed(chunk).unwrap());
    }
    out.extend(converter.finish(None));
    out.iter()
        .map(|b| String::from_utf8(b.to_vec()).unwrap())
        .collect()
}

fn events(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|e| !e.trim().is_empty())
        .map(|event| {
            let name = event
                .lines()
                .find_map(|l| l.strip_prefix("event: "))
                .unwrap_or_default()
                .to_string();
            let data = event
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .collect::<Vec<_>>()
                .join("\n");
            (name, serde_json::from_str(&data).unwrap())
        })
        .collect()
}

/// Claude Code 的重建方式：按 index 累积各块，message_start / message_delta 取用量
fn summarize_anthropic(body: &str) -> Summary {
    let mut blocks: BTreeMap<u64, Value> = BTreeMap::new();
    let mut partial: BTreeMap<u64, String> = BTreeMap::new();
    let mut start_usage = Value::Null;
    let mut delta = Value::Null;
    let mut last = String::new();
    for (name, data) in events(body) {
        let index = data["index"].as_u64().unwrap_or(0);
        match name.as_str() {
            "message_start" => start_usage = data["message"]["usage"].clone(),
            "content_block_start" => {
                assert!(blocks
                    .insert(index, data["content_block"].clone())
                    .is_none());
            }
            "content_block_delta" => {
                let block = blocks.get_mut(&index).expect("delta after start");
                let d = &data["delta"];
                match d["type"].as_str().unwrap() {
                    "text_delta" => {
                        let t = block["text"].as_str().unwrap().to_string()
                            + d["text"].as_str().unwrap();
                        block["text"] = json!(t);
                    }
                    "thinking_delta" => {
                        let t = block["thinking"].as_str().unwrap().to_string()
                            + d["thinking"].as_str().unwrap();
                        block["thinking"] = json!(t);
                    }
                    "signature_delta" => block["signature"] = d["signature"].clone(),
                    "input_json_delta" => partial
                        .entry(index)
                        .or_default()
                        .push_str(d["partial_json"].as_str().unwrap()),
                    other => panic!("unexpected delta {other}"),
                }
            }
            "content_block_stop" => {
                let block = blocks.get_mut(&index).unwrap();
                if block["type"] == "tool_use" {
                    let raw = partial.remove(&index).unwrap_or_default();
                    if !raw.trim().is_empty() {
                        block["input"] = serde_json::from_str(&raw).unwrap();
                    }
                }
            }
            "message_delta" => delta = data,
            "error" => panic!("unexpected error event: {data}"),
            _ => {}
        }
        last = name;
    }
    assert_eq!(last, "message_stop");
    let usage = |key: &str| {
        delta["usage"][key]
            .as_u64()
            .filter(|n| *n > 0)
            .or_else(|| start_usage[key].as_u64())
            .unwrap_or(0)
    };
    let mut summary = Summary {
        thinking: String::new(),
        text: String::new(),
        tools: Vec::new(),
        stop: delta["delta"]["stop_reason"].as_str().unwrap().to_string(),
        input_total: usage("input_tokens")
            + usage("cache_read_input_tokens")
            + usage("cache_creation_input_tokens"),
        cache_read: usage("cache_read_input_tokens"),
        output: usage("output_tokens"),
        signatures: Vec::new(),
    };
    for block in blocks.values() {
        match block["type"].as_str().unwrap() {
            "thinking" => {
                summary
                    .thinking
                    .push_str(block["thinking"].as_str().unwrap());
                if let Some(s) = block["signature"].as_str().filter(|s| !s.is_empty()) {
                    summary.signatures.push(s.to_string());
                }
            }
            "redacted_thinking" => summary
                .signatures
                .push(block["data"].as_str().unwrap().to_string()),
            "text" => summary.text.push_str(block["text"].as_str().unwrap()),
            "tool_use" => summary.tools.push((
                block["name"].as_str().unwrap().to_string(),
                block["input"].clone(),
            )),
            other => panic!("unexpected block {other}"),
        }
    }
    summary
}

/// Codex 的重建方式：以 response.completed 的 output 与 usage 为准，并校验事件序列自洽
fn summarize_responses(body: &str) -> Summary {
    let mut last_sequence = None;
    let mut completed = Value::Null;
    let mut deltas = String::new();
    for (name, data) in events(body) {
        assert_eq!(data["type"], name.as_str());
        let sequence = data["sequence_number"].as_u64().expect("sequence_number");
        if let Some(last) = last_sequence {
            assert!(sequence > last);
        }
        last_sequence = Some(sequence);
        match name.as_str() {
            "response.output_text.delta" => deltas.push_str(data["delta"].as_str().unwrap()),
            "response.completed" => completed = data["response"].clone(),
            "response.failed" => panic!("unexpected failure: {data}"),
            _ => {}
        }
    }
    assert_eq!(completed["status"], "completed");
    let usage = &completed["usage"];
    let mut summary = Summary {
        thinking: String::new(),
        text: String::new(),
        tools: Vec::new(),
        stop: "completed".into(),
        input_total: usage["input_tokens"].as_u64().unwrap(),
        cache_read: usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
        output: usage["output_tokens"].as_u64().unwrap(),
        signatures: Vec::new(),
    };
    for item in completed["output"].as_array().unwrap() {
        match item["type"].as_str().unwrap() {
            "reasoning" => {
                for part in item["summary"].as_array().unwrap() {
                    summary.thinking.push_str(part["text"].as_str().unwrap());
                }
                if let Some(s) = item["encrypted_content"].as_str() {
                    summary.signatures.push(s.to_string());
                }
            }
            "message" => {
                for part in item["content"].as_array().unwrap() {
                    summary.text.push_str(part["text"].as_str().unwrap());
                }
            }
            "function_call" => summary.tools.push((
                item["name"].as_str().unwrap().to_string(),
                serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap(),
            )),
            other => panic!("unexpected item {other}"),
        }
    }
    assert_eq!(deltas, summary.text, "deltas rebuild the final text");
    summary
}

fn summarize(client: Interface, body: &str) -> Summary {
    match client {
        Interface::Claude => summarize_anthropic(body),
        _ => summarize_responses(body),
    }
}

/// 签名在客户端中的预期形态
fn expected_signature(client: Interface, upstream: Interface) -> Option<&'static str> {
    match (client, upstream) {
        (_, Interface::OpenaiChat) => None,
        (Interface::Claude, Interface::Claude) => {
            Some("EqQBCkgIBxABGAIiQMockSignatureForFixture==")
        }
        (Interface::OpenaiResponses, Interface::OpenaiResponses) => {
            Some("gAAAAABo2MockEncryptedContentForFixture")
        }
        (_, Interface::Claude) => Some("ccsw1.claude."),
        (_, Interface::OpenaiResponses) => Some("ccsw1.openai_responses."),
        (_, Interface::Gemini) => Some("ccsw1.gemini."),
    }
}

#[test]
fn every_upstream_fixture_rebuilds_the_same_conversation_for_every_client() {
    for client in [Interface::Claude, Interface::OpenaiResponses] {
        for upstream in Interface::ALL {
            let whole = summarize(client, &replay(client, upstream, usize::MAX));
            let context = format!("{} -> {}", upstream.as_str(), client.as_str());

            assert_eq!(whole.thinking, THINKING, "{context}");
            assert_eq!(whole.text, TEXT, "{context}");
            assert_eq!(
                whole.tools,
                vec![(
                    "Bash".to_string(),
                    json!({"command": "ls -la", "description": "列出文件"})
                )],
                "{context}"
            );
            let stop = if client == Interface::Claude {
                "tool_use"
            } else {
                "completed"
            };
            assert_eq!(whole.stop, stop, "{context}");
            assert_eq!(whole.input_total, 21594, "{context}");
            assert_eq!(whole.cache_read, 18342, "{context}");
            assert_eq!(whole.output, 87, "{context}");
            match expected_signature(client, upstream) {
                None => assert!(
                    whole.signatures.is_empty(),
                    "{context}: {:?}",
                    whole.signatures
                ),
                Some(expected) => {
                    assert_eq!(
                        whole.signatures.len(),
                        1,
                        "{context}: {:?}",
                        whole.signatures
                    );
                    assert!(
                        whole.signatures[0].starts_with(expected),
                        "{context}: {}",
                        whole.signatures[0]
                    );
                }
            }

            // 分片无关：任意切分（含把多字节字符与 \n\n 切开）重建结果相同
            for piece in [1, 2, 3, 7, 64] {
                let split = summarize(client, &replay(client, upstream, piece));
                // Gemini 调用签名封套内含生成的调用 id，每次解码不同：只比较封套前缀
                let mut split = split;
                let mut whole = whole.clone();
                if upstream == Interface::Gemini {
                    for s in split
                        .signatures
                        .iter_mut()
                        .chain(whole.signatures.iter_mut())
                    {
                        s.truncate("ccsw1.gemini.".len());
                    }
                }
                assert_eq!(split, whole, "{context} piece={piece}");
            }
        }
    }
}
