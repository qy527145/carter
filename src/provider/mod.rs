//! 能力抽象层 —— agent loop 唯一依赖的 LLM 接口。
//!
//! 底座是 **llm_adapt**：三种线协议、SSE 切帧、内容块装配、缓存断点全在那边。
//! 这里只保留 agent loop 真正需要的那一点形状，并且把错误类型换成 carter 自己的，
//! 好让上层不必认识底座。
//!
//! 纪律：本目录不得 import 任何具体的 HTTP / 协议实现类型（`reqwest`、各 codec 等）。
//! 底座可换，上层不动 —— 这条从第一天就有，现在换掉 genai 也没有例外。

pub mod llm_log;
pub mod wire;

use std::pin::Pin;

use futures::Stream;

use crate::error::Result;

// 协议与 IR 的词汇直接从底座再导出，避免 carter 各处散落 `llm_adapt_core::` 前缀。
pub use llm_adapt_core::{
    Block, BlockAssembler, CacheUsage, CacheWriteUsage, FinishReason, ImageSource, Protocol,
    ProtocolId, Role, StreamChunk, ThinkingConfig, ToolChoice, ToolResultContent, ToolSpec, Usage,
    WireMessage, WireRequest,
};

/// 一次待执行的工具调用。
///
/// 从 IR 的 [`Block::ToolUse`] 提取出来：执行层（`ToolRegistry`、hooks、检查点）不该
/// 认识 IR，只需要 id / 名字 / 参数这三样。模型给的 `input` 已经在装配阶段解析成 JSON。
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

impl ToolCall {
    /// 从装配好的助手内容块里按出现顺序取出全部工具调用。
    ///
    /// 认不出的块（`Unmodeled`）**不算**工具调用 —— 那些是厂商在服务端自己执行完的
    /// 工具活动（`web_search_call` 之类），本地没有可执行的东西，硬当成工具去派发
    /// 只会得到一个「找不到工具」的错误回灌给模型。
    pub fn collect(blocks: &[Block]) -> Vec<ToolCall> {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse {
                    id, name, input, ..
                } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// 一次推理请求。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 走哪种线协议。`None` = 用渠道配置里的默认协议。
    pub protocol: Option<ProtocolId>,
    /// 协议无关的请求体。工具、多模态、思考、缓存断点都在里面。
    pub wire: WireRequest,
}

/// 归一化事件流。
///
/// 元素是底座的 [`StreamChunk`]（带块序号与块边界），错误已映射成 carter 的错误类型。
pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>;

/// 能力抽象 trait。底座可换，上层不动。
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// 流式推理，返回归一化事件流。
    async fn stream(&self, req: ChatRequest) -> Result<EventStream>;
}
