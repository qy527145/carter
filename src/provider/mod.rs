//! 能力抽象层 —— 自研 `LlmProvider` trait 与统一入参类型。
//! agent loop 唯一依赖的接口；底座（genai/逃生舱）实现此 trait。

mod event;
pub mod genai_provider;

pub use event::{Event, StopReason, ToolCall, Usage};

use futures::Stream;
use std::pin::Pin;

use crate::registry::ReasoningEffort;

/// 一次推理请求（自研类型，不暴露底座类型）。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 模型的 API 名（已由 registry 解析自别名）。
    pub model_api_name: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    /// 工具定义（支持工具的模型才下发）。
    pub tools: Vec<ToolSpec>,
    pub reasoning: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u32>,
}

/// 会话消息。
#[allow(dead_code)] // System 变体预留给 system prompt 注入
#[derive(Debug, Clone)]
pub enum Message {
    System(String),
    User(String),
    Assistant(String),
    /// assistant 发起的工具调用（多轮回灌历史用）。
    ToolCalls(Vec<ToolCall>),
    /// 工具执行结果（回灌给模型）。
    Tool { call_id: String, content: String },
}

/// 工具规格（下发给模型的工具声明）。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// 归一化事件流。
pub type EventStream = Pin<Box<dyn Stream<Item = crate::Result<Event>> + Send>>;

/// 能力抽象 trait。底座可换，上层不动。
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// 流式推理，返回归一化事件流。
    async fn stream(&self, req: ChatRequest) -> crate::Result<EventStream>;
}
