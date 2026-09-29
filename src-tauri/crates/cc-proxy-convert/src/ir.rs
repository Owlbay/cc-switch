//! 统一中间表示（设计文档 §4）。
//!
//! 每种协议只实现“解析为 IR”与“从 IR 生成”两个方向，任意两种协议即可互转。
//! IR 只建模跨协议有意义的字段，未建模的字段在转换中丢弃（§5.4）。

use cc_proxy_core::Interface;
use serde_json::Value;

/// 统一请求
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Request {
    pub model: String,
    /// 系统提示（仅文本）
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub tool_choice: ToolChoice,
    pub parallel_tool_calls: Option<bool>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop: Vec<String>,
    pub stream: bool,
    pub reasoning: Reasoning,
    pub user: Option<String>,
    /// 请求中声明、但被映射为上游内置能力或剔除的客户端 server tool
    pub server_tools: Vec<ServerTool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaSource {
    Base64 { media_type: String, data: String },
    Url(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        source: MediaSource,
    },
    Document {
        source: MediaSource,
        name: Option<String>,
    },
    /// `arguments` 为 JSON 文本，原样保存，不解析再序列化
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        signature: Option<Signature>,
    },
    ToolResult {
        call_id: String,
        content: Vec<Block>,
        is_error: bool,
    },
    Thinking {
        text: String,
        signature: Option<Signature>,
    },
    /// 只有不透明数据、没有可见文本的推理块
    RedactedThinking {
        signature: Signature,
    },
}

/// 上游返回的、要求在后续请求中原样回放的不透明值（§5.5）
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    /// 产生它的上游协议
    pub source: Interface,
    /// 原始值：Anthropic 为整个 thinking / redacted_thinking 块（JSON），Responses 为整个
    /// reasoning 项（JSON），Gemini 为 `thoughtSignature` 字符串
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
    /// 客户端以自定义（非 function）形式声明（Responses `type: custom`），参数降级为 `{input: string}`
    pub custom: bool,
}

/// 客户端声明的内置工具（如 Anthropic `web_search_*`）
#[derive(Debug, Clone, PartialEq)]
pub struct ServerTool {
    pub kind: ServerToolKind,
    /// 客户端协议中的原始定义
    pub raw: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerToolKind {
    WebSearch,
    Other,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
        }
    }

    /// 解析 effort 字符串；`max` / `ultra` 视为 `xhigh`
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minimal" => Some(Effort::Minimal),
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" | "max" | "ultra" => Some(Effort::XHigh),
            _ => None,
        }
    }

    /// effort → thinking 预算（§5.1 换算表）
    pub fn budget_tokens(self) -> u64 {
        match self {
            Effort::Minimal | Effort::Low => 2048,
            Effort::Medium => 8192,
            Effort::High => 16384,
            Effort::XHigh => 24576,
        }
    }

    /// thinking 预算 → 最接近的 effort 档
    pub fn from_budget(budget: u64) -> Self {
        [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh]
            .into_iter()
            .min_by_key(|effort| effort.budget_tokens().abs_diff(budget))
            .unwrap_or(Effort::Medium)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reasoning {
    /// 客户端没有表态，由上游决定
    #[default]
    Unspecified,
    Disabled,
    Effort(Effort),
    Budget(u64),
    /// Anthropic adaptive thinking（`output_config.effort` 可选）
    Adaptive(Option<Effort>),
}

impl Reasoning {
    /// 折算为 effort（给只认 effort 的协议）；Disabled / Unspecified 为 None
    pub fn effort(self) -> Option<Effort> {
        match self {
            Reasoning::Unspecified | Reasoning::Disabled => None,
            Reasoning::Effort(effort) => Some(effort),
            Reasoning::Budget(budget) => Some(Effort::from_budget(budget)),
            Reasoning::Adaptive(effort) => Some(effort.unwrap_or(Effort::Medium)),
        }
    }
}

/// 统一非流式响应
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub content: Vec<Block>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Refusal,
    Other(String),
}

/// token 用量。三桶互斥：`input_tokens` 不含缓存读写（Anthropic 语义）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// 含缓存读写的输入总数（OpenAI / Gemini 的 prompt 语义）
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// 块类型（流式）
#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Text,
    Thinking,
    RedactedThinking,
    ToolCall { id: String, name: String },
}

/// 规范化的流式事件（§4.3）：每个块有 BlockStart / BlockStop 配对，index 从 0 递增，
/// Finish 恰好一次且在所有 BlockStop 之后。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Start {
        id: String,
        model: String,
        usage: Usage,
    },
    BlockStart {
        index: usize,
        kind: BlockKind,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    SignatureDelta {
        index: usize,
        signature: Signature,
    },
    ToolArgumentsDelta {
        index: usize,
        partial_json: String,
    },
    BlockStop {
        index: usize,
    },
    Finish {
        stop_reason: StopReason,
        usage: Usage,
    },
    Error {
        kind: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_budget_mapping() {
        assert_eq!(Effort::Low.budget_tokens(), 2048);
        assert_eq!(Effort::High.budget_tokens(), 16384);
        assert_eq!(Effort::from_budget(1024), Effort::Low);
        assert_eq!(Effort::from_budget(10000), Effort::Medium);
        assert_eq!(Effort::from_budget(16000), Effort::High);
        assert_eq!(Effort::from_budget(32000), Effort::XHigh);
        assert_eq!(Effort::parse("MAX"), Some(Effort::XHigh));
        assert_eq!(Effort::parse("none"), None);
    }

    #[test]
    fn reasoning_to_effort() {
        assert_eq!(Reasoning::Unspecified.effort(), None);
        assert_eq!(Reasoning::Disabled.effort(), None);
        assert_eq!(Reasoning::Budget(8000).effort(), Some(Effort::Medium));
        assert_eq!(Reasoning::Adaptive(None).effort(), Some(Effort::Medium));
        assert_eq!(
            Reasoning::Adaptive(Some(Effort::High)).effort(),
            Some(Effort::High)
        );
    }

    #[test]
    fn usage_total_input() {
        let usage = Usage {
            input_tokens: 10,
            cache_read_tokens: 5,
            cache_write_tokens: 2,
            ..Default::default()
        };
        assert_eq!(usage.total_input(), 17);
    }
}
