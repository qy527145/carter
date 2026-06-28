//! genai 底座实现。**唯一**允许 import `genai::*` 的文件；
//! 负责自研类型 ↔ genai 类型的双向映射，把 genai 事件归一到我们的 `Event`。

use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest as GenaiChatRequest, ChatStreamEvent,
    ReasoningEffort as GenaiReasoning, StopReason as GenaiStopReason, Tool as GenaiTool,
    ToolCall as GenaiToolCall, ToolResponse as GenaiToolResponse, Usage as GenaiUsage,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};

use super::{llm_log, ChatRequest, Event, EventStream, LlmProvider, Message, StopReason, ToolSpec, Usage};
use crate::config::ProviderConfig;
use crate::error::CarterError;
use crate::registry::ReasoningEffort;

/// 基于 genai 的 provider。
pub struct GenaiProvider {
    client: Client,
    /// 开启后把每次「请求+响应」写到独立的按天 jsonl（见 llm_log）。
    debug: bool,
    /// LLM 日志目录。
    log_dir: std::path::PathBuf,
    /// 日志里的人类可读标识（kind @ endpoint）。
    label: String,
    /// 配置的端点（base_url）；genai 默认时为 None。
    endpoint: Option<String>,
    /// 协议适配 kind（anthropic/openai/...）。
    adapter: String,
}

impl GenaiProvider {
    /// 走 genai 默认行为（按 model 名推断 adapter + 默认环境变量 key）。
    pub fn new(debug: bool, log_dir: std::path::PathBuf) -> Self {
        Self {
            client: Client::default(),
            debug,
            log_dir,
            label: "genai-default".to_string(),
            endpoint: None,
            adapter: "auto".to_string(),
        }
    }

    /// 配置驱动构建：用 `ProviderConfig` 的 kind/base_url/api_key 覆盖
    /// genai 的 endpoint/auth/adapter。这样就能接 anthropic-messages 格式的
    /// 自定义端点，或 OpenAI 兼容的第三方/自托管端点。
    pub fn from_provider_config(
        cfg: &ProviderConfig,
        debug: bool,
        log_dir: std::path::PathBuf,
    ) -> crate::Result<Self> {
        let adapter = adapter_kind_from_str(&cfg.kind)
            .ok_or_else(|| CarterError::Config(format!("unknown provider kind: {}", cfg.kind)))?;

        // 把需要的字段克隆进 resolver 闭包（闭包要求 'static + Clone）。
        let base_url = cfg.base_url.clone();
        let api_key = cfg.api_key.clone();
        let label = format!(
            "{} @ {}",
            cfg.kind,
            cfg.base_url.as_deref().unwrap_or("<default endpoint>")
        );

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
        Ok(Self {
            client,
            debug,
            log_dir,
            label,
            endpoint: cfg.base_url.clone(),
            adapter: cfg.kind.clone(),
        })
    }
}

impl Default for GenaiProvider {
    fn default() -> Self {
        Self::new(false, crate::config::paths::llm_log_dir())
    }
}

#[async_trait::async_trait]
impl LlmProvider for GenaiProvider {
    async fn stream(&self, req: ChatRequest) -> crate::Result<EventStream> {
        let mut greq = GenaiChatRequest::default();
        // system 分段 → genai 的 system-role 消息（按序，顺序由 iter_systems 保留）。
        // 在最后一段打 cache_control 断点：既触发 Anthropic 的 system **数组**多段格式
        // （genai 仅当任一 system 段带 cache_control 时才输出数组，否则拼成单串），
        // 又把整个 system 前缀纳入 prompt cache（系统段在单次会话内是稳定的）。
        let sys_last = req.system.len().saturating_sub(1);
        for (i, seg) in req.system.iter().enumerate() {
            let mut m = ChatMessage::system(seg.clone());
            if i == sys_last {
                m = m.with_options(CacheControl::Ephemeral);
            }
            greq = greq.append_message(m);
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

        // 调试：开启后组装请求 JSON，流式累积响应，流末写一条「请求+响应」交换记录。
        let debug = self.debug;
        let log_dir = self.log_dir.clone();
        let label = self.label.clone();
        let request_json = if debug {
            Some(build_request_json(&self.endpoint, &self.adapter, &req))
        } else {
            None
        };

        let res = self
            .client
            .exec_chat_stream(req.model_api_name.as_str(), greq, Some(&opts))
            .await
            .map_err(|e| CarterError::Provider(e.to_string()))?;

        // 累积流式响应（仅 debug 时填充）：合并后的正文 / 思考。
        let mut acc_text = String::new();
        let mut acc_think = String::new();

        // 把 genai 的 ChatStreamEvent 流归一到 Event 流。
        // 工具调用必须从 StreamEnd 的 captured 取（流式 ToolCallChunk 的 fn_arguments
        // 是分片累积的 String，未 parse 成 JSON；captured 在流末已拼好并 parse）。
        // 因此逐 ToolCallChunk 不 emit，仅在 End 展开成 [ToolCall*, Usage, Done]。
        let mapped = res.stream.flat_map(move |ev| {
            let events: Vec<crate::Result<Event>> = match ev {
                Ok(ChatStreamEvent::Chunk(c)) => {
                    if debug {
                        acc_text.push_str(&c.content);
                    }
                    vec![Ok(Event::TextDelta(c.content))]
                }
                Ok(ChatStreamEvent::ReasoningChunk(c)) => {
                    if debug {
                        acc_think.push_str(&c.content);
                    }
                    vec![Ok(Event::ThinkingDelta(c.content))]
                }
                Ok(ChatStreamEvent::End(end)) => {
                    let mut out: Vec<crate::Result<Event>> = Vec::new();
                    // 完整工具调用（流末已拼好）。同时收集供日志。
                    let mut tool_log: Vec<serde_json::Value> = Vec::new();
                    if let Some(calls) = end.captured_tool_calls() {
                        for tc in calls {
                            if debug {
                                tool_log.push(serde_json::json!({
                                    "id": tc.call_id,
                                    "name": tc.fn_name,
                                    "args": tc.fn_arguments,
                                }));
                            }
                            out.push(Ok(Event::ToolCall(super::ToolCall {
                                id: tc.call_id.clone(),
                                name: tc.fn_name.clone(),
                                args: tc.fn_arguments.clone(),
                            })));
                        }
                    }
                    // 用量。
                    let mut usage_log = None;
                    if let Some(u) = end.captured_usage {
                        let usage = from_genai_usage(&u);
                        usage_log = Some(usage.clone());
                        out.push(Ok(Event::Usage(usage)));
                    }
                    // 终止原因。
                    let reason = end
                        .captured_stop_reason
                        .map(from_genai_stop_reason)
                        .unwrap_or(StopReason::EndTurn);
                    // 写一条完整交换（请求 + 合并后的响应）。
                    if let Some(req_json) = &request_json {
                        let response_json = serde_json::json!({
                            "stop": format!("{reason:?}"),
                            "text": acc_text,
                            "thinking": if acc_think.is_empty() { None } else { Some(acc_think.clone()) },
                            "tool_calls": tool_log,
                            "usage": usage_log.as_ref().map(|u: &Usage| serde_json::json!({
                                "input": u.input, "output": u.output,
                                "cache_read": u.cache_read, "cache_write": u.cache_write,
                                "reasoning": u.reasoning,
                            })),
                        });
                        let entry = serde_json::json!({
                            "ts": llm_log::iso_utc(crate::session::now_ms()),
                            "provider": label,
                            "request": req_json,
                            "response": response_json,
                        });
                        llm_log::write_exchange(&log_dir, &entry);
                    }
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

/// 组装请求侧 JSON（carter 归一化视图，非逐字节 wire）。
fn build_request_json(
    endpoint: &Option<String>,
    adapter: &str,
    req: &ChatRequest,
) -> serde_json::Value {
    serde_json::json!({
        "endpoint": endpoint,
        "method": "POST",
        "adapter": adapter,
        "model": req.model_api_name,
        "max_output_tokens": req.max_output_tokens,
        "reasoning": req.reasoning.as_ref().map(|r| format!("{r:?}")),
        "system": req.system,
        "messages": req.messages,
        "tools": req.tools,
    })
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
    // 已知上游缺陷（genai 0.6.5，最新版）：Anthropic 流式 usage 在 streamer.rs 里对
    // message_start 与 message_delta 两处的 input/output 都做 `+=` 累加，而非取最后值。
    // 标准 Anthropic 仅在 message_start 报 input，故一般只多加一次 output 初值（≈1）；
    // 但当端点/网关在 message_delta 里也回 input_tokens 时，input 会被翻倍（实测 4219→8438）。
    // genai 只在流末暴露累加后的 captured_usage，映射层无法还原 → 仅影响显示的用量/成本估算，
    // 不影响真实 API 计费。修复需 patch genai（把 `+=` 改成取 max），暂记录不动。
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
