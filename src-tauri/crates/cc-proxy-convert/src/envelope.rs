//! 签名封套（设计文档 §5.5）。
//!
//! 上游要求原样回放的不透明值（Anthropic thinking 签名、Responses `encrypted_content`、
//! Gemini `thoughtSignature`）只对产生它的上游有意义。中转不保存状态，跨协议时把原始值
//! 封装进客户端协议的签名字段，由客户端下一轮原样带回：
//!
//! ```text
//! ccsw1.<src>.<base64url(JSON)>
//! ```
//!
//! 解码只接受版本、来源与载荷结构都正确的封套；其余一律视为无效（按“缺签名”降级）。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use cc_proxy_core::Interface;
use serde_json::{json, Value};

use crate::ir::Signature;

const VERSION: &str = "ccsw1";

/// 把签名封装为字符串
pub fn encode(signature: &Signature) -> String {
    let payload = match signature.source {
        Interface::Claude => json!({ "block": signature.value }),
        Interface::OpenaiResponses => json!({ "item": signature.value }),
        Interface::Gemini => signature.value.clone(),
        // Chat 不产生需要回放的签名；为完整起见按原值封装
        Interface::OpenaiChat => json!({ "value": signature.value }),
    };
    format!(
        "{VERSION}.{}.{}",
        signature.source.as_str(),
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

/// 是否形如封套（用于区分客户端直接带回的原始签名）
pub fn looks_like(value: &str) -> bool {
    value.starts_with("ccsw")
}

/// 解码封套；版本、来源或载荷结构不对时返回 None
pub fn decode(value: &str) -> Option<Signature> {
    let mut parts = value.splitn(3, '.');
    if parts.next()? != VERSION {
        return None;
    }
    let source = match parts.next()? {
        "claude" => Interface::Claude,
        "openai_responses" => Interface::OpenaiResponses,
        "gemini" => Interface::Gemini,
        _ => return None,
    };
    let bytes = URL_SAFE_NO_PAD.decode(parts.next()?).ok()?;
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    let value = match source {
        Interface::Claude => {
            let block = payload.get("block")?;
            let kind = block.get("type")?.as_str()?;
            let valid = match kind {
                "thinking" => block.get("signature")?.as_str().is_some(),
                "redacted_thinking" => block.get("data")?.as_str().is_some(),
                _ => false,
            };
            valid.then(|| block.clone())?
        }
        Interface::OpenaiResponses => {
            let item = payload.get("item")?;
            (item.get("type")?.as_str()? == "reasoning").then(|| item.clone())?
        }
        Interface::Gemini => {
            payload.get("sig")?.as_str()?;
            let has_call = payload.get("call_id").and_then(Value::as_str).is_some();
            let is_text = payload.get("text").and_then(Value::as_bool) == Some(true);
            (has_call || is_text).then_some(payload)?
        }
        Interface::OpenaiChat => return None,
    };
    Some(Signature { source, value })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(signature: Signature) {
        let encoded = encode(&signature);
        assert!(encoded.starts_with("ccsw1."), "{encoded}");
        assert!(looks_like(&encoded));
        assert!(!encoded.contains('='), "base64url without padding");
        assert_eq!(decode(&encoded), Some(signature));
    }

    #[test]
    fn roundtrips_each_source() {
        roundtrip(Signature {
            source: Interface::Claude,
            value: json!({"type": "thinking", "thinking": "hmm", "signature": "EqQB..."}),
        });
        roundtrip(Signature {
            source: Interface::Claude,
            value: json!({"type": "redacted_thinking", "data": "opaque"}),
        });
        roundtrip(Signature {
            source: Interface::OpenaiResponses,
            value: json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAA"}),
        });
        roundtrip(Signature {
            source: Interface::Gemini,
            value: json!({"sig": "CiQB", "call_id": "call_1"}),
        });
        roundtrip(Signature {
            source: Interface::Gemini,
            value: json!({"sig": "CiQB", "text": true}),
        });
    }

    #[test]
    fn rejects_malformed_envelopes() {
        let valid = encode(&Signature {
            source: Interface::Claude,
            value: json!({"type": "thinking", "thinking": "", "signature": "s"}),
        });
        let body = valid.rsplit('.').next().unwrap();
        for bad in [
            "".to_string(),
            "EqQBsignature".to_string(),
            format!("ccsw2.claude.{body}"),
            format!("ccsw1h.claude.{body}"),
            format!("ccsw1.openai_chat.{body}"),
            format!("ccsw1.unknown.{body}"),
            "ccsw1.claude.!!!".to_string(),
            format!("ccsw1.claude.{}", URL_SAFE_NO_PAD.encode("not json")),
            // 来源与载荷结构不符
            format!("ccsw1.openai_responses.{body}"),
            format!(
                "ccsw1.claude.{}",
                URL_SAFE_NO_PAD.encode(json!({"block": {"type": "text"}}).to_string())
            ),
            format!(
                "ccsw1.gemini.{}",
                URL_SAFE_NO_PAD.encode(json!({"sig": "x"}).to_string())
            ),
        ] {
            assert_eq!(decode(&bad), None, "{bad}");
        }
    }
}
