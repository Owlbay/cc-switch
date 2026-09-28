//! 用量统计的 token 语义与计价辅助（纯函数 / 常量）。

/// Set of `app_type` values whose stored `input_tokens` already includes
/// `cache_read_tokens`. Aggregations subtract cache reads from these rows
/// to recover the fresh-input semantics used by Claude.
///
/// Why list providers explicitly: new providers default to the
/// Claude-style "input excludes cache" semantics, which is safer if the
/// caller forgets to update this list. The wrong direction (a new OpenAI-
/// style provider not added here) shows up loudly as a too-low cache hit
/// rate, which is easier to catch than the silent over-deduction that
/// would happen with the opposite default.
/// 单一语义集（SSOT）：写入侧（proxy logger/calculator）、回填侧
/// （usage_stats 成本重算）与展示侧（sql_helpers 的 SQL 归一）都必须引用这里，
/// 防止同一语义散落多处后新增 app 时漏改（grokbuild 曾在回填侧漏掉）。
/// 前端 `src/types/usage.ts` 的同名常量是跨语言的对应物，改动须同步。
pub const CACHE_INCLUSIVE_APP_TYPES: &[&str] = &["codex", "gemini", "grokbuild"];

/// `app_type` 的存储 `input_tokens` 是否已包含 cache read/write。
pub fn is_cache_inclusive_app(app_type: &str) -> bool {
    CACHE_INCLUSIVE_APP_TYPES.contains(&app_type)
}

pub const INPUT_TOKEN_SEMANTICS_LEGACY: i64 = 0;
pub const INPUT_TOKEN_SEMANTICS_TOTAL: i64 = 1;
pub const INPUT_TOKEN_SEMANTICS_FRESH: i64 = 2;

/// 模型 ID 是否为占位值（空、unknown、null、none），这类模型不参与计价。
pub fn is_placeholder_pricing_model(model_id: &str) -> bool {
    let normalized = model_id.trim().to_ascii_lowercase();
    normalized.is_empty() || matches!(normalized.as_str(), "unknown" | "null" | "none")
}
