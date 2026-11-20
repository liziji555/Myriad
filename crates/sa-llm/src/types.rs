use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 模型要调用的一个工具。`arguments` 是模型给出的原始 JSON 文本，可能不合法。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// 仅 assistant：这一轮的思考内容。带工具的请求里 DeepSeek 要求原样传回。
    pub reasoning: Option<String>,
    /// 仅 assistant。
    pub tool_calls: Vec<ToolCall>,
    /// 仅 tool：这是哪次调用的结果。
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn plain(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content)
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            ..Self::plain(Role::Tool, content)
        }
    }
}

/// 提供给模型的一个工具。`parameters` 是 JSON Schema。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    /// 是否开启思考模式。`None` 表示用服务商的默认值。
    pub thinking: Option<bool>,
    /// 这次请求属于哪个会话。同一个会话始终用同一个值；有的网关靠它做路由和提示词缓存。
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// 输入 token 总数，包含命中缓存的部分。
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_tokens += other.cached_tokens;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    /// 输出被长度上限截断。
    MaxTokens,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallArgsDelta { id: String, json_fragment: String },
    Usage(Usage),
    Done { stop: StopReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCaps {
    pub context_window: u32,
    pub supports_tools: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    #[error("模型服务拒绝了访问凭证：{0}")]
    Auth(String),
    #[error("模型服务不接受这个请求：{0}")]
    BadRequest(String),
    #[error("模型服务限流了：{0}")]
    RateLimited(String),
    #[error("模型服务出错了：{0}")]
    Server(String),
    #[error("连不上模型服务：{0}")]
    Network(String),
    #[error("模型服务太久没有回应")]
    Timeout,
    #[error("模型服务返回了看不懂的内容：{0}")]
    Protocol(String),
    #[error("已取消")]
    Cancelled,
}

impl LlmError {
    /// 过一会儿再试有没有可能成功。凭证和参数错误重试没有意义。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited(_) | Self::Server(_) | Self::Network(_) | Self::Timeout
        )
    }
}

/// 把一串 [`StreamEvent`] 拼成完整的一轮回复。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Accumulator {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub stop: Option<StopReason>,
}

impl Accumulator {
    pub fn push(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta(text) => self.text.push_str(text),
            StreamEvent::ReasoningDelta(text) => self.reasoning.push_str(text),
            StreamEvent::ToolCallStart { id, name } => self.tool_calls.push(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: String::new(),
            }),
            StreamEvent::ToolCallArgsDelta { id, json_fragment } => {
                if let Some(call) = self.tool_calls.iter_mut().find(|c| &c.id == id) {
                    call.arguments.push_str(json_fragment);
                }
            }
            StreamEvent::Usage(usage) => self.usage = Some(*usage),
            StreamEvent::Done { stop } => self.stop = Some(stop.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_assembles_interleaved_tool_calls() {
        let mut acc = Accumulator::default();
        for event in [
            StreamEvent::ReasoningDelta("想".into()),
            StreamEvent::TextDelta("我来".into()),
            StreamEvent::TextDelta("看看".into()),
            StreamEvent::ToolCallStart {
                id: "a".into(),
                name: "read".into(),
            },
            StreamEvent::ToolCallStart {
                id: "b".into(),
                name: "grep".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                id: "b".into(),
                json_fragment: "{\"pattern\":".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                id: "a".into(),
                json_fragment: "{}".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                id: "b".into(),
                json_fragment: "\"x\"}".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                id: "missing".into(),
                json_fragment: "ignored".into(),
            },
            StreamEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 4,
                cached_tokens: 2,
            }),
            StreamEvent::Done {
                stop: StopReason::ToolUse,
            },
        ] {
            acc.push(&event);
        }
        assert_eq!(acc.text, "我来看看");
        assert_eq!(acc.reasoning, "想");
        assert_eq!(acc.tool_calls[0].arguments, "{}");
        assert_eq!(acc.tool_calls[1].arguments, "{\"pattern\":\"x\"}");
        assert_eq!(acc.usage.unwrap().input_tokens, 10);
        assert_eq!(acc.stop, Some(StopReason::ToolUse));
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        assert!(LlmError::RateLimited("x".into()).is_retryable());
        assert!(LlmError::Server("x".into()).is_retryable());
        assert!(LlmError::Network("x".into()).is_retryable());
        assert!(LlmError::Timeout.is_retryable());
        assert!(!LlmError::Auth("x".into()).is_retryable());
        assert!(!LlmError::BadRequest("x".into()).is_retryable());
        assert!(!LlmError::Protocol("x".into()).is_retryable());
        assert!(!LlmError::Cancelled.is_retryable());
    }
}
