//! CC Switch 共享领域类型与纯函数。
//!
//! 本 crate 不做任何 IO，不依赖 Tauri / SQLite，供桌面 app 与 `cc-proxy-core` 共用。
//! 拆分计划见 `docs/standalone-proxy-core-design-zh.md`（P1 起逐步迁入）。

mod app_type;
mod error;

pub use app_type::AppType;
pub use error::DomainError;
