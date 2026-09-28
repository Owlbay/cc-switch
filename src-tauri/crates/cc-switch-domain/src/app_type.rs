use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// 应用类型
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppType {
    Claude,
    #[serde(
        rename = "claude-desktop",
        alias = "claude_desktop",
        alias = "claudeDesktop"
    )]
    ClaudeDesktop,
    Codex,
    Gemini,
    GrokBuild,
    OpenCode,
    OpenClaw,
    Hermes,
    Pi,
    Mcode,
}

impl AppType {
    pub fn as_str(&self) -> &str {
        match self {
            AppType::Claude => "claude",
            AppType::ClaudeDesktop => "claude-desktop",
            AppType::Codex => "codex",
            AppType::Gemini => "gemini",
            AppType::GrokBuild => "grokbuild",
            AppType::OpenCode => "opencode",
            AppType::OpenClaw => "openclaw",
            AppType::Hermes => "hermes",
            AppType::Pi => "pi",
            AppType::Mcode => "mcode",
        }
    }

    /// Check if this app uses additive mode
    ///
    /// - Switch mode (false): Only the current provider is written to live config (Claude, Codex, Gemini)
    /// - Additive mode (true): Providers coexist in native config and can be enabled independently
    ///   (OpenCode, OpenClaw, Hermes, Pi)
    pub fn is_additive_mode(&self) -> bool {
        matches!(
            self,
            AppType::OpenCode | AppType::OpenClaw | AppType::Hermes | AppType::Pi | AppType::Mcode
        )
    }

    pub fn supports_local_proxy(&self) -> bool {
        matches!(
            self,
            AppType::Claude | AppType::Codex | AppType::Gemini | AppType::GrokBuild
        )
    }

    /// Return an iterator over all app types
    pub fn all() -> impl Iterator<Item = AppType> {
        [
            AppType::Claude,
            AppType::ClaudeDesktop,
            AppType::Codex,
            AppType::Gemini,
            AppType::GrokBuild,
            AppType::OpenCode,
            AppType::OpenClaw,
            AppType::Hermes,
            AppType::Pi,
            AppType::Mcode,
        ]
        .into_iter()
    }
}

impl FromStr for AppType {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_lowercase();
        match normalized.as_str() {
            "claude" => Ok(AppType::Claude),
            "claude-desktop" | "claude_desktop" | "claudedesktop" => Ok(AppType::ClaudeDesktop),
            "codex" => Ok(AppType::Codex),
            "gemini" => Ok(AppType::Gemini),
            "grokbuild" | "grok-build" | "grok_build" | "grok" => Ok(AppType::GrokBuild),
            "opencode" => Ok(AppType::OpenCode),
            "openclaw" => Ok(AppType::OpenClaw),
            "hermes" => Ok(AppType::Hermes),
            "pi" => Ok(AppType::Pi),
            "mcode" => Ok(AppType::Mcode),
            other => Err(DomainError::localized(
                "unsupported_app",
                format!("不支持的应用标识: '{other}'。可选值: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi。"),
                format!("Unsupported app id: '{other}'. Allowed: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi."),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误文本快照：必须与迁移前 `AppError::Localized` 的输出逐字一致。
    #[test]
    fn unsupported_app_error_text_is_stable() {
        let err = AppType::from_str("bogus").unwrap_err();
        assert_eq!(
            err.to_string(),
            "不支持的应用标识: 'bogus'。可选值: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi。 (Unsupported app id: 'bogus'. Allowed: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi.)"
        );
        assert!(matches!(
            err,
            DomainError::Localized {
                key: "unsupported_app",
                ..
            }
        ));
    }
}
