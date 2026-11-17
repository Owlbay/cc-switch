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
    Kimi,
    Dsh,
    Agy,
    Omp,
    Copilot,
    #[serde(rename = "workbuddy", alias = "work-buddy")]
    WorkBuddy,
}

/// 一个应用支持哪些功能。每个应用都在 [`AppType::spec`] 里写全每一项，新增应用时漏写
/// 就编译不过。前端 `src/config/appSpecs.ts` 的 `APP_SPECS` 有同样的几项，
/// `tests/config/appSpecs.json` 由这里的测试生成，前端测试拿它比对两边。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSpec {
    /// 序列化、命令参数和前端用的应用 ID
    pub id: &'static str,
    /// 累加式：供应商并存在一个 live 文件里，可以各自启用。
    ///
    /// GitHub Copilot CLI 是混合的：自带模型卡累加式写进 `providers.json`，官方登录卡仍是
    /// 启动式（只打开终端、不写文件，启动时照旧从 settings.json 导入）。
    pub additive: bool,
    /// 有完整的本地路由和故障转移（`proxy_config` 行）
    pub local_proxy: bool,
    /// 有设备级的直连 / 路由模式（`live-state.json`，`mode::controller`）。Claude Desktop 经
    /// 同一个路由服务转发，但没有 `proxy_config` 行、不做故障转移，所以只有这一项。
    pub route_mode: bool,
    /// 支持 Stack 模式（`mode::stack`）
    pub stack: bool,
}

impl AppType {
    /// 这个应用的功能表。不要加 `_ =>` 分支：新增应用必须在这里写全每一项。
    pub const fn spec(&self) -> AppSpec {
        match self {
            AppType::Claude => AppSpec {
                id: "claude",
                additive: false,
                local_proxy: true,
                route_mode: true,
                stack: true,
            },
            AppType::ClaudeDesktop => AppSpec {
                id: "claude-desktop",
                additive: false,
                local_proxy: false,
                route_mode: true,
                stack: true,
            },
            AppType::Codex => AppSpec {
                id: "codex",
                additive: false,
                local_proxy: true,
                route_mode: true,
                stack: true,
            },
            AppType::Gemini => AppSpec {
                id: "gemini",
                additive: false,
                local_proxy: true,
                route_mode: true,
                stack: false,
            },
            AppType::GrokBuild => AppSpec {
                id: "grokbuild",
                additive: false,
                local_proxy: true,
                route_mode: true,
                stack: false,
            },
            AppType::OpenCode => AppSpec {
                id: "opencode",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::OpenClaw => AppSpec {
                id: "openclaw",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::Hermes => AppSpec {
                id: "hermes",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::Pi => AppSpec {
                id: "pi",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::Mcode => AppSpec {
                id: "mcode",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::Kimi => AppSpec {
                id: "kimi",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::Dsh => AppSpec {
                id: "dsh",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::Agy => AppSpec {
                id: "agy",
                additive: false,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::Omp => AppSpec {
                id: "omp",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::Copilot => AppSpec {
                id: "copilot",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
            AppType::WorkBuddy => AppSpec {
                id: "workbuddy",
                additive: true,
                local_proxy: false,
                route_mode: false,
                stack: false,
            },
        }
    }

    pub fn as_str(&self) -> &str {
        self.spec().id
    }

    /// 所有应用，按默认顺序
    pub const ALL: [AppType; 16] = [
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
        AppType::Kimi,
        AppType::Dsh,
        AppType::Agy,
        AppType::Omp,
        AppType::Copilot,
        AppType::WorkBuddy,
    ];

    pub fn all() -> impl Iterator<Item = AppType> {
        Self::ALL.into_iter()
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
            "kimi" | "kimi-code" => Ok(AppType::Kimi),
            "dsh" | "deepseek-harness" => Ok(AppType::Dsh),
            "agy" | "antigravity" | "antigravity-cli" => Ok(AppType::Agy),
            "omp" | "oh-my-pi" | "oh_my_pi" | "ohmypi" => Ok(AppType::Omp),
            "copilot" | "copilot-cli" | "github-copilot" | "github-copilot-cli" => {
                Ok(AppType::Copilot)
            }
            "workbuddy" | "work-buddy" | "work_buddy" => Ok(AppType::WorkBuddy),
            other => Err(DomainError::localized(
                "unsupported_app",
                format!("不支持的应用标识: '{other}'。可选值: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi, mcode, kimi, dsh, agy, omp, copilot, workbuddy。"),
                format!("Unsupported app id: '{other}'. Allowed: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi, mcode, kimi, dsh, agy, omp, copilot, workbuddy."),
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
            "不支持的应用标识: 'bogus'。可选值: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi, mcode, kimi, dsh, agy, omp, copilot, workbuddy。 (Unsupported app id: 'bogus'. Allowed: claude, claude-desktop, codex, gemini, grokbuild, opencode, openclaw, hermes, pi, mcode, kimi, dsh, agy, omp, copilot, workbuddy.)"
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
