//! 配置文件加载（设计文档 §10）。
//!
//! TOML 先解析为 `toml::Value`，只对**字符串值**展开 `${VAR}`，再反序列化为
//! [`RelayConfig`]。不在原始文本上做替换，避免变量值中的引号、换行破坏 TOML 结构。
//!
//! 展开规则：
//! - `${NAME}`：`NAME` 必须匹配 `[A-Za-z_][A-Za-z0-9_]*`，变量未定义时报错；
//! - `$${`：转义，得到字面量 `${`；
//! - 其它 `$` 原样保留；
//! - 替换结果不再二次展开（变量值中的 `${` 按字面量保留）。

use std::path::{Path, PathBuf};

use cc_proxy_core::RelayConfig;

#[derive(Debug, thiserror::Error)]
pub enum ConfigFileError {
    #[error("读取配置文件 {path} 失败: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("解析配置文件 {path} 失败: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("配置文件 {path} 中 {location}: {message}")]
    Expand {
        path: PathBuf,
        location: String,
        message: String,
    },
}

/// 读取并解析配置文件，使用进程环境变量展开 `${VAR}`。不做取值校验（由调用方 validate）。
pub fn load(path: &Path) -> Result<RelayConfig, ConfigFileError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigFileError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&text, path, |name| std::env::var(name).ok())
}

/// 解析配置文本；`env` 用于查询环境变量（便于测试注入）。
pub fn parse(
    text: &str,
    path: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Result<RelayConfig, ConfigFileError> {
    let mut value: toml::Value = toml::from_str(text).map_err(|e| ConfigFileError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    expand_value(&mut value, &mut String::new(), &env).map_err(|(location, message)| {
        ConfigFileError::Expand {
            path: path.to_path_buf(),
            location,
            message,
        }
    })?;
    value
        .try_into::<RelayConfig>()
        .map_err(|e| ConfigFileError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
}

/// 默认配置路径：显式参数 > `CC_PROXY_CONFIG` > `$HOME/.cc-proxy/config.toml`
/// （Windows 为 `%USERPROFILE%\.cc-proxy\config.toml`）。
pub fn resolve_path(
    explicit: Option<PathBuf>,
    env: impl Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path);
    }
    if let Some(path) = env("CC_PROXY_CONFIG").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env(home_var)
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".cc-proxy").join("config.toml"))
}

fn expand_value(
    value: &mut toml::Value,
    location: &mut String,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(), (String, String)> {
    match value {
        toml::Value::String(s) => {
            *s = expand_str(s, env).map_err(|message| (location.clone(), message))?;
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let len = location.len();
                location.push_str(&format!("[{index}]"));
                expand_value(item, location, env)?;
                location.truncate(len);
            }
        }
        toml::Value::Table(table) => {
            for (key, item) in table.iter_mut() {
                let len = location.len();
                if !location.is_empty() {
                    location.push('.');
                }
                location.push_str(key);
                expand_value(item, location, env)?;
                location.truncate(len);
            }
        }
        _ => {}
    }
    Ok(())
}

fn expand_str(input: &str, env: &impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(after) = tail.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
        } else if let Some(after) = tail.strip_prefix("${") {
            let end = after
                .find('}')
                .ok_or_else(|| "`${` 没有闭合的 `}`".to_string())?;
            let name = &after[..end];
            if !is_valid_name(name) {
                return Err(format!(
                    "`${{{name}}}` 不是合法的环境变量名（只允许字母、数字和下划线，且不以数字开头）"
                ));
            }
            let value = env(name).ok_or_else(|| format!("引用了未定义的环境变量 {name}"))?;
            out.push_str(&value);
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use cc_proxy_core::config::AuthScheme;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn expand(input: &str, pairs: &[(&str, &str)]) -> Result<String, String> {
        expand_str(input, &env(pairs))
    }

    #[test]
    fn expansion_rules() {
        assert_eq!(expand("${A}", &[("A", "x")]).unwrap(), "x");
        assert_eq!(
            expand("pre-${A}-${B_2}", &[("A", "1"), ("B_2", "2")]).unwrap(),
            "pre-1-2"
        );
        assert_eq!(expand("$${A}", &[]).unwrap(), "${A}");
        assert_eq!(expand("cost $5 and $", &[]).unwrap(), "cost $5 and $");
        assert_eq!(expand("${EMPTY}", &[("EMPTY", "")]).unwrap(), "");
        // 替换结果不再二次展开
        assert_eq!(
            expand("${A}", &[("A", "${B}"), ("B", "no")]).unwrap(),
            "${B}"
        );
    }

    #[test]
    fn expansion_errors() {
        assert!(expand("${MISSING}", &[]).unwrap_err().contains("MISSING"));
        assert!(expand("${A", &[("A", "x")])
            .unwrap_err()
            .contains("没有闭合"));
        assert!(expand("${1A}", &[]).unwrap_err().contains("不是合法"));
        assert!(expand("${A-B}", &[]).unwrap_err().contains("不是合法"));
        assert!(expand("${}", &[]).unwrap_err().contains("不是合法"));
    }

    const EXAMPLE: &str = r#"
[server]
listen = "127.0.0.1:15721"
auth_tokens = ["${CC_PROXY_TOKEN}"]

[tls]
extra_ca_file = "${HOME_DIR}/ca.pem"

[[upstreams.claude]]
id = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "${ANTHROPIC_API_KEY}"
auth = "bearer"

[[upstreams.gemini]]
id = "google"
base_url = "https://generativelanguage.googleapis.com"
api_key = "${GEMINI_API_KEY}"
"#;

    #[test]
    fn parses_and_expands_into_relay_config() {
        let config = parse(
            EXAMPLE,
            Path::new("test.toml"),
            env(&[
                ("CC_PROXY_TOKEN", "relay-token"),
                ("HOME_DIR", "/home/u"),
                ("ANTHROPIC_API_KEY", "sk-ant \"quoted\" ']]\n[x]"),
                ("GEMINI_API_KEY", "g-key"),
            ]),
        )
        .unwrap();
        assert_eq!(config.server.listen.to_string(), "127.0.0.1:15721");
        assert_eq!(config.server.auth_tokens[0].expose(), "relay-token");
        assert_eq!(
            config.tls.extra_ca_file.as_deref(),
            Some(Path::new("/home/u/ca.pem"))
        );
        // 变量值中的引号、括号与换行不会破坏 TOML 结构（展开发生在解析之后）
        assert_eq!(
            config.upstreams.claude[0].api_key.expose(),
            "sk-ant \"quoted\" ']]\n[x]"
        );
        assert_eq!(config.upstreams.claude[0].auth, Some(AuthScheme::Bearer));
        assert_eq!(config.upstreams.gemini[0].api_key.expose(), "g-key");
        // 未写的段落取默认值
        assert_eq!(config.timeouts.idle_secs, 300);
        // 结构完好，但含换行的 Key 会被取值校验拒绝
        let errors = config.validate().unwrap_err().to_string();
        assert!(errors.contains("控制字符"), "{errors}");
    }

    #[test]
    fn undefined_variable_reports_its_location() {
        let err = parse(
            EXAMPLE,
            Path::new("test.toml"),
            env(&[
                ("CC_PROXY_TOKEN", "t"),
                ("HOME_DIR", "/h"),
                ("GEMINI_API_KEY", "g"),
            ]),
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("upstreams.claude[0].api_key"), "{text}");
        assert!(text.contains("ANTHROPIC_API_KEY"), "{text}");
    }

    #[test]
    fn unknown_fields_and_bad_values_are_parse_errors() {
        let err = parse(
            "[server]\nlisten_addr = \"x\"\n",
            Path::new("t.toml"),
            env(&[]),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("listen_addr"), "{err}");

        let err = parse(
            "[server]\nlisten = \"not-an-addr\"\n",
            Path::new("t.toml"),
            env(&[]),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse { .. }), "{err}");

        let err = parse("[server\n", Path::new("t.toml"), env(&[])).unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse { .. }), "{err}");
    }

    #[test]
    fn keys_and_non_string_values_are_not_expanded() {
        let config = parse(
            "[server]\nauth_tokens = [\"$${literal}\"]\nmax_body_bytes = 10\n",
            Path::new("t.toml"),
            env(&[]),
        )
        .unwrap();
        assert_eq!(config.server.auth_tokens[0].expose(), "${literal}");
        assert_eq!(config.server.max_body_bytes, 10);
    }

    #[test]
    fn path_resolution_order() {
        let explicit = resolve_path(
            Some(PathBuf::from("/x.toml")),
            env(&[("CC_PROXY_CONFIG", "/y.toml")]),
        );
        assert_eq!(explicit, Some(PathBuf::from("/x.toml")));
        let from_env = resolve_path(
            None,
            env(&[
                ("CC_PROXY_CONFIG", "/y.toml"),
                ("HOME", "/h"),
                ("USERPROFILE", "/h"),
            ]),
        );
        assert_eq!(from_env, Some(PathBuf::from("/y.toml")));
        let from_home = resolve_path(None, env(&[("HOME", "/h"), ("USERPROFILE", "/h")]));
        assert_eq!(
            from_home,
            Some(PathBuf::from("/h").join(".cc-proxy").join("config.toml"))
        );
        assert_eq!(resolve_path(None, env(&[])), None);
    }
}
