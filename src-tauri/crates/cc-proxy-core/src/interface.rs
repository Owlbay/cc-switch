//! 接口识别（设计文档 §3）。
//!
//! 只读取 method、path 与请求头来决定走哪个接口的上游，不解析 body、不看模型名。

use http::{HeaderMap, Method};
use serde::{Deserialize, Serialize};

/// 中转支持的四个模型接口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interface {
    /// Anthropic Messages（Claude Code）
    Claude,
    /// OpenAI Chat Completions
    OpenaiChat,
    /// OpenAI Responses（Codex CLI）
    OpenaiResponses,
    /// Gemini generateContent / streamGenerateContent（Gemini CLI，API Key 模式）
    Gemini,
}

impl Interface {
    pub const ALL: [Interface; 4] = [
        Interface::Claude,
        Interface::OpenaiChat,
        Interface::OpenaiResponses,
        Interface::Gemini,
    ];

    /// 配置文件与日志中使用的名字
    pub fn as_str(&self) -> &'static str {
        match self {
            Interface::Claude => "claude",
            Interface::OpenaiChat => "openai_chat",
            Interface::OpenaiResponses => "openai_responses",
            Interface::Gemini => "gemini",
        }
    }
}

const ANTHROPIC_VERSION: &str = "anthropic-version";

/// 按 method + path（不含 query）+ 请求头识别接口；不匹配返回 `None`。
///
/// 中转自身的 `/_relay/*` 路径由 server 在调用本函数之前处理，这里总是返回 `None`。
pub fn identify(method: &Method, path: &str, headers: &HeaderMap) -> Option<Interface> {
    if path.starts_with("/_relay/") {
        return None;
    }

    if method == Method::POST && (path == "/v1/messages" || path == "/v1/messages/count_tokens") {
        return Some(Interface::Claude);
    }

    if method == Method::POST && path == "/v1/chat/completions" {
        return Some(Interface::OpenaiChat);
    }

    if (method == Method::POST && path == "/v1/responses") || path.starts_with("/v1/responses/") {
        return Some(Interface::OpenaiResponses);
    }

    if path.starts_with("/v1beta/") {
        return Some(Interface::Gemini);
    }

    if path == "/v1/models" {
        return (method == Method::GET).then(|| models_list_owner(headers));
    }

    if let Some(rest) = path.strip_prefix("/v1/models/") {
        if rest.is_empty() {
            return None;
        }
        // Gemini GA 的动作（generateContent、countTokens 等）都是 POST 且形如 `{name}:{action}`；
        // OpenAI 微调模型 id 也含 `:`（`ft:gpt-4o-mini:org::id`），但查询模型是 GET。
        if method == Method::POST && rest.contains(':') {
            return Some(Interface::Gemini);
        }
        if method == Method::GET {
            return Some(models_list_owner(headers));
        }
    }

    None
}

/// `GET /v1/models` 与 `GET /v1/models/{id}` 同时被 Claude Code（带 `anthropic-version`）
/// 与 Codex CLI 使用，按是否带 `anthropic-version` 头分流。只读头，不改内容。
fn models_list_owner(headers: &HeaderMap) -> Interface {
    if headers.contains_key(ANTHROPIC_VERSION) {
        Interface::Claude
    } else {
        Interface::OpenaiResponses
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn id(method: Method, path: &str) -> Option<Interface> {
        identify(&method, path, &HeaderMap::new())
    }

    fn anthropic_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ANTHROPIC_VERSION, HeaderValue::from_static("2023-06-01"));
        headers
    }

    #[test]
    fn claude_paths() {
        assert_eq!(id(Method::POST, "/v1/messages"), Some(Interface::Claude));
        assert_eq!(
            id(Method::POST, "/v1/messages/count_tokens"),
            Some(Interface::Claude)
        );
        assert_eq!(id(Method::GET, "/v1/messages"), None);
        assert_eq!(id(Method::POST, "/v1/messages/batches"), None);
    }

    #[test]
    fn openai_chat_path() {
        assert_eq!(
            id(Method::POST, "/v1/chat/completions"),
            Some(Interface::OpenaiChat)
        );
        assert_eq!(id(Method::GET, "/v1/chat/completions"), None);
    }

    #[test]
    fn openai_responses_paths() {
        assert_eq!(
            id(Method::POST, "/v1/responses"),
            Some(Interface::OpenaiResponses)
        );
        assert_eq!(
            id(Method::POST, "/v1/responses/compact"),
            Some(Interface::OpenaiResponses)
        );
        assert_eq!(
            id(Method::GET, "/v1/responses/resp_123"),
            Some(Interface::OpenaiResponses)
        );
        assert_eq!(
            id(Method::DELETE, "/v1/responses/resp_123"),
            Some(Interface::OpenaiResponses)
        );
        assert_eq!(id(Method::GET, "/v1/responses"), None);
    }

    #[test]
    fn models_list_is_routed_by_anthropic_version_header() {
        assert_eq!(
            id(Method::GET, "/v1/models"),
            Some(Interface::OpenaiResponses)
        );
        assert_eq!(
            identify(&Method::GET, "/v1/models", &anthropic_headers()),
            Some(Interface::Claude)
        );
        assert_eq!(
            id(Method::GET, "/v1/models/gpt-4o"),
            Some(Interface::OpenaiResponses)
        );
        assert_eq!(
            identify(
                &Method::GET,
                "/v1/models/claude-sonnet-5",
                &anthropic_headers()
            ),
            Some(Interface::Claude)
        );
        assert_eq!(id(Method::POST, "/v1/models"), None);
    }

    #[test]
    fn fine_tuned_model_id_with_colons_is_not_gemini() {
        assert_eq!(
            id(Method::GET, "/v1/models/ft:gpt-4o-mini:org::abc123"),
            Some(Interface::OpenaiResponses)
        );
    }

    #[test]
    fn gemini_paths() {
        assert_eq!(
            id(
                Method::POST,
                "/v1beta/models/gemini-2.5-flash:streamGenerateContent"
            ),
            Some(Interface::Gemini)
        );
        assert_eq!(
            id(Method::GET, "/v1beta/models/gemini-2.5-flash"),
            Some(Interface::Gemini)
        );
        assert_eq!(
            id(Method::POST, "/v1/models/gemini-2.5-flash:generateContent"),
            Some(Interface::Gemini)
        );
        assert_eq!(
            id(Method::POST, "/v1/models/gemini-2.5-flash:countTokens"),
            Some(Interface::Gemini)
        );
    }

    #[test]
    fn excluded_and_unknown_paths() {
        for (method, path) in [
            (Method::GET, "/"),
            (Method::POST, "/v1/alpha/search"),
            (Method::POST, "/v1/images/generations"),
            (Method::POST, "/v1/images/edits"),
            (Method::POST, "/v1/embeddings"),
            (Method::GET, "/v1/models/"),
            (Method::GET, "/_relay/health"),
            (Method::POST, "/v1/v1/messages"),
        ] {
            assert_eq!(id(method.clone(), path), None, "{method} {path}");
        }
    }

    #[test]
    fn as_str_matches_serde_name() {
        for interface in Interface::ALL {
            assert_eq!(
                serde_json::to_value(interface).unwrap(),
                serde_json::Value::String(interface.as_str().to_string())
            );
        }
    }
}
