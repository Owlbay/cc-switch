//! 入站鉴权（设计文档 §6）。
//!
//! 客户端按各自协议的习惯携带中转 token：`x-api-key`、`authorization: Bearer`、
//! `x-goog-api-key` 或 Gemini 的 `?key=`。四处都收集为候选值，任一候选与任一配置的
//! token 相等即通过；比较使用常量时间，且不提前退出，避免依赖头的顺序。
//! 凭据的移除由 `headers::upstream_request_headers` 与 `upstream_url::strip_key_param` 完成。

use http::header::{self, HeaderMap};
use subtle::ConstantTimeEq;

use crate::config::{Secret, ServerConfig};
use crate::upstream_url::key_param_values;

/// 入站鉴权结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    /// token 匹配
    Allowed,
    /// 匿名模式（`allow_anonymous = true` 且未配置 token）
    Anonymous,
    /// 没有匹配的 token
    Denied,
}

impl AuthDecision {
    pub fn is_allowed(self) -> bool {
        !matches!(self, AuthDecision::Denied)
    }
}

/// 入站鉴权器
#[derive(Clone)]
pub struct InboundAuth {
    tokens: Vec<Secret>,
    allow_anonymous: bool,
}

impl InboundAuth {
    pub fn new(tokens: Vec<Secret>, allow_anonymous: bool) -> Self {
        Self {
            tokens,
            allow_anonymous,
        }
    }

    pub fn from_config(server: &ServerConfig) -> Self {
        Self::new(server.auth_tokens.clone(), server.allow_anonymous)
    }

    /// 按请求头与原始 query（不含 `?`）做鉴权判断。
    pub fn check(&self, headers: &HeaderMap, query: Option<&str>) -> AuthDecision {
        if self.tokens.is_empty() {
            return if self.allow_anonymous {
                AuthDecision::Anonymous
            } else {
                AuthDecision::Denied
            };
        }

        let mut matched = subtle::Choice::from(0u8);
        for candidate in candidates(headers, query) {
            for token in &self.tokens {
                // 长度不等时 `ct_eq` 自行返回 0
                matched |= candidate.ct_eq(token.expose().as_bytes());
            }
        }
        if bool::from(matched) {
            AuthDecision::Allowed
        } else {
            AuthDecision::Denied
        }
    }
}

/// 收集四处候选值（原始字节，不做解码）。
///
/// `authorization` 只取 `Bearer` scheme（大小写不敏感）之后的值；其它 scheme 不算候选，
/// 但同样会被移除，不会转发给上游。
pub fn candidates<'a>(headers: &'a HeaderMap, query: Option<&'a str>) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    for value in headers.get_all("x-api-key") {
        out.push(value.as_bytes());
    }
    for value in headers.get_all(header::AUTHORIZATION) {
        if let Some(token) = bearer_token(value.as_bytes()) {
            out.push(token);
        }
    }
    for value in headers.get_all("x-goog-api-key") {
        out.push(value.as_bytes());
    }
    if let Some(query) = query {
        out.extend(key_param_values(query).map(str::as_bytes));
    }
    out
}

fn bearer_token(value: &[u8]) -> Option<&[u8]> {
    const SCHEME: &[u8] = b"bearer ";
    if value.len() < SCHEME.len() || !value[..SCHEME.len()].eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    Some(value[SCHEME.len()..].trim_ascii())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn auth() -> InboundAuth {
        InboundAuth::new(vec![Secret::new("relay-a"), Secret::new("relay-b")], false)
    }

    #[test]
    fn each_carrier_is_accepted() {
        let auth = auth();
        assert_eq!(
            auth.check(&headers(&[("x-api-key", "relay-a")]), None),
            AuthDecision::Allowed
        );
        assert_eq!(
            auth.check(&headers(&[("authorization", "Bearer relay-b")]), None),
            AuthDecision::Allowed
        );
        assert_eq!(
            auth.check(&headers(&[("authorization", "bearer  relay-a ")]), None),
            AuthDecision::Allowed
        );
        assert_eq!(
            auth.check(&headers(&[("x-goog-api-key", "relay-a")]), None),
            AuthDecision::Allowed
        );
        assert_eq!(
            auth.check(&HeaderMap::new(), Some("alt=sse&key=relay-b")),
            AuthDecision::Allowed
        );
    }

    #[test]
    fn any_matching_candidate_wins_regardless_of_order() {
        let auth = auth();
        let both = headers(&[("x-api-key", "relay-a"), ("authorization", "Bearer wrong")]);
        assert_eq!(auth.check(&both, None), AuthDecision::Allowed);
        let reversed = headers(&[("x-api-key", "wrong"), ("authorization", "Bearer relay-a")]);
        assert_eq!(auth.check(&reversed, None), AuthDecision::Allowed);
    }

    #[test]
    fn wrong_missing_or_non_bearer_is_denied() {
        let auth = auth();
        assert_eq!(auth.check(&HeaderMap::new(), None), AuthDecision::Denied);
        assert_eq!(
            auth.check(&headers(&[("x-api-key", "relay-a2")]), None),
            AuthDecision::Denied
        );
        assert_eq!(
            auth.check(&headers(&[("x-api-key", "relay")]), None),
            AuthDecision::Denied
        );
        assert_eq!(
            auth.check(&headers(&[("authorization", "Basic relay-a")]), None),
            AuthDecision::Denied
        );
        assert_eq!(
            auth.check(&headers(&[("authorization", "relay-a")]), None),
            AuthDecision::Denied
        );
        // query 中的 key 按原始字节比较，不做解码
        assert_eq!(
            auth.check(&HeaderMap::new(), Some("keys=relay-a&apikey=relay-a")),
            AuthDecision::Denied
        );
    }

    #[test]
    fn anonymous_rules() {
        let anonymous = InboundAuth::new(Vec::new(), true);
        assert_eq!(
            anonymous.check(&HeaderMap::new(), None),
            AuthDecision::Anonymous
        );
        assert!(AuthDecision::Anonymous.is_allowed());

        let closed = InboundAuth::new(Vec::new(), false);
        assert_eq!(closed.check(&HeaderMap::new(), None), AuthDecision::Denied);

        // 配置了 token 时 allow_anonymous 不生效
        let tokens_win = InboundAuth::new(vec![Secret::new("t")], true);
        assert_eq!(
            tokens_win.check(&HeaderMap::new(), None),
            AuthDecision::Denied
        );
    }

    #[test]
    fn candidates_are_collected_from_all_carriers() {
        let map = headers(&[
            ("x-api-key", "a"),
            ("authorization", "Bearer b"),
            ("authorization", "Basic ignored"),
            ("x-goog-api-key", "c"),
        ]);
        let got: Vec<&[u8]> = candidates(&map, Some("key=d&x=1"));
        assert_eq!(got, vec![&b"a"[..], b"b", b"c", b"d"]);
    }
}
