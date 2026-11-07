//! Grok CLI 登录文件 `~/.grok/auth.json` 里 xAI OIDC 那一条（`https://auth.x.ai::<client>`）。
//!
//! Grok 官方卡绑了授权中心的 xAI 账号时，切到这张卡就把账号的登录写进这一条，Grok CLI
//! 下次读到的就是这个号。授权中心的 xAI 登录和 Grok CLI 是同一个 OAuth client，令牌通用；
//! 两边可能共用同一个 refresh token，所以：
//! - 切走前先看 CLI 有没有把它轮换过（[`read_live_login`]），换过就先收进授权中心；
//! - cc-switch 自己刷新换了 refresh token 时，CLI 手里的还是旧的，要写回去
//!   （[`sync_after_refresh`]），否则 CLI 下次刷新会被拒、掉登录。
//!
//! 只动这一条，文件里的其他条目（旧版 session 登录等）原样保留。

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};

use crate::live::patch::{LivePatch, LiveWriteError};

pub(crate) const OIDC_ISSUER: &str = "https://auth.x.ai";

/// `auth.json` 里 OIDC 登录那一条的 key
pub(crate) fn oidc_scope() -> String {
    format!(
        "{OIDC_ISSUER}::{}",
        crate::proxy::providers::xai_oauth_auth::XAI_CLIENT_ID
    )
}

pub(crate) fn auth_path() -> PathBuf {
    crate::grok_config::get_grok_config_dir().join("auth.json")
}

/// 要写进 Grok CLI 的一个账号的登录
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrokCliLogin {
    /// xAI 用户 id（令牌的 `sub`），即授权中心的账号 id
    pub account_id: String,
    pub email: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
    pub team_id: Option<String>,
    pub principal_type: Option<String>,
    pub principal_id: Option<String>,
}

/// live 里 OIDC 那一条现在是谁、拿着哪个 refresh token
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveLogin {
    pub user_id: String,
    pub email: Option<String>,
    pub refresh_token: Option<String>,
}

fn parse_root(path: &Path, content: &[u8]) -> Result<Map<String, Value>, LiveWriteError> {
    let value: Value = serde_json::from_slice(content).map_err(|e| LiveWriteError::Parse {
        path: path.to_path_buf(),
        line: e.line(),
        column: e.column(),
        message: e.to_string(),
    })?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(LiveWriteError::Shape {
            path: path.to_path_buf(),
            key_path: crate::live::patch::KeyPath::root(),
            expected: "对象",
        }),
    }
}

fn is_blank(content: &[u8]) -> bool {
    content.iter().all(u8::is_ascii_whitespace)
}

/// 读出 OIDC 那一条的用户和 refresh token；文件不存在、没有这一条或这一条没有 `user_id`
/// 时为 `None`。
pub(crate) fn read_live_login(
    path: &Path,
    content: Option<&[u8]>,
) -> Result<Option<LiveLogin>, LiveWriteError> {
    let Some(content) = content.filter(|c| !is_blank(c)) else {
        return Ok(None);
    };
    let root = parse_root(path, content)?;
    let Some(entry) = root.get(&oidc_scope()).and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(user_id) = entry
        .get("user_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(LiveLogin {
        user_id: user_id.to_string(),
        email: entry
            .get("email")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
            .map(str::to_string),
        refresh_token: entry
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
    }))
}

/// 读盘上的 OIDC 登录（读不了、解析不了都当没有）
pub(crate) fn read_live_login_from_disk() -> Option<LiveLogin> {
    let path = auth_path();
    let content = crate::live::engine::read_current(&path).ok()??;
    read_live_login(&path, Some(&content)).ok()?
}

fn iso_micros(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// 以 `pre` 为底，把 OIDC 那一条换成 `login`。同一个用户时保留 CLI 自己写的其他字段
/// （名字、头像、团队设置等），换了用户就整条重建，免得留下上一个号的资料。
pub(crate) fn merge_login(
    path: &Path,
    pre: Option<&[u8]>,
    login: &GrokCliLogin,
    now: DateTime<Utc>,
) -> Result<Vec<u8>, LiveWriteError> {
    let mut root = match pre.filter(|c| !is_blank(c)) {
        Some(content) => parse_root(path, content)?,
        None => Map::new(),
    };
    let scope = oidc_scope();
    let mut entry = root
        .get(&scope)
        .and_then(Value::as_object)
        .filter(|existing| {
            existing.get("user_id").and_then(Value::as_str) == Some(login.account_id.as_str())
        })
        .cloned()
        .unwrap_or_default();

    let mut set = |key: &str, value: Value| {
        entry.insert(key.to_string(), value);
    };
    set("key", Value::String(login.access_token.clone()));
    set("auth_mode", Value::String("oidc".to_string()));
    set(
        "create_time",
        Value::String(now.to_rfc3339_opts(SecondsFormat::Micros, true)),
    );
    set("user_id", Value::String(login.account_id.clone()));
    if let Some(email) = login.email.as_deref().filter(|e| e.contains('@')) {
        set("email", Value::String(email.to_string()));
    }
    set(
        "principal_type",
        Value::String(
            login
                .principal_type
                .clone()
                .unwrap_or_else(|| "User".to_string()),
        ),
    );
    set(
        "principal_id",
        Value::String(
            login
                .principal_id
                .clone()
                .unwrap_or_else(|| login.account_id.clone()),
        ),
    );
    if let Some(team_id) = &login.team_id {
        set("team_id", Value::String(team_id.clone()));
    }
    set("refresh_token", Value::String(login.refresh_token.clone()));
    set("expires_at", Value::String(iso_micros(login.expires_at_ms)));
    set("oidc_issuer", Value::String(OIDC_ISSUER.to_string()));
    set(
        "oidc_client_id",
        Value::String(crate::proxy::providers::xai_oauth_auth::XAI_CLIENT_ID.to_string()),
    );

    root.insert(scope, Value::Object(entry));
    let mut bytes =
        serde_json::to_vec_pretty(&Value::Object(root)).map_err(|e| LiveWriteError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// 切到绑了账号的官方卡时写 `auth.json` 的补丁。`expected` 是准备阶段看到的 OIDC 登录：
/// 发布前 Grok CLI 又刷新过（refresh token 变了），就拒绝写，免得把刚轮换出来的令牌覆盖掉。
pub(crate) struct GrokAuthPatch {
    pub login: GrokCliLogin,
    pub expected: Option<LiveLogin>,
}

impl LivePatch for GrokAuthPatch {
    fn apply(&self, path: &Path, pre: Option<&[u8]>) -> Result<Vec<u8>, LiveWriteError> {
        if read_live_login(path, pre)? != self.expected {
            return Err(LiveWriteError::Conflict {
                path: path.to_path_buf(),
            });
        }
        merge_login(path, pre, &self.login, Utc::now())
    }
}

/// cc-switch 刷新账号后，把新令牌同步给共用同一个 refresh token 的 Grok CLI。
///
/// 只有 live 那一条就是这个账号、且拿着这次被换掉的 `consumed_refresh` 时才写；CLI 自己
/// 登录的另一套会话（refresh token 不同）不碰。返回是否写了。
pub(crate) fn sync_after_refresh(
    consumed_refresh: &str,
    login: &GrokCliLogin,
) -> Result<bool, String> {
    let path = auth_path();
    let pre = crate::live::engine::read_current(&path).map_err(|e| e.to_string())?;
    let live = read_live_login(&path, pre.as_deref()).map_err(|e| e.to_string())?;
    let shares_session = live.is_some_and(|live| {
        live.user_id == login.account_id && live.refresh_token.as_deref() == Some(consumed_refresh)
    });
    if !shares_session {
        return Ok(false);
    }
    let bytes = merge_login(&path, pre.as_deref(), login, Utc::now()).map_err(|e| e.to_string())?;
    crate::config::atomic_write_private(&path, &bytes).map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(account: &str, refresh: &str) -> GrokCliLogin {
        GrokCliLogin {
            account_id: account.to_string(),
            email: Some(format!("{account}@example.com")),
            access_token: format!("at-{account}"),
            refresh_token: refresh.to_string(),
            expires_at_ms: 1_800_000_000_000,
            team_id: Some("team-1".to_string()),
            principal_type: Some("User".to_string()),
            principal_id: Some(account.to_string()),
        }
    }

    fn path() -> PathBuf {
        PathBuf::from("/tmp/grok/auth.json")
    }

    fn entry(bytes: &[u8]) -> Map<String, Value> {
        let root: Value = serde_json::from_slice(bytes).unwrap();
        root[oidc_scope()].as_object().unwrap().clone()
    }

    #[test]
    fn writes_fresh_file_with_cli_shape() {
        let out = merge_login(&path(), None, &login("u1", "rt1"), Utc::now()).unwrap();
        let e = entry(&out);
        assert_eq!(e["key"], "at-u1");
        assert_eq!(e["user_id"], "u1");
        assert_eq!(e["refresh_token"], "rt1");
        assert_eq!(e["auth_mode"], "oidc");
        assert_eq!(e["oidc_issuer"], OIDC_ISSUER);
        assert_eq!(e["email"], "u1@example.com");
        assert!(e["expires_at"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn same_user_keeps_cli_fields_and_other_entries() {
        let pre = serde_json::json!({
            "https://accounts.x.ai/sign-in": {"key": "legacy"},
            (oidc_scope()): {"user_id": "u1", "first_name": "Ann", "key": "old", "refresh_token": "rt0"}
        })
        .to_string();
        let out = merge_login(
            &path(),
            Some(pre.as_bytes()),
            &login("u1", "rt1"),
            Utc::now(),
        )
        .unwrap();
        let e = entry(&out);
        assert_eq!(e["first_name"], "Ann");
        assert_eq!(e["key"], "at-u1");
        let root: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(root["https://accounts.x.ai/sign-in"]["key"], "legacy");
    }

    #[test]
    fn other_user_entry_is_rebuilt() {
        let pre = serde_json::json!({
            (oidc_scope()): {"user_id": "u2", "first_name": "Bob", "key": "old"}
        })
        .to_string();
        let out = merge_login(
            &path(),
            Some(pre.as_bytes()),
            &login("u1", "rt1"),
            Utc::now(),
        )
        .unwrap();
        let e = entry(&out);
        assert!(!e.contains_key("first_name"));
        assert_eq!(e["user_id"], "u1");
    }

    #[test]
    fn reads_live_login() {
        assert_eq!(read_live_login(&path(), None).unwrap(), None);
        assert_eq!(read_live_login(&path(), Some(b"  ")).unwrap(), None);
        let pre = serde_json::json!({
            (oidc_scope()): {"user_id": "u1", "refresh_token": "rt1"}
        })
        .to_string();
        assert_eq!(
            read_live_login(&path(), Some(pre.as_bytes())).unwrap(),
            Some(LiveLogin {
                user_id: "u1".to_string(),
                email: None,
                refresh_token: Some("rt1".to_string())
            })
        );
        assert!(read_live_login(&path(), Some(b"{oops")).is_err());
    }

    #[test]
    fn patch_refuses_when_cli_rotated_since_prepare() {
        let pre = serde_json::json!({
            (oidc_scope()): {"user_id": "u2", "refresh_token": "rotated"}
        })
        .to_string();
        let patch = GrokAuthPatch {
            login: login("u1", "rt1"),
            expected: Some(LiveLogin {
                user_id: "u2".to_string(),
                email: None,
                refresh_token: Some("before".to_string()),
            }),
        };
        assert!(matches!(
            patch.apply(&path(), Some(pre.as_bytes())),
            Err(LiveWriteError::Conflict { .. })
        ));
        let patch = GrokAuthPatch {
            login: login("u1", "rt1"),
            expected: Some(LiveLogin {
                user_id: "u2".to_string(),
                email: None,
                refresh_token: Some("rotated".to_string()),
            }),
        };
        assert!(patch.apply(&path(), Some(pre.as_bytes())).is_ok());
    }
}
