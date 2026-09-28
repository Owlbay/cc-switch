use thiserror::Error;

/// domain 层的错误类型。
///
/// app 侧通过 `impl From<DomainError> for AppError` 映射到同名变体；两边的 `#[error]`
/// 模板必须逐字一致（见 `src-tauri/src/error.rs`），保证错误文本不因迁移而变化。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    /// 对应 `AppError::Localized`
    #[error("{zh} ({en})")]
    Localized {
        key: &'static str,
        zh: String,
        en: String,
    },
    /// 对应 `AppError::Message`
    #[error("{0}")]
    Message(String),
}

impl DomainError {
    pub fn localized(key: &'static str, zh: impl Into<String>, en: impl Into<String>) -> Self {
        Self::Localized {
            key,
            zh: zh.into(),
            en: en.into(),
        }
    }
}
