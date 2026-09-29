//! 请求 / 响应头处理（设计文档 §2.2、§2.4、§5.3）。
//!
//! 只做透传边界内允许的变化：移除 hop-by-hop 头、`expect` 与入站凭据，原位替换 `host`，
//! 追加上游鉴权头。其余头按入站顺序逐条复制，值不变。
//!
//! 顺序说明：`http::HeaderMap` 会把同名头聚合到首次出现的位置，同名头彼此的相对顺序不变。

use http::header::{self, HeaderMap, HeaderName, HeaderValue};

use crate::config::{AuthScheme, Secret};

/// RFC 9110 §7.6.1 的逐跳头，外加非标准的 `proxy-connection`。
pub const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 入站鉴权可能携带中转 token 的头，无论鉴权结果如何都不转发给上游。
pub const INBOUND_CREDENTIAL_HEADERS: [&str; 3] = ["authorization", "x-api-key", "x-goog-api-key"];

/// `connection` 头值中列出的字段名（它们同样是逐跳的）。
fn connection_listed(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect()
}

fn is_hop_by_hop(name: &HeaderName, listed: &[HeaderName]) -> bool {
    HOP_BY_HOP.contains(&name.as_str()) || listed.contains(name)
}

/// 构造发往上游的请求头。
///
/// - 移除：hop-by-hop（含 `connection` 列出的字段）、`expect`、入站凭据头。
/// - `host`：在入站 `host` 的原位置写入上游 host；入站没有 `host` 时放在最前。
/// - `upstream_auth`：追加在末尾。
pub fn upstream_request_headers(
    inbound: &HeaderMap,
    upstream_host: HeaderValue,
    upstream_auth: Option<(HeaderName, HeaderValue)>,
) -> HeaderMap {
    let listed = connection_listed(inbound);
    let mut out = HeaderMap::with_capacity(inbound.len() + 2);
    let mut upstream_host = Some(upstream_host);

    if !inbound.contains_key(header::HOST) {
        if let Some(host) = upstream_host.take() {
            out.insert(header::HOST, host);
        }
    }

    for (name, value) in inbound {
        if name == header::HOST {
            if let Some(host) = upstream_host.take() {
                out.insert(header::HOST, host);
            }
            continue;
        }
        if name == header::EXPECT
            || is_hop_by_hop(name, &listed)
            || INBOUND_CREDENTIAL_HEADERS.contains(&name.as_str())
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }

    if let Some((name, value)) = upstream_auth {
        out.append(name, value);
    }
    out
}

/// 构造返回给客户端的响应头：只移除 hop-by-hop（含 `connection` 列出的字段）。
pub fn client_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let listed = connection_listed(upstream);
    let mut out = HeaderMap::with_capacity(upstream.len());
    for (name, value) in upstream {
        if !is_hop_by_hop(name, &listed) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// 按鉴权方式生成上游鉴权头；`AuthScheme::None` 返回 `None`。
///
/// 值标记为 sensitive，hyper 与 `http` 的调试输出不会打印它。Key 含非法头字符时返回错误
/// （配置校验已拒绝控制字符，这里是兜底）。
pub fn upstream_auth_header(
    scheme: AuthScheme,
    key: &Secret,
) -> Result<Option<(HeaderName, HeaderValue)>, http::header::InvalidHeaderValue> {
    let (name, value) = match scheme {
        AuthScheme::None => return Ok(None),
        AuthScheme::Bearer => (header::AUTHORIZATION, format!("Bearer {}", key.expose())),
        AuthScheme::XApiKey => (
            HeaderName::from_static("x-api-key"),
            key.expose().to_string(),
        ),
        AuthScheme::XGoogApiKey => (
            HeaderName::from_static("x-goog-api-key"),
            key.expose().to_string(),
        ),
    };
    let mut value = HeaderValue::from_str(&value)?;
    value.set_sensitive(true);
    Ok(Some((name, value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &[u8])]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_bytes(value).unwrap(),
            );
        }
        map
    }

    fn pairs(map: &HeaderMap) -> Vec<(String, Vec<u8>)> {
        map.iter()
            .map(|(n, v)| (n.as_str().to_string(), v.as_bytes().to_vec()))
            .collect()
    }

    fn host(value: &'static str) -> HeaderValue {
        HeaderValue::from_static(value)
    }

    #[test]
    fn keeps_order_values_and_replaces_host_in_place() {
        let inbound = headers(&[
            ("user-agent", b"claude-cli/2.1"),
            ("host", b"127.0.0.1:15721"),
            ("anthropic-version", b"2023-06-01"),
            ("content-type", b"application/json"),
            ("content-length", b"42"),
            ("accept-encoding", b"gzip, br"),
            ("x-forwarded-for", b"10.0.0.1"),
        ]);
        let out = upstream_request_headers(&inbound, host("api.anthropic.com"), None);
        assert_eq!(
            pairs(&out),
            vec![
                ("user-agent".into(), b"claude-cli/2.1".to_vec()),
                ("host".into(), b"api.anthropic.com".to_vec()),
                ("anthropic-version".into(), b"2023-06-01".to_vec()),
                ("content-type".into(), b"application/json".to_vec()),
                ("content-length".into(), b"42".to_vec()),
                ("accept-encoding".into(), b"gzip, br".to_vec()),
                ("x-forwarded-for".into(), b"10.0.0.1".to_vec()),
            ]
        );
    }

    #[test]
    fn removes_hop_by_hop_expect_and_credentials() {
        let inbound = headers(&[
            ("host", b"relay"),
            ("connection", b"keep-alive, X-Custom-Hop"),
            ("keep-alive", b"timeout=5"),
            ("proxy-connection", b"keep-alive"),
            ("proxy-authorization", b"Basic abc"),
            ("te", b"trailers"),
            ("trailer", b"x-checksum"),
            ("transfer-encoding", b"chunked"),
            ("upgrade", b"h2c"),
            ("x-custom-hop", b"1"),
            ("expect", b"100-continue"),
            ("authorization", b"Bearer relay-token"),
            ("x-api-key", b"relay-token"),
            ("x-goog-api-key", b"relay-token"),
            ("anthropic-beta", b"a"),
        ]);
        let out = upstream_request_headers(&inbound, host("up.example.com"), None);
        assert_eq!(
            pairs(&out),
            vec![
                ("host".into(), b"up.example.com".to_vec()),
                ("anthropic-beta".into(), b"a".to_vec()),
            ]
        );
    }

    #[test]
    fn non_bearer_authorization_is_also_removed() {
        let inbound = headers(&[("authorization", b"Basic dXNlcjpwYXNz")]);
        let out = upstream_request_headers(&inbound, host("h"), None);
        assert!(!out.contains_key(header::AUTHORIZATION));
    }

    #[test]
    fn duplicate_headers_keep_relative_order_and_group_at_first_position() {
        let inbound = headers(&[
            ("anthropic-beta", b"one"),
            ("user-agent", b"ua"),
            ("anthropic-beta", b"two"),
        ]);
        let out = upstream_request_headers(&inbound, host("h"), None);
        assert_eq!(
            pairs(&out),
            vec![
                ("host".into(), b"h".to_vec()),
                ("anthropic-beta".into(), b"one".to_vec()),
                ("anthropic-beta".into(), b"two".to_vec()),
                ("user-agent".into(), b"ua".to_vec()),
            ]
        );
    }

    #[test]
    fn non_utf8_header_values_are_preserved() {
        let inbound = headers(&[("x-raw", &[0x80, 0xff, b'a'])]);
        let out = upstream_request_headers(&inbound, host("h"), None);
        assert_eq!(out.get("x-raw").unwrap().as_bytes(), &[0x80, 0xff, b'a']);
    }

    #[test]
    fn upstream_auth_is_appended_last() {
        let inbound = headers(&[
            ("host", b"relay"),
            ("x-api-key", b"relay-token"),
            ("accept", b"*/*"),
        ]);
        let auth = upstream_auth_header(AuthScheme::XApiKey, &Secret::new("sk-up")).unwrap();
        let out = upstream_request_headers(&inbound, host("h"), auth);
        assert_eq!(
            pairs(&out),
            vec![
                ("host".into(), b"h".to_vec()),
                ("accept".into(), b"*/*".to_vec()),
                ("x-api-key".into(), b"sk-up".to_vec()),
            ]
        );
        assert!(out.get("x-api-key").unwrap().is_sensitive());
    }

    #[test]
    fn auth_header_per_scheme() {
        let key = Secret::new("k");
        let cases = [
            (AuthScheme::Bearer, Some(("authorization", "Bearer k"))),
            (AuthScheme::XApiKey, Some(("x-api-key", "k"))),
            (AuthScheme::XGoogApiKey, Some(("x-goog-api-key", "k"))),
            (AuthScheme::None, None),
        ];
        for (scheme, expected) in cases {
            let actual = upstream_auth_header(scheme, &key).unwrap();
            let actual = actual
                .as_ref()
                .map(|(n, v)| (n.as_str(), v.to_str().unwrap()));
            assert_eq!(actual, expected, "{scheme:?}");
        }
        assert!(upstream_auth_header(AuthScheme::XApiKey, &Secret::new("bad\nkey")).is_err());
    }

    #[test]
    fn response_headers_drop_only_hop_by_hop() {
        let upstream = headers(&[
            ("content-type", b"text/event-stream"),
            ("transfer-encoding", b"chunked"),
            ("connection", b"close, x-hop"),
            ("x-hop", b"1"),
            ("content-encoding", b"gzip"),
            ("retry-after", b"3"),
            ("set-cookie", b"a=1"),
            ("set-cookie", b"b=2"),
            ("x-request-id", b"req_1"),
        ]);
        let out = client_response_headers(&upstream);
        assert_eq!(
            pairs(&out),
            vec![
                ("content-type".into(), b"text/event-stream".to_vec()),
                ("content-encoding".into(), b"gzip".to_vec()),
                ("retry-after".into(), b"3".to_vec()),
                ("set-cookie".into(), b"a=1".to_vec()),
                ("set-cookie".into(), b"b=2".to_vec()),
                ("x-request-id".into(), b"req_1".to_vec()),
            ]
        );
    }
}
