//! 上下文工程：token 估算 + 分级压缩（L1 清旧工具输出 / L2 滚动摘要）。
//! 纪律：本文件不得 import 任何具体底座类型；压缩的模型调用走 `LlmProvider` trait。

use futures::StreamExt;

use crate::provider::{
    Block, ChatRequest, LlmProvider, RequestMeta, RequestPurpose, ResponseFormat, Role,
    StreamChunk, ToolResultContent, WireMessage, WireRequest,
};
use crate::registry::ModelInfo;

use super::thread::Thread;
use super::ui::{UiEvent, UiSink};

/// L1 保护窗口：最近 N 条消息的工具输出不折叠。
const L1_KEEP_RECENT: usize = 6;
/// L2 必留的尾部消息条数（含原始任务首条另算）。
const L2_KEEP_RECENT: usize = 8;
/// L2 有损摘要的输出上限。
const SUMMARY_MAX_TOKENS: u32 = 2000;
/// L3 结构化高保真摘要的输出上限（比 L2 宽，保留更多细节）。
const L3_SUMMARY_MAX_TOKENS: u32 = 3000;

/// 估算一组消息的 token 数。默认走 cl100k_base（OpenAI 系精确、Anthropic 系合理近似）；
/// 用 `estimate_tokens_for` 可显式按 model.tokenizer 选编码器拿到更精确结果。
/// 仅用于"是否触发上下文压缩"的判定；真实计费仍以 provider 返回的 Usage 为准。
pub fn estimate_tokens(messages: &[WireMessage]) -> u64 {
    crate::tokens::estimate_messages(messages, "cl100k")
}

/// 按模型 tokenizer 估算 token 数（精确路径，供 turn.rs 在已知 model 时调用）。
pub fn estimate_tokens_for(messages: &[WireMessage], tokenizer: &str) -> u64 {
    crate::tokens::estimate_messages(messages, tokenizer)
}

/// 压缩 thread.messages：先 L1（清旧工具输出），不够再 L2（滚动摘要）。
/// `threshold` = 触发/目标 token 阈值（由调用方按 context_window * ratio 算好）。
/// 始终保留首条 user（原始任务）；todo 状态在 thread.todos，由复诵带回，不进摘要。
/// 摘要调用失败 → 降级为纯 L1，不中断（错误即数据）。
pub async fn compact(
    thread: &mut Thread,
    provider: &dyn LlmProvider,
    model: &ModelInfo,
    threshold: u64,
    ui: &mut dyn UiSink,
) -> crate::Result<()> {
    // L0：剥离较旧消息里的图片引用 token（多模态会话下 token 量大头）。
    // 真实图片每张 ≈ 1.5k tokens（1568px 边），保护窗口外的不再回灌避免反复占位。
    let imgs_stripped = strip_old_image_refs(&mut thread.messages, L1_KEEP_RECENT);
    if imgs_stripped > 0 {
        ui.emit(UiEvent::Notice(format!(
            "[compact] L0 stripped {imgs_stripped} image refs from older messages"
        )));
    }

    // L1：折叠较旧的工具输出。
    let before = estimate_tokens_for(&thread.messages, &model.tokenizer);
    elide_old_tool_outputs(&mut thread.messages, L1_KEEP_RECENT);
    let after_l1 = estimate_tokens_for(&thread.messages, &model.tokenizer);
    ui.emit(UiEvent::Notice(format!(
        "[compact] L1 elided old tool outputs: ~{before} → ~{after_l1} tokens (tiktoken {})",
        model.tokenizer
    )));

    // L1 已把估算降到阈值下 → 跳过 L2。
    if after_l1 <= threshold {
        return Ok(());
    }

    // L2：切分 head / middle / recent，对 middle 摘要。
    // 摘要请求归到这条会话（子 agent 的则归到它那条线）下，挂在最近一轮上。
    let meta = RequestMeta {
        session_id: thread.session_id.clone(),
        agent_id: thread.agent_id.clone(),
        turn: thread.user_turns.saturating_sub(1),
        round: 0,
        purpose: RequestPurpose::Compaction,
    };
    let Some((head, middle, recent)) = split_for_summary(&thread.messages, L2_KEEP_RECENT) else {
        // 历史太短无法切分（仅 head + recent），L1 已尽力。
        return Ok(());
    };
    if middle.is_empty() {
        return Ok(());
    }

    // L3 优先（结构化高保真）；仅当 L3 返回空/解析失败才回落 L2（传输错误大概率 L2 也失败，
    // 不浪费一次调用）；两者都拿不到 → 保留 L1 结果，不中断主循环。
    let tiered = match summarize_structured(&middle, provider, model, &meta).await {
        Ok(s) => Some(("L3", s)),
        Err(e3) => {
            ui.emit(UiEvent::Notice(format!(
                "[compact] L3 structured summary failed ({e3}); trying L2"
            )));
            match summarize(&middle, provider, model, &meta).await {
                Ok(s) => Some(("L2", s)),
                Err(e2) => {
                    ui.emit(UiEvent::Notice(format!(
                        "[compact] L2 summary failed ({e2}); kept L1-only result"
                    )));
                    None
                }
            }
        }
    };

    if let Some((tier, summary)) = tiered {
        let mut rebuilt = Vec::with_capacity(2 + recent.len());
        rebuilt.push(head);
        rebuilt.push(WireMessage::new(
            Role::Assistant,
            vec![Block::text(format!("【历史摘要·{tier}】\n{summary}"))],
        ));
        rebuilt.extend(recent);
        let after = estimate_tokens_for(&rebuilt, &model.tokenizer);
        thread.messages = rebuilt;
        // 落盘压缩后的全量快照（resume 重放据此整体替换，见 docs/04）。
        if let Some(rec) = thread.recorder() {
            rec.record(crate::session::RecordKind::Compacted {
                tier: tier.to_string(),
                messages: thread.messages.clone(),
            });
        }
        ui.emit(UiEvent::Notice(format!(
            "[compact] {tier} rolling summary applied: ~{after} tokens (est)"
        )));
    }
    Ok(())
}

/// L1：把保护窗口之外的工具结果大输出替换为占位摘要。纯函数，便于测试。
fn elide_old_tool_outputs(messages: &mut [WireMessage], keep_recent: usize) {
    let len = messages.len();
    if len <= keep_recent {
        return;
    }
    let cutoff = len - keep_recent;
    for msg in messages.iter_mut().take(cutoff) {
        for block in &mut msg.content {
            if let Block::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
                && let ToolResultContent::Text { text } = content
            {
                // 已折叠过的不再处理。
                if text.starts_with("[tool result elided") {
                    continue;
                }
                let bytes = text.len();
                *text = format!("[tool result elided: {bytes} bytes, call_id={tool_use_id}]");
            }
        }
    }
}

/// L0：把保护窗口之外的消息文本里的 `[img:<hash>.<ext>]` 引用替换为占位文本
/// `[image elided]`，让 provider 边界不再发对应的多模态块。返回剥离的引用条数。
///
/// 历史里存的是**引用 token** 而不是图片字节（否则会话 JSONL 会被 base64 撑爆），
/// 所以「丢掉旧图」在这里就是改写那些 token。最近 `keep_recent` 条不动 ——
/// 当前任务的图得让模型还看得到。
fn strip_old_image_refs(messages: &mut [WireMessage], keep_recent: usize) -> usize {
    let len = messages.len();
    if len <= keep_recent {
        return 0;
    }
    let cutoff = len - keep_recent;
    let mut stripped = 0usize;
    for msg in messages.iter_mut().take(cutoff) {
        for block in &mut msg.content {
            match block {
                Block::Text { text, .. } => stripped += strip_in_str(text),
                // `read_file` 读图回的是带引用的文本工具结果，provider 边界会把它
                // 展开成多模态块 —— 旧轮次的也得剔，不然图照样发出去。
                Block::ToolResult {
                    content: ToolResultContent::Text { text },
                    ..
                } => stripped += strip_in_str(text),
                _ => {}
            }
        }
    }
    stripped
}

/// 把单个字符串里的 `[img:<hex>.<alnum>]` 全部替换为 `[image elided]`，返回替换次数。
fn strip_in_str(s: &mut String) -> usize {
    let segs = crate::media::parse_segments(s);
    let img_count = segs
        .iter()
        .filter(|seg| matches!(seg, crate::media::Segment::Image(_)))
        .count();
    if img_count == 0 {
        return 0;
    }
    let mut out = String::with_capacity(s.len());
    for seg in segs {
        match seg {
            crate::media::Segment::Text(t) => out.push_str(&t),
            crate::media::Segment::Image(_) => out.push_str("[image elided]"),
        }
    }
    *s = out;
    img_count
}

/// L2 切分：首条 user（head，必留）+ 中段（middle，待摘要）+ 尾部 keep_recent 条（必留）。
/// 返回 None 表示无法有效切分（历史太短）。纯函数，便于测试。
fn split_for_summary(
    messages: &[WireMessage],
    keep_recent: usize,
) -> Option<(WireMessage, Vec<WireMessage>, Vec<WireMessage>)> {
    if messages.is_empty() {
        return None;
    }
    let head = messages[0].clone();
    let rest = &messages[1..];
    if rest.len() <= keep_recent {
        return None;
    }
    let split = rest.len() - keep_recent;
    let middle = rest[..split].to_vec();
    let recent = rest[split..].to_vec();
    Some((head, middle, recent))
}

/// 对中段历史调一次模型生成摘要。复用 provider.stream() 收集 TextDelta。
async fn summarize(
    middle: &[WireMessage],
    provider: &dyn LlmProvider,
    model: &ModelInfo,
    meta: &RequestMeta,
) -> crate::Result<String> {
    let rendered = render_messages(middle);
    let system = "你是上下文压缩器。把以下 agent 执行历史压成简洁要点，\
                  务必保留：已做的关键决策、尝试过的方案、重要发现/文件路径、未决问题与下一步。\
                  丢弃冗长的原始工具输出。用紧凑的中文列点。";
    run_prompt(
        &compact_input(&rendered),
        system,
        SUMMARY_MAX_TOKENS,
        ResponseFormat::Text,
        provider,
        model,
        meta,
    )
    .await
}

/// L3 结构化高保真摘要的 system 提示。固定分节，便于模型与人都能稳定解析。
fn structured_summary_system() -> &'static str {
    "你是上下文压缩器，做高保真结构化压缩。把以下 agent 执行历史压成下列固定分节的中文笔记，\
     每节用紧凑列点，保留具体细节（文件路径、函数名、关键数值、错误信息），丢弃冗长的原始工具输出：\n\
     ## 已完成的关键决策\n## 涉及的文件/路径\n## 未决问题\n## 下一步\n## 关键发现\n\
     即使某节暂无内容也要保留标题并写「（无）」。"
}

/// L3：用结构化模板生成高保真摘要。
async fn summarize_structured(
    middle: &[WireMessage],
    provider: &dyn LlmProvider,
    model: &ModelInfo,
    meta: &RequestMeta,
) -> crate::Result<String> {
    let rendered = render_messages(middle);
    run_prompt(
        &compact_input(&rendered),
        structured_summary_system(),
        L3_SUMMARY_MAX_TOKENS,
        ResponseFormat::Text,
        provider,
        model,
        meta,
    )
    .await
}

/// 压缩历史在用户消息里的抬头。
///
/// **只给压缩 / 摘要用。** 生成标题也走同一条单次请求的路，但那句用户消息是用户
/// 自己说的第一句话，不是「一段待压缩的历史」—— 给它加这个抬头，模型会一边被告知
/// 这是要压缩的历史、一边被要求在 system 提示里起标题，两边打架。
const COMPACT_HEADING: &str = "【待压缩历史】";

/// 把要压缩的历史包成用户消息。
fn compact_input(rendered: &str) -> String {
    format!("{COMPACT_HEADING}\n{rendered}")
}

/// 单次请求公共流程：发一次 system + 用户消息，收集 TextDelta，空输出视为失败。
///
/// `user_text` 由调用方准备好：压缩路径带 [`COMPACT_HEADING`] 抬头，起标题路径
/// 就是原样的那句话。`meta` 决定这次请求归到哪条会话 —— 它会变成请求头，网关
/// 按它归类。
async fn run_prompt(
    user_text: &str,
    system: &str,
    max_tokens: u32,
    format: ResponseFormat,
    provider: &dyn LlmProvider,
    model: &ModelInfo,
    meta: &RequestMeta,
) -> crate::Result<String> {
    let mut wire = WireRequest::new(model.api_name.clone());
    wire.system = vec![Block::text(system)];
    wire.messages = vec![WireMessage::user_text(user_text)];
    wire.max_tokens = Some(max_tokens);
    wire.response_format = format;
    wire.meta = meta.clone();
    let req = ChatRequest {
        protocol: None,
        wire,
    };

    let mut stream = provider.stream(req).await?;
    let mut summary = String::new();
    while let Some(ev) = stream.next().await {
        if let StreamChunk::TextDelta { text, .. } = ev? {
            summary.push_str(&text);
        }
    }
    if summary.trim().is_empty() {
        return Err(crate::error::CarterError::Provider(
            "summary returned empty text".to_string(),
        ));
    }
    Ok(summary)
}

/// 为会话生成简短标题：用首条 user prompt 调一次（fast）模型。
/// 整个会话只调一次（由调用方保证）。失败由调用方静默忽略。
///
/// `session_id` 是这个标题所属的会话：起标题的请求也算这条对话的一部分，
/// 网关要靠它把请求归进来。
///
/// ## 结构化输出，带降级
///
/// 先用 json_schema 约束输出（`{"title": "..."}`）—— 比「请只输出标题」稳：不会多出
/// 引号、解释、第二行。但不是每个渠道都认：中转站、OpenAI 兼容端点常常不支持
/// `response_format`，会 400；也有的收下了却照样回纯文本。所以：
///
/// 1. 渠道**拒绝**这个请求（400 / 422 / 参数校验 / 编码器说不支持）→ 不带 schema
///    原样重发一次，走纯文本提示；
/// 2. 不管哪条路，回来的文本都用 [`parse_title`] 宽松解析：是 JSON 就取 `title`，
///    不是就取首行去引号。
///
/// 限流、网络、鉴权之类不是「不认 schema」，不换写法，原样交给调用方。
pub async fn generate_title(
    first_prompt: &str,
    session_id: Option<&str>,
    provider: &dyn LlmProvider,
    model: &ModelInfo,
) -> crate::Result<String> {
    let meta = RequestMeta {
        session_id: session_id.map(str::to_string),
        purpose: RequestPurpose::Title,
        ..Default::default()
    };
    // 用户那句话原样发：它不是一段待压缩的历史（见 `COMPACT_HEADING`）。
    let structured = run_prompt(
        first_prompt,
        TITLE_SYSTEM,
        TITLE_MAX_TOKENS,
        title_format(),
        provider,
        model,
        &meta,
    )
    .await;
    let out = match structured {
        Err(crate::error::CarterError::Llm(ref e)) if format_rejected(e) => {
            tracing::debug!(model = %model.api_name, "渠道不认结构化输出，标题改用纯文本：{e}");
            run_prompt(
                first_prompt,
                TITLE_SYSTEM,
                TITLE_MAX_TOKENS,
                ResponseFormat::Text,
                provider,
                model,
                &meta,
            )
            .await?
        }
        other => other?,
    };
    Ok(parse_title(&out))
}

/// 起标题的 system 提示。结构化与纯文本两条路共用：schema 管形状，
/// 提示管内容；纯文本那条路上「只输出标题本身」这句才真正起作用。
const TITLE_SYSTEM: &str = "为下面这条用户消息开启的对话生成一个简洁的 3-6 词中文标题。\
                            只输出标题本身，不要引号、不要标点结尾、不要解释。\
                            如果要求 JSON，就输出 {\"title\": \"标题\"}。";

/// 标题请求的输出上限。JSON 外壳（`{"title":""}`）要占十来个 token，比纯文本多留一点。
const TITLE_MAX_TOKENS: u32 = 48;

/// 标题的 JSON schema：只有一个必填字符串字段。
fn title_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        name: "session_title".into(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
            "additionalProperties": false,
        }),
        strict: true,
    }
}

/// 这个错误是「渠道不认结构化输出」吗 —— 只有这种才值得去掉 schema 重发。
fn format_rejected(err: &llm_adapt_core::LLMError) -> bool {
    use llm_adapt_core::LLMError;
    match err {
        LLMError::Validation { .. } | LLMError::Unsupported { .. } => true,
        LLMError::Provider { ctx, .. } => matches!(ctx.http_status, Some(400 | 422)),
        _ => false,
    }
}

/// 从模型输出里取出标题：JSON 就取 `title`，否则取首行。都去掉首尾引号与空白。
///
/// 宽松是故意的：有的渠道收下 schema 却照样回纯文本，有的把 JSON 包在代码围栏里。
fn parse_title(raw: &str) -> String {
    let trimmed = raw.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    // 是 JSON 但没有 `title`：当作没起出来（空串，调用方会重试或兜底），
    // 而不是把一串 JSON 当标题挂到侧栏上。
    let text = match serde_json::from_str::<serde_json::Value>(unfenced) {
        Ok(v) if v.is_object() => v
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        _ => unfenced.lines().next().unwrap_or("").to_string(),
    };
    text.trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”' | '「' | '」'))
        .trim()
        .to_string()
}

/// 把消息渲染成纯文本（供摘要输入）。
///
/// 按块展开而不是按消息：一条助手消息里可能既有思考又有工具调用，
/// 摘要器需要看到这个次序才能叙述「它先想了什么、再做了什么」。
fn render_messages(messages: &[WireMessage]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = msg.role.as_str();
        for b in &msg.content {
            match b {
                Block::Text { text, .. } => out.push_str(&format!(
                    "[{role}] {text}
"
                )),
                Block::Thinking { text, .. } => out.push_str(&format!(
                    "[thinking] {text}
"
                )),
                Block::ToolUse { name, input, .. } => out.push_str(&format!(
                    "[tool_call] {name}({input})
"
                )),
                Block::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => out.push_str(&format!(
                    "[tool_result {tool_use_id}] {}
",
                    content.as_text()
                )),
                Block::Image { .. } => out.push_str(
                    "[image]
",
                ),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// v1 风格的助手文本消息构造，测试里用起来短一些。
    fn assistant_ir(text: impl Into<String>) -> WireMessage {
        WireMessage::new(Role::Assistant, vec![Block::text(text)])
    }

    use super::*;
    use crate::provider::{
        Block, ChatRequest, EventStream, FinishReason, Role, StreamChunk, ToolResultContent,
        WireMessage,
    };
    use crate::registry::{Pricing, ReasoningEffort};
    use serde_json::json;

    /// 取出消息里第一个工具结果的文本。
    fn tool_text(m: &WireMessage) -> String {
        m.content
            .iter()
            .find_map(|b| match b {
                Block::ToolResult { content, .. } => Some(content.as_text()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn tool_msg(id: &str, content: &str) -> WireMessage {
        WireMessage::new(
            Role::User,
            vec![Block::ToolResult {
                tool_use_id: id.to_string(),
                content: ToolResultContent::Text {
                    text: content.to_string(),
                },
                is_error: false,
                cache: None,
            }],
        )
    }

    /// 返回固定一次性流的假 provider（供摘要逻辑单测，不触网）。
    struct FakeProvider {
        text: String,
        /// 收到的用户消息文本（起标题不该带任何「待压缩」抬头）。
        seen: std::sync::Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FakeProvider {
        async fn stream(&self, req: ChatRequest) -> crate::Result<EventStream> {
            let user = req.wire.messages.iter().find_map(|m| {
                m.content
                    .iter()
                    .find_map(|b| b.as_text().map(str::to_string))
            });
            *self.seen.lock().unwrap() = user;
            let events = vec![
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: self.text.clone(),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn fake_model() -> ModelInfo {
        ModelInfo {
            key: "k".into(),
            provider: "p".into(),
            api_name: "m".into(),
            context_window: 100_000,
            max_output_tokens: 8000,
            tokenizer: "cl100k".into(),
            capabilities: vec![],
            pricing: Pricing {
                input: 0.0,
                output: 0.0,
                cache_write: None,
                cache_read: None,
            },
            default_reasoning: Some(ReasoningEffort::Medium),
        }
    }

    #[test]
    fn estimate_empty_is_zero() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn estimate_grows_with_content() {
        let short = vec![WireMessage::user_text("hi")];
        let long = vec![WireMessage::user_text("x".repeat(400))];
        assert!(estimate_tokens(&long) > estimate_tokens(&short));
    }

    #[test]
    fn l1_elides_old_tool_outputs_keeps_recent() {
        let mut msgs = vec![
            WireMessage::user_text("task"),
            tool_msg("a", &"big output ".repeat(100)),
            assistant_ir("thinking".to_string()),
            tool_msg("b", "recent output kept"),
        ];
        // keep_recent = 2 → 最后 2 条保留，前 2 条折叠。
        elide_old_tool_outputs(&mut msgs, 2);
        // 第 1 条 Tool(a) 应被折叠。
        assert!(tool_text(&msgs[1]).starts_with("[tool result elided"));
        // 最后一条 Tool(b) 原样。
        assert_eq!(tool_text(&msgs[3]), "recent output kept");
    }

    #[test]
    fn l1_noop_when_short() {
        let mut msgs = vec![tool_msg("a", "out")];
        elide_old_tool_outputs(&mut msgs, 6);
        assert_eq!(tool_text(&msgs[0]), "out");
    }

    #[test]
    fn strip_old_image_refs_replaces_only_old_imgs() {
        let mut msgs = vec![
            WireMessage::user_text("oldest [img:abc.png] question"),
            assistant_ir("answer"),
            WireMessage::user_text("recent [img:def.png] still visible"),
        ];
        // keep_recent = 1 → 前两条会被 strip，最后一条保留。
        let n = super::strip_old_image_refs(&mut msgs, 1);
        assert_eq!(n, 1);
        let first = msgs[0].concat_text();
        assert!(first.contains("[image elided]"));
        assert!(!first.contains("[img:abc"));
        assert!(msgs[2].concat_text().contains("[img:def.png]"));
    }

    /// 工具结果里的引用也要剔：provider 边界会把它们展开成真图。
    #[test]
    fn strip_old_image_refs_also_covers_tool_results() {
        let mut msgs = vec![
            tool_msg(
                "a",
                "[img:abc.png]\n[image stored: 1 bytes, image/png mime]",
            ),
            assistant_ir("answer"),
            tool_msg(
                "b",
                "[img:def.png]\n[image stored: 1 bytes, image/png mime]",
            ),
        ];
        let n = super::strip_old_image_refs(&mut msgs, 1);
        assert_eq!(n, 1);
        assert!(tool_text(&msgs[0]).contains("[image elided]"));
        assert!(tool_text(&msgs[2]).contains("[img:def.png]"));
    }

    #[test]
    fn strip_old_image_refs_returns_zero_when_no_imgs() {
        let mut msgs = vec![
            WireMessage::user_text("plain text"),
            assistant_ir("answer"),
            WireMessage::user_text("more text"),
        ];
        let n = super::strip_old_image_refs(&mut msgs, 1);
        assert_eq!(n, 0);
    }

    #[test]
    fn split_keeps_head_and_recent() {
        let msgs = vec![
            WireMessage::user_text("ORIGINAL TASK".to_string()),
            assistant_ir("m1".to_string()),
            WireMessage::new(
                Role::Assistant,
                vec![Block::ToolUse {
                    id: "1".into(),
                    name: "read".into(),
                    input: json!({}),
                    cache: None,
                }],
            ),
            tool_msg("1", "r1"),
            assistant_ir("m2".to_string()),
            tool_msg("2", "r2"),
        ];
        let (head, middle, recent) = split_for_summary(&msgs, 2).unwrap();
        // head 必是原始任务。
        assert_eq!(head.role, Role::User, "head 必须是首条 user 消息");
        assert_eq!(head.concat_text(), "ORIGINAL TASK");
        // recent 是最后 2 条。
        assert_eq!(recent.len(), 2);
        // middle 是中间剩余。
        assert_eq!(middle.len(), 3);
    }

    #[test]
    fn split_none_when_too_short() {
        let msgs = vec![
            WireMessage::user_text("task"),
            assistant_ir("m1".to_string()),
        ];
        assert!(split_for_summary(&msgs, 8).is_none());
    }

    #[test]
    fn l3_system_has_all_sections() {
        let s = structured_summary_system();
        for section in [
            "## 已完成的关键决策",
            "## 涉及的文件/路径",
            "## 未决问题",
            "## 下一步",
            "## 关键发现",
        ] {
            assert!(s.contains(section), "missing section: {section}");
        }
    }

    #[tokio::test]
    async fn l3_summarize_returns_text() {
        let provider = FakeProvider {
            text: "## 已完成的关键决策\n- 做了 X".to_string(),
            seen: Default::default(),
        };
        let model = fake_model();
        let middle = vec![WireMessage::user_text("hi"), assistant_ir("ok")];
        let out = summarize_structured(&middle, &provider, &model, &RequestMeta::default())
            .await
            .unwrap();
        assert!(out.contains("做了 X"));
    }

    #[tokio::test]
    async fn summarize_empty_text_errors() {
        let provider = FakeProvider {
            text: "   ".to_string(),
            seen: Default::default(),
        };
        let model = fake_model();
        let middle = vec![WireMessage::user_text("hi")];
        assert!(
            summarize_structured(&middle, &provider, &model, &RequestMeta::default())
                .await
                .is_err()
        );
    }

    /// 压缩路径仍然带着抬头 —— 顺手把「去掉前缀」这件事的范围钉住。
    #[test]
    fn compaction_input_keeps_its_heading() {
        assert_eq!(compact_input("用户: 你好"), "【待压缩历史】\n用户: 你好");
    }

    /// 不认 `response_format` 的渠道：带 schema 就 400，不带才回 `text`。记下每次请求的格式。
    struct NoSchema {
        text: &'static str,
        error: fn() -> crate::error::CarterError,
        formats: std::sync::Mutex<Vec<ResponseFormat>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for NoSchema {
        async fn stream(&self, req: ChatRequest) -> crate::Result<EventStream> {
            let fmt = req.wire.response_format.clone();
            self.formats.lock().unwrap().push(fmt.clone());
            if fmt != ResponseFormat::Text {
                return Err((self.error)());
            }
            let events = vec![Ok(StreamChunk::TextDelta {
                index: 0,
                text: self.text.to_string(),
            })];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn bad_request() -> crate::error::CarterError {
        crate::error::CarterError::Llm(llm_adapt_core::LLMError::Provider {
            message: "unknown field response_format".into(),
            ctx: llm_adapt_core::ErrorContext {
                http_status: Some(400),
                ..Default::default()
            },
        })
    }

    fn rate_limited() -> crate::error::CarterError {
        crate::error::CarterError::Llm(llm_adapt_core::LLMError::RateLimit {
            message: "slow down".into(),
            retry_after_ms: None,
            ctx: Default::default(),
        })
    }

    /// 结构化输出是第一选择：请求带 schema，回来的 JSON 取 `title`。
    #[tokio::test]
    async fn generate_title_asks_for_json_and_reads_it() {
        let provider = FakeProvider {
            text: r#"{"title": "修复字间隔"}"#.to_string(),
            seen: Default::default(),
        };
        let title = generate_title("帮我修复", None, &provider, &fake_model())
            .await
            .unwrap();
        assert_eq!(title, "修复字间隔");
    }

    /// 渠道不认 schema（400）：去掉 schema 再发一次，纯文本照样出标题。
    #[tokio::test]
    async fn generate_title_falls_back_to_text_when_schema_is_rejected() {
        let provider = NoSchema {
            text: "\"工具功能一览\"\n解释",
            error: bad_request,
            formats: Default::default(),
        };
        let title = generate_title("你好", None, &provider, &fake_model())
            .await
            .unwrap();
        assert_eq!(title, "工具功能一览");
        let formats = provider.formats.lock().unwrap();
        assert_eq!(formats.len(), 2);
        assert!(matches!(formats[0], ResponseFormat::JsonSchema { .. }));
        assert_eq!(formats[1], ResponseFormat::Text);
    }

    /// 限流不是「不认 schema」：不换写法重发，原样交给调用方去退避。
    #[tokio::test]
    async fn generate_title_does_not_downgrade_on_rate_limit() {
        let provider = NoSchema {
            text: "",
            error: rate_limited,
            formats: Default::default(),
        };
        assert!(
            generate_title("你好", None, &provider, &fake_model())
                .await
                .is_err()
        );
        assert_eq!(provider.formats.lock().unwrap().len(), 1);
    }

    #[test]
    fn parse_title_accepts_json_fences_and_plain_text() {
        assert_eq!(parse_title(r#"{"title":"工具功能一览"}"#), "工具功能一览");
        assert_eq!(
            parse_title("```json\n{\"title\": \"代码围栏\"}\n```"),
            "代码围栏"
        );
        assert_eq!(parse_title("「纯文本标题」\n第二行"), "纯文本标题");
        assert_eq!(parse_title(r#"{"other": 1}"#), "");
    }

    #[tokio::test]
    async fn generate_title_strips_quotes_and_takes_first_line() {
        let provider = FakeProvider {
            text: "\"修复字间隔\"\n多余的第二行".to_string(),
            seen: Default::default(),
        };
        let model = fake_model();
        let title = generate_title("帮我修复对话历史字间隔过宽", None, &provider, &model)
            .await
            .unwrap();
        assert_eq!(title, "修复字间隔");
        // 用户那句话原样过去：标题不是在压缩一段历史，不能带压缩器的抬头。
        assert_eq!(
            provider.seen.lock().unwrap().as_deref(),
            Some("帮我修复对话历史字间隔过宽")
        );
    }
}
