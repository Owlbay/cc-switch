//! CC Switch 中转的协议转换组件：通过统一中间表示（IR）在 Anthropic Messages、
//! OpenAI Chat Completions、OpenAI Responses、Gemini 之间转换。
//!
//! 后续实现 `cc_proxy_core::convert::Converter`，由可执行程序在构造中转时注入；没有配置
//! 转换的上游不经过本 crate，仍逐字节透传。设计见 `docs/protocol-conversion-design-zh.md`。

pub mod envelope;
pub mod ir;
pub mod sse;
