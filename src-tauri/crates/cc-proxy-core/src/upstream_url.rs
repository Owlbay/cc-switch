//! 上游 URL 拼接与 query 过滤（设计文档 §4、§6.3）。
//!
//! 入站 path 与 query 按字节原样拼接，不做任何解码 / 重新编码；只有入站鉴权用的 Gemini
//! `key` 参数会被整段移除。

use url::Url;

/// 字节级移除 query 中名字精确等于 `key` 的参数段（`key=...` 或单独的 `key`）。
///
/// 按 `&` 切分后原样拼回，其余参数（包括空段、`+`、`%20` 等编码）逐字节保留，
/// 不使用 form-urlencoded 重新序列化。
pub fn strip_key_param(query: &str) -> String {
    query
        .split('&')
        .filter(|segment| !is_key_segment(segment))
        .collect::<Vec<_>>()
        .join("&")
}

fn is_key_segment(segment: &str) -> bool {
    segment == "key" || segment.starts_with("key=")
}

/// 从 query 中取出所有 `key` 参数的原始值（不解码），供入站鉴权使用。
pub fn key_param_values(query: &str) -> impl Iterator<Item = &str> {
    query.split('&').filter_map(|segment| {
        if segment == "key" {
            Some("")
        } else {
            segment.strip_prefix("key=")
        }
    })
}

/// `strip_prefix` 是否合法：以 `/` 开头、不以 `/` 结尾、且不只是 `/`。
pub fn is_valid_strip_prefix(prefix: &str) -> bool {
    prefix.len() > 1 && prefix.starts_with('/') && !prefix.ends_with('/')
}

/// 入站 path 等于 `prefix` 或以 `prefix + "/"` 开头时剥离，否则原样返回。
pub fn apply_strip_prefix<'a>(path: &'a str, prefix: Option<&str>) -> &'a str {
    let Some(prefix) = prefix else {
        return path;
    };
    match path.strip_prefix(prefix) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => path,
    }
}

/// 拼出发往上游的请求 URI（绝对形式，authority 带显式端口）。
///
/// - `path` 为入站原始 path（不含 query），`inbound_query` 为入站原始 query（不含 `?`）。
/// - 入站 query 先移除 `key` 参数，再与 `base_url` 自带的 query 用 `&` 合并（base 在前）。
/// - authority 总是写出端口（http 默认 80、https 默认 443），避免经 HTTP CONNECT 代理时
///   hyper-util 的 `Tunnel` 对无端口 URI 一律按 443 连接。
pub fn upstream_uri(
    base_url: &Url,
    strip_prefix: Option<&str>,
    path: &str,
    inbound_query: Option<&str>,
) -> String {
    let mut uri = String::new();
    uri.push_str(base_url.scheme());
    uri.push_str("://");
    uri.push_str(base_url.host_str().unwrap_or_default());
    if let Some(port) = base_url.port_or_known_default() {
        uri.push(':');
        uri.push_str(&port.to_string());
    }

    let base_path = base_url.path().trim_end_matches('/');
    let path = apply_strip_prefix(path, strip_prefix);
    uri.push_str(base_path);
    uri.push_str(path);
    if base_path.is_empty() && path.is_empty() {
        uri.push('/');
    }

    let inbound = inbound_query.map(strip_key_param).unwrap_or_default();
    let query = [base_url.query().unwrap_or_default(), inbound.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("&");
    if !query.is_empty() {
        uri.push('?');
        uri.push_str(&query);
    }
    uri
}

/// 发往上游的 `host` 头的值：host，加上非默认端口。
pub fn upstream_host(base_url: &Url) -> String {
    let host = base_url.host_str().unwrap_or_default();
    match base_url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// `base_url` 的 path 若以端点路径结尾，多半是用户粘贴了完整请求地址。
pub fn looks_like_endpoint_path(base_url: &Url) -> bool {
    const ENDPOINT_SUFFIXES: [&str; 6] = [
        "/messages",
        "/chat/completions",
        "/responses",
        ":generateContent",
        ":streamGenerateContent",
        "/models",
    ];
    let path = base_url.path().trim_end_matches('/');
    ENDPOINT_SUFFIXES
        .iter()
        .any(|suffix| path.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn strip_key_keeps_other_params_byte_for_byte() {
        assert_eq!(
            strip_key_param("alt=sse&key=abc&x=a%20b+c"),
            "alt=sse&x=a%20b+c"
        );
        assert_eq!(strip_key_param("key=abc"), "");
        assert_eq!(strip_key_param("key"), "");
        assert_eq!(strip_key_param("alt=sse"), "alt=sse");
        assert_eq!(strip_key_param("a=1&&b=2"), "a=1&&b=2");
        // 名字只精确匹配 `key`
        assert_eq!(
            strip_key_param("keys=1&apikey=2&KEY=3"),
            "keys=1&apikey=2&KEY=3"
        );
        assert_eq!(strip_key_param("key=1&x=2&key=3"), "x=2");
    }

    #[test]
    fn key_param_values_are_raw() {
        let values: Vec<_> = key_param_values("alt=sse&key=a%2Bb&key&keys=z").collect();
        assert_eq!(values, vec!["a%2Bb", ""]);
    }

    #[test]
    fn strip_prefix_rules() {
        assert!(is_valid_strip_prefix("/v1"));
        assert!(!is_valid_strip_prefix("/"));
        assert!(!is_valid_strip_prefix("v1"));
        assert!(!is_valid_strip_prefix("/v1/"));

        assert_eq!(
            apply_strip_prefix("/v1/chat/completions", Some("/v1")),
            "/chat/completions"
        );
        assert_eq!(apply_strip_prefix("/v1", Some("/v1")), "");
        // 只按段匹配：/v1beta 不以 "/v1/" 开头
        assert_eq!(
            apply_strip_prefix("/v1beta/models/x", Some("/v1")),
            "/v1beta/models/x"
        );
        assert_eq!(apply_strip_prefix("/v1/messages", None), "/v1/messages");
    }

    #[test]
    fn plain_base_url() {
        assert_eq!(
            upstream_uri(
                &url("https://api.deepseek.com"),
                None,
                "/v1/chat/completions",
                None
            ),
            "https://api.deepseek.com:443/v1/chat/completions"
        );
    }

    #[test]
    fn base_url_with_path_and_strip_prefix() {
        assert_eq!(
            upstream_uri(
                &url("https://ark.example.com/api/v3/"),
                Some("/v1"),
                "/v1/chat/completions",
                None
            ),
            "https://ark.example.com:443/api/v3/chat/completions"
        );
        assert_eq!(
            upstream_uri(
                &url("https://open.bigmodel.cn/api/anthropic"),
                None,
                "/v1/messages",
                Some("beta=true")
            ),
            "https://open.bigmodel.cn:443/api/anthropic/v1/messages?beta=true"
        );
    }

    #[test]
    fn base_url_query_is_merged_before_inbound_query() {
        assert_eq!(
            upstream_uri(
                &url("https://x.openai.azure.com/openai?api-version=2024-10-21"),
                None,
                "/v1/chat/completions",
                Some("x=1")
            ),
            "https://x.openai.azure.com:443/openai/v1/chat/completions?api-version=2024-10-21&x=1"
        );
    }

    #[test]
    fn gemini_key_removed_and_other_query_preserved() {
        assert_eq!(
            upstream_uri(
                &url("https://generativelanguage.googleapis.com"),
                None,
                "/v1beta/models/gemini-2.5-flash:streamGenerateContent",
                Some("alt=sse&key=abc&x=a%20b+c")
            ),
            "https://generativelanguage.googleapis.com:443/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse&x=a%20b+c"
        );
        assert_eq!(
            upstream_uri(
                &url("https://generativelanguage.googleapis.com"),
                None,
                "/v1beta/models",
                Some("key=abc")
            ),
            "https://generativelanguage.googleapis.com:443/v1beta/models"
        );
    }

    #[test]
    fn http_upstream_gets_explicit_port() {
        assert_eq!(
            upstream_uri(&url("http://127.0.0.1"), None, "/v1/messages", None),
            "http://127.0.0.1:80/v1/messages"
        );
        assert_eq!(
            upstream_uri(&url("http://localhost:8080/"), None, "/v1/messages", None),
            "http://localhost:8080/v1/messages"
        );
    }

    #[test]
    fn empty_path_becomes_root() {
        assert_eq!(
            upstream_uri(&url("https://example.com"), Some("/v1"), "/v1", None),
            "https://example.com:443/"
        );
    }

    #[test]
    fn host_header_value() {
        assert_eq!(
            upstream_host(&url("https://api.anthropic.com")),
            "api.anthropic.com"
        );
        assert_eq!(
            upstream_host(&url("https://example.com:443")),
            "example.com"
        );
        assert_eq!(
            upstream_host(&url("http://127.0.0.1:8080")),
            "127.0.0.1:8080"
        );
        assert_eq!(upstream_host(&url("http://[::1]:9000")), "[::1]:9000");
    }

    #[test]
    fn detects_pasted_endpoint_paths() {
        for bad in [
            "https://api.anthropic.com/v1/messages",
            "https://api.deepseek.com/v1/chat/completions/",
            "https://api.openai.com/v1/responses",
            "https://g.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent",
            "https://api.openai.com/v1/models",
        ] {
            assert!(looks_like_endpoint_path(&url(bad)), "{bad}");
        }
        for good in [
            "https://api.anthropic.com",
            "https://open.bigmodel.cn/api/anthropic",
            "https://ark.example.com/api/v3",
        ] {
            assert!(!looks_like_endpoint_path(&url(good)), "{good}");
        }
    }
}
