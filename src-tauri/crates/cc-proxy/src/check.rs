//! `cc-proxy check`：校验配置并打印每个上游的 dry-run URL，不发任何请求。

use std::fmt::Write as _;
use std::path::Path;

use cc_proxy_core::config::AuthScheme;
use cc_proxy_core::upstream_url::upstream_uri;
use cc_proxy_core::{Interface, Relay};

/// 各接口用于 dry-run 的典型入站 path
pub fn sample_path(interface: Interface) -> &'static str {
    match interface {
        Interface::Claude => "/v1/messages",
        Interface::OpenaiChat => "/v1/chat/completions",
        Interface::OpenaiResponses => "/v1/responses",
        Interface::Gemini => "/v1beta/models/gemini-2.5-flash:streamGenerateContent",
    }
}

/// 生成 dry-run 报告：每个上游最终请求的 URL、鉴权方式与出站方式。不包含任何 Key，
/// URL 中 query 参数只保留名字。
pub fn report(relay: &Relay) -> String {
    let mut out = String::new();
    for interface in Interface::ALL {
        let upstreams = relay.upstreams(interface);
        let _ = writeln!(out, "[{}]", interface.as_str());
        if upstreams.is_empty() {
            let _ = writeln!(out, "  （未配置上游，该接口的请求将返回 503）");
            continue;
        }
        for upstream in upstreams {
            let uri = upstream_uri(
                &upstream.base_url,
                upstream.strip_prefix.as_deref(),
                sample_path(upstream.protocol),
                None,
            );
            let convert = if !upstream.converts {
                String::new()
            } else if upstream.protocol == interface {
                format!(" convert={}->{}", interface.as_str(), interface.as_str())
            } else {
                // 跨协议时客户端可不给 max_tokens，打印兜底值便于核对
                format!(
                    " convert={}->{} default_max_output_tokens={}",
                    interface.as_str(),
                    upstream.protocol.as_str(),
                    upstream.default_max_output_tokens
                )
            };
            let _ = writeln!(
                out,
                "  {} -> {}  auth={} via={}{}",
                upstream.id,
                mask_query_values(&uri),
                auth_label(upstream.auth),
                upstream.client.kind(),
                convert
            );
        }
    }
    out
}

fn auth_label(scheme: AuthScheme) -> &'static str {
    match scheme {
        AuthScheme::Bearer => "bearer",
        AuthScheme::XApiKey => "x-api-key",
        AuthScheme::XGoogApiKey => "x-goog-api-key",
        AuthScheme::None => "none",
    }
}

/// `?a=1&b=2` → `?a=***&b=***`：base_url 的 query 可能带凭据，打印时只保留参数名。
fn mask_query_values(uri: &str) -> String {
    let Some((base, query)) = uri.split_once('?') else {
        return uri.to_string();
    };
    let masked = query
        .split('&')
        .map(|segment| match segment.split_once('=') {
            Some((name, _)) => format!("{name}=***"),
            None => segment.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{masked}")
}

/// 配置文件可能含明文 Key：在 unix 上对 group / other 可读时给出提示。
pub fn permission_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode();
        if mode & 0o077 != 0 {
            return Some(format!(
                "配置文件 {} 的权限为 {:o}，其他用户可读；若其中含明文 Key，建议 chmod 600",
                path.display(),
                mode & 0o777
            ));
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_only_query_values() {
        assert_eq!(
            mask_query_values("https://h:443/p?api-version=1&key=secret&flag"),
            "https://h:443/p?api-version=***&key=***&flag"
        );
        assert_eq!(mask_query_values("https://h:443/p"), "https://h:443/p");
    }

    #[test]
    fn sample_paths_are_identified_as_their_interface() {
        for interface in Interface::ALL {
            let method = http::Method::POST;
            assert_eq!(
                cc_proxy_core::identify(&method, sample_path(interface), &http::HeaderMap::new()),
                Some(interface)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn warns_about_group_readable_config() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cc-proxy-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(permission_warning(&path).unwrap().contains("644"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(permission_warning(&path), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
