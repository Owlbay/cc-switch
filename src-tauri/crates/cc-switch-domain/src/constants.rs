//! 跨模块共享的常量。

/// 内置 Codex 官方供应商的固定 ID
pub const CODEX_OFFICIAL_PROVIDER_ID: &str = "codex-official";
/// 内置 Grok Build 官方供应商的固定 ID
pub const GROKBUILD_OFFICIAL_PROVIDER_ID: &str = "grokbuild-official";

/// 计价模型来源：按上游响应中的模型计价
pub const PRICING_SOURCE_RESPONSE: &str = "response";
/// 计价模型来源：按请求中的模型计价
pub const PRICING_SOURCE_REQUEST: &str = "request";

/// 模型 ID 中表示 1M 上下文的标记
pub const ONE_M_CONTEXT_MARKER: &str = "[1m]";
