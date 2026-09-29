//! CC Switch 模型接口透传中转引擎。
//!
//! 把 Anthropic Messages、OpenAI Chat Completions、OpenAI Responses、Gemini 四个接口的
//! 请求原样转发给配置的上游，并把响应原样流回。core 不修改任何语义内容；需要改写时
//! 由独立的拦截器组件接入。不依赖 Tauri / SQLite。
//! 设计见 `docs/passthrough-relay-design-zh.md`。

pub mod interface;
pub mod upstream_url;

pub use interface::{identify, Interface};
