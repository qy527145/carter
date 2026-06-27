//! genai 底座实现。**唯一**允许 import `genai::*` 的文件；
//! 负责自研类型 ↔ genai 类型的双向映射，把 genai 事件归一到我们的 `Event`。

use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest as GenaiChatRequest, ChatStreamEvent,
    ReasoningEffort as GenaiReasoning, StopReason as GenaiStopReason, Tool as GenaiTool,
    ToolCall as GenaiToolCall, ToolResponse as GenaiToolResponse, Usage as GenaiUsage,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};

use super::{ChatRequest, Event, EventStream, LlmProvider, Message, StopReason, ToolSpec, Usage};
use crate::config::ProviderConfig;
use crate::error::CarterError;
use crate::registry::ReasoningEffort;

/// 基于 genai 的 provider。
pub struct GenaiProvider {
    client: Client,
}

impl GenaiProvider {
    /// 走 genai 默认行为（按 model 名推断 adapter + 默认环境变量 key）。
    pub fn new() -> Self {
        Self {
            client: Client::default(),
        }
    }

    /// 配置驱动构建：用 `ProviderConfig` 的 kind/base_url/api_key 覆盖
    /// genai 的 endpoint/auth/adapter。这样就能接 anthropic-messages 格式的
    /// 自定义端点，或 OpenAI 兼容的第三方/自托管端点。
    pub fn from_provider_config(cfg: &ProviderConfig) -> crate::Result<Self> {
        let adapter = adapter_kind_from_str(&cfg.kind)
            .ok_or_else(|| CarterError::Config(format!("unknown provider kind: {}", cfg.kind)))?;

        // 把需要的字段克隆进 resolver 闭包（闭包要求 'static + Clone）。
        let base_url = cfg.base_url.clone();
        let api_key = cfg.api_key.clone();

        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |mut target: ServiceTarget| -> genai::resolver::Result<ServiceTarget> {
                // 强制 adapter 协议（如 Anthropic native messages）。
                target.model = ModelIden::new(adapter, target.model.model_name.clone());
                if let Some(url) = &base_url {
                    target.endpoint = Endpoint::from_owned(url.clone());
                }
                if let Some(key) = &api_key {
                    target.auth = AuthData::from_single(key.clone());
                }
                Ok(target)
            },
        );

        let client = Client::builder()
            .with_service_target_resolver(resolver)
            .build();
        Ok(Self { client })
    }
}

impl Default for GenaiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LlmProvider for GenaiProvider {
    async fn stream(&self, req: ChatRequest) -> crate::Result<EventStream> {
        let mut greq = GenaiChatRequest::default();
        if let Some(sys) = &req.system {
            greq = greq.with_system(sys.clone());
        }
        for msg in &req.messages {
            greq = greq.append_message(to_genai_message(msg));
        }
        // 下发工具（支持工具的模型才会带）。
        if !req.tools.is_empty() {
            greq = greq.with_tools(req.tools.iter().map(to_genai_tool).collect::<Vec<_>>());
        }

        let mut opts = ChatOptions::default()
            .with_capture_usage(true)
            .with_capture_reasoning_content(true)
            .with_capture_tool_calls(true);
        if let Some(max) = req.max_output_tokens {
            opts = opts.with_max_tokens(max);
        }
        if let Some(effort) = &req.reasoning {
            opts = opts.with_reasoning_effort(to_genai_reasoning(effort));
        }

        let res = self
            .client
            .exec_chat_stream(req.model_api_name.as_str(), greq, Some(&opts))
            .await
            .map_err(|e| CarterError::Provider(e.to_string()))?;

        // 把 genai 的 ChatStreamEvent 流归一到 Event 流。
        // 工具调用必须从 StreamEnd 的 captured 取（流式 ToolCallChunk 的 fn_arguments
        // 是分片累积的 String，未 parse 成 JSON；captured 在流末已拼好并 parse）。
        // 因此逐 ToolCallChunk 不 emit，仅在 End 展开成 [ToolCall*, Usage, Done]。
        let mapped = res.stream.flat_map(|ev| {
            let events: Vec<crate::Result<Event>> = match ev {
                Ok(ChatStreamEvent::Chunk(c)) => vec![Ok(Event::TextDelta(c.content))],
                Ok(ChatStreamEvent::ReasoningChunk(c)) => {
                    vec![Ok(Event::ThinkingDelta(c.content))]
                }
                Ok(ChatStreamEvent::End(end)) => {
                    let mut out: Vec<crate::Result<Event>> = Vec::new();
                    // 1) 完整工具调用（流末已拼好）。
                    if let Some(calls) = end.captured_tool_calls() {
                        for tc in calls {
                            out.push(Ok(Event::ToolCall(super::ToolCall {
                                id: tc.call_id.clone(),
                                name: tc.fn_name.clone(),
                                args: tc.fn_arguments.clone(),
                            })));
                        }
                    }
                    // 2) 用量。
                    if let Some(u) = end.captured_usage {
                        out.push(Ok(Event::Usage(from_genai_usage(&u))));
                    }
                    // 3) 终止原因。
                    let reason = end
                        .captured_stop_reason
                        .map(from_genai_stop_reason)
                        .unwrap_or(StopReason::EndTurn);
                    out.push(Ok(Event::Done(reason)));
                    out
                }
                // Start / ToolCallChunk（分片，忽略）/ ThoughtSignatureChunk：过滤。
                Ok(_) => vec![],
                Err(e) => vec![Err(CarterError::Provider(e.to_string()))],
            };
            futures::stream::iter(events)
        });

        Ok(Box::pin(mapped))
    }
}

/// `kind` 字段 → genai AdapterKind。对应配置里 [providers.*].kind。
/// openai_responses → OpenAIResp；openai_compat/custom → OpenAI（兼容协议）。
fn adapter_kind_from_str(kind: &str) -> Option<AdapterKind> {
    match kind {
        "anthropic" => Some(AdapterKind::Anthropic),
        "openai" => Some(AdapterKind::OpenAI),
        "openai_responses" => Some(AdapterKind::OpenAIResp),
        "gemini" => Some(AdapterKind::Gemini),
        "openai_compat" => Some(AdapterKind::OpenAI),
        "ollama" => Some(AdapterKind::Ollama),
        "groq" => Some(AdapterKind::Groq),
        "deepseek" => Some(AdapterKind::DeepSeek),
        "xai" => Some(AdapterKind::Xai),
        "openrouter" => Some(AdapterKind::OpenRouter),
        // custom 逃生舱当前也按 OpenAI 兼容协议处理（后续可接自研实现）。
        "custom" => Some(AdapterKind::OpenAI),
        _ => None,
    }
}

fn to_genai_message(msg: &Message) -> ChatMessage {
    match msg {
        Message::System(s) => ChatMessage::system(s.clone()),
        Message::User(s) => ChatMessage::user(s.clone()),
        Message::Assistant(s) => ChatMessage::assistant(s.clone()),
        // assistant 发起的工具调用：转成 genai ToolCall 列表（assistant 役）。
        Message::ToolCalls(calls) => {
            let gcalls: Vec<GenaiToolCall> = calls
                .iter()
                .map(|tc| GenaiToolCall {
                    call_id: tc.id.clone(),
                    fn_name: tc.name.clone(),
                    fn_arguments: tc.args.clone(),
                    thought_signatures: None,
                })
                .collect();
            ChatMessage::from(gcalls)
        }
        // 工具执行结果：tool 役 ToolResponse。
        Message::Tool { call_id, content } => {
            ChatMessage::from(GenaiToolResponse::new(call_id.clone(), content.clone()))
        }
    }
}

/// 自研 ToolSpec → genai Tool。
fn to_genai_tool(spec: &ToolSpec) -> GenaiTool {
    GenaiTool::new(spec.name.clone())
        .with_description(spec.description.clone())
        .with_schema(spec.parameters.clone())
}

fn to_genai_reasoning(effort: &ReasoningEffort) -> GenaiReasoning {
    match effort {
        ReasoningEffort::None => GenaiReasoning::None,
        ReasoningEffort::Low => GenaiReasoning::Low,
        ReasoningEffort::Medium => GenaiReasoning::Medium,
        ReasoningEffort::High => GenaiReasoning::High,
        ReasoningEffort::XHigh => GenaiReasoning::XHigh,
        ReasoningEffort::Max => GenaiReasoning::Max,
        ReasoningEffort::Budget(n) => GenaiReasoning::Budget(*n),
    }
}

fn from_genai_stop_reason(r: GenaiStopReason) -> StopReason {
    match r {
        GenaiStopReason::ToolCall(_) => StopReason::ToolUse,
        GenaiStopReason::MaxTokens(_) => StopReason::MaxTokens,
        GenaiStopReason::Completed(_) => StopReason::EndTurn,
        GenaiStopReason::StopSequence(_) => StopReason::Stop,
        GenaiStopReason::ContentFilter(s) | GenaiStopReason::Other(s) => StopReason::Other(s),
    }
}

fn from_genai_usage(u: &GenaiUsage) -> Usage {
    let to_u64 = |v: Option<i32>| v.unwrap_or(0).max(0) as u64;
    let prompt_details = u.prompt_tokens_details.as_ref();
    let completion_details = u.completion_tokens_details.as_ref();
    Usage {
        input: to_u64(u.prompt_tokens),
        output: to_u64(u.completion_tokens),
        cache_read: to_u64(prompt_details.and_then(|d| d.cached_tokens)),
        cache_write: to_u64(prompt_details.and_then(|d| d.cache_creation_tokens)),
        reasoning: to_u64(completion_details.and_then(|d| d.reasoning_tokens)),
    }
}
