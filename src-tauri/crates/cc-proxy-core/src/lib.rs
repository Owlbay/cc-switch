//! CC Switch 模型接口透传中转引擎。
//!
//! 把 Anthropic Messages、OpenAI Chat Completions、OpenAI Responses、Gemini 四个接口的
//! 请求原样转发给配置的上游，并把响应原样流回。core 不修改任何语义内容；需要改写时
//! 由独立的拦截器组件接入。不依赖 Tauri / SQLite。
//! 设计见 `docs/passthrough-relay-design-zh.md`。

pub mod access_log;
pub mod auth;
pub mod breaker;
pub mod config;
pub mod convert;
pub mod error;
pub mod forward;
pub mod headers;
pub mod idle;
pub mod interface;
pub mod server;
pub mod tls;
pub mod upstream;
pub mod upstream_url;

pub use config::RelayConfig;
pub use forward::Relay;
pub use interface::{identify, Interface};
pub use server::serve;
