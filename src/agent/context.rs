//! 上下文工程：token 估算 + 分级压缩（L1 清旧工具输出 / L2 滚动摘要）。
//! 纪律：本文件不得 import 任何 `genai::*`；压缩的模型调用走 `LlmProvider` trait。

use futures::StreamExt;

use crate::provider::{ChatRequest, Event, LlmProvider, Message};
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
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    crate::tokens::estimate_messages(messages, "cl100k")
}

/// 按模型 tokenizer 估算 token 数（精确路径，供 turn.rs 在已知 model 时调用）。
pub fn estimate_tokens_for(messages: &[Message], tokenizer: &str) -> u64 {
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
    let Some((head, middle, recent)) = split_for_summary(&thread.messages, L2_KEEP_RECENT) else {
        // 历史太短无法切分（仅 head + recent），L1 已尽力。
        return Ok(());
    };
    if middle.is_empty() {
        return Ok(());
    }

    // L3 优先（结构化高保真）；仅当 L3 返回空/解析失败才回落 L2（传输错误大概率 L2 也失败，
    // 不浪费一次调用）；两者都拿不到 → 保留 L1 结果，不中断主循环。
    let tiered = match summarize_structured(&middle, provider, model).await {
        Ok(s) => Some(("L3", s)),
        Err(e3) => {
            ui.emit(UiEvent::Notice(format!(
                "[compact] L3 structured summary failed ({e3}); trying L2"
            )));
            match summarize(&middle, provider, model).await {
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
        rebuilt.push(Message::Assistant(format!("【历史摘要·{tier}】\n{summary}")));
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

/// L1：把保护窗口之外的 `Message::Tool` 大输出替换为占位摘要。纯函数，便于测试。
fn elide_old_tool_outputs(messages: &mut [Message], keep_recent: usize) {
    let len = messages.len();
    if len <= keep_recent {
        return;
    }
    let cutoff = len - keep_recent;
    for msg in messages.iter_mut().take(cutoff) {
        if let Message::Tool { call_id, content } = msg {
            // 已折叠过的不再处理。
            if content.starts_with("[tool result elided") {
                continue;
            }
            let bytes = content.len();
            *content = format!("[tool result elided: {bytes} bytes, call_id={call_id}]");
        }
    }
}

/// L0：把保护窗口之外的消息文本里的 `[img:<hash>.<ext>]` 引用替换为占位文本
/// `[image elided]`，让 genai 边界不再发对应的多模态块。返回剥离的引用条数。
///
/// 仅扫 User / Assistant / Tool 文本字段；不动 ToolCalls / System。
/// 最近 `keep_recent` 条不动（让模型还能看到当前任务的图）。
fn strip_old_image_refs(messages: &mut [Message], keep_recent: usize) -> usize {
    let len = messages.len();
    if len <= keep_recent {
        return 0;
    }
    let cutoff = len - keep_recent;
    let mut stripped = 0usize;
    for msg in messages.iter_mut().take(cutoff) {
        match msg {
            Message::User(s) | Message::Assistant(s) => {
                stripped += strip_in_str(s);
            }
            Message::Tool { content, .. } => {
                stripped += strip_in_str(content);
            }
            _ => {}
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
    messages: &[Message],
    keep_recent: usize,
) -> Option<(Message, Vec<Message>, Vec<Message>)> {
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
    middle: &[Message],
    provider: &dyn LlmProvider,
    model: &ModelInfo,
) -> crate::Result<String> {
    let rendered = render_messages(middle);
    let system = "你是上下文压缩器。把以下 agent 执行历史压成简洁要点，\
                  务必保留：已做的关键决策、尝试过的方案、重要发现/文件路径、未决问题与下一步。\
                  丢弃冗长的原始工具输出。用紧凑的中文列点。";
    run_summary(&rendered, system, SUMMARY_MAX_TOKENS, provider, model).await
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
    middle: &[Message],
    provider: &dyn LlmProvider,
    model: &ModelInfo,
) -> crate::Result<String> {
    let rendered = render_messages(middle);
    run_summary(
        &rendered,
        structured_summary_system(),
        L3_SUMMARY_MAX_TOKENS,
        provider,
        model,
    )
    .await
}

/// 摘要请求公共流程：发一次 system+rendered，收集 TextDelta，空输出视为失败。
async fn run_summary(
    rendered: &str,
    system: &str,
    max_tokens: u32,
    provider: &dyn LlmProvider,
    model: &ModelInfo,
) -> crate::Result<String> {
    let req = ChatRequest {
        model_api_name: model.api_name.clone(),
        system: vec![system.to_string()],
        messages: vec![Message::User(format!("【待压缩历史】\n{rendered}"))],
        tools: Vec::new(),
        reasoning: None,
        max_output_tokens: Some(max_tokens),
    };

    let mut stream = provider.stream(req).await?;
    let mut summary = String::new();
    while let Some(ev) = stream.next().await {
        if let Event::TextDelta(t) = ev? {
            summary.push_str(&t);
        }
    }
    if summary.trim().is_empty() {
        return Err(crate::error::CarterError::Provider(
            "summary returned empty text".to_string(),
        ));
    }
    Ok(summary)
}

/// 为会话生成简短标题：用首条 user prompt 调一次（fast）模型，取首行去引号。
/// 整个会话只调一次（由调用方保证）。失败由调用方静默忽略。
pub async fn generate_title(
    first_prompt: &str,
    provider: &dyn LlmProvider,
    model: &ModelInfo,
) -> crate::Result<String> {
    let system = "为下面这条用户消息开启的对话生成一个简洁的 3-6 词中文标题。\
                  只输出标题本身，不要引号、不要标点结尾、不要解释。";
    let out = run_summary(first_prompt, system, 32, provider, model).await?;
    let title = out
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"')
        .to_string();
    Ok(title)
}

/// 把消息渲染成纯文本（供摘要输入）。
fn render_messages(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::System(s) => out.push_str(&format!("[system] {s}\n")),
            Message::User(s) => out.push_str(&format!("[user] {s}\n")),
            Message::Assistant(s) => out.push_str(&format!("[assistant] {s}\n")),
            Message::ToolCalls(calls) => {
                for c in calls {
                    out.push_str(&format!("[tool_call] {}({})\n", c.name, c.args));
                }
            }
            Message::Tool { call_id, content } => {
                out.push_str(&format!("[tool_result {call_id}] {content}\n"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatRequest, EventStream, ToolCall};
    use crate::registry::{Pricing, ReasoningEffort};
    use serde_json::json;

    fn tool_msg(id: &str, content: &str) -> Message {
        Message::Tool {
            call_id: id.to_string(),
            content: content.to_string(),
        }
    }

    /// 返回固定一次性流的假 provider（供摘要逻辑单测，不触网）。
    struct FakeProvider {
        text: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FakeProvider {
        async fn stream(&self, _req: ChatRequest) -> crate::Result<EventStream> {
            let events = vec![
                Ok(Event::TextDelta(self.text.clone())),
                Ok(Event::Done(crate::provider::StopReason::EndTurn)),
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
        let short = vec![Message::User("hi".to_string())];
        let long = vec![Message::User("x".repeat(400))];
        assert!(estimate_tokens(&long) > estimate_tokens(&short));
    }

    #[test]
    fn l1_elides_old_tool_outputs_keeps_recent() {
        let mut msgs = vec![
            Message::User("task".to_string()),
            tool_msg("a", &"big output ".repeat(100)),
            Message::Assistant("thinking".to_string()),
            tool_msg("b", "recent output kept"),
        ];
        // keep_recent = 2 → 最后 2 条保留，前 2 条折叠。
        elide_old_tool_outputs(&mut msgs, 2);
        // 第 1 条 Tool(a) 应被折叠。
        if let Message::Tool { content, .. } = &msgs[1] {
            assert!(content.starts_with("[tool result elided"));
        } else {
            panic!("expected tool msg");
        }
        // 最后一条 Tool(b) 原样。
        if let Message::Tool { content, .. } = &msgs[3] {
            assert_eq!(content, "recent output kept");
        } else {
            panic!("expected tool msg");
        }
    }

    #[test]
    fn l1_noop_when_short() {
        let mut msgs = vec![tool_msg("a", "out")];
        elide_old_tool_outputs(&mut msgs, 6);
        if let Message::Tool { content, .. } = &msgs[0] {
            assert_eq!(content, "out");
        }
    }

    #[test]
    fn strip_old_image_refs_replaces_only_old_imgs() {
        let mut msgs = vec![
            Message::User("oldest [img:abc.png] question".into()),
            Message::Assistant("answer".into()),
            Message::User("recent [img:def.png] still visible".into()),
        ];
        // keep_recent = 1 → 前两条会被 strip，最后一条保留。
        let n = super::strip_old_image_refs(&mut msgs, 1);
        assert_eq!(n, 1);
        match &msgs[0] {
            Message::User(s) => {
                assert!(s.contains("[image elided]"));
                assert!(!s.contains("[img:abc"));
            }
            _ => panic!(),
        }
        match &msgs[2] {
            Message::User(s) => {
                assert!(s.contains("[img:def.png]"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn strip_old_image_refs_returns_zero_when_no_imgs() {
        let mut msgs = vec![
            Message::User("plain text".into()),
            Message::Assistant("answer".into()),
            Message::User("more text".into()),
        ];
        let n = super::strip_old_image_refs(&mut msgs, 1);
        assert_eq!(n, 0);
    }

    #[test]
    fn split_keeps_head_and_recent() {
        let msgs = vec![
            Message::User("ORIGINAL TASK".to_string()),
            Message::Assistant("m1".to_string()),
            Message::ToolCalls(vec![ToolCall {
                id: "1".into(),
                name: "read".into(),
                args: json!({}),
            }]),
            tool_msg("1", "r1"),
            Message::Assistant("m2".to_string()),
            tool_msg("2", "r2"),
        ];
        let (head, middle, recent) = split_for_summary(&msgs, 2).unwrap();
        // head 必是原始任务。
        if let Message::User(s) = head {
            assert_eq!(s, "ORIGINAL TASK");
        } else {
            panic!("head must be first user msg");
        }
        // recent 是最后 2 条。
        assert_eq!(recent.len(), 2);
        // middle 是中间剩余。
        assert_eq!(middle.len(), 3);
    }

    #[test]
    fn split_none_when_too_short() {
        let msgs = vec![
            Message::User("task".to_string()),
            Message::Assistant("m1".to_string()),
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
        };
        let model = fake_model();
        let middle = vec![Message::User("hi".into()), Message::Assistant("ok".into())];
        let out = summarize_structured(&middle, &provider, &model).await.unwrap();
        assert!(out.contains("做了 X"));
    }

    #[tokio::test]
    async fn summarize_empty_text_errors() {
        let provider = FakeProvider { text: "   ".to_string() };
        let model = fake_model();
        let middle = vec![Message::User("hi".into())];
        assert!(summarize_structured(&middle, &provider, &model).await.is_err());
    }

    #[tokio::test]
    async fn generate_title_strips_quotes_and_takes_first_line() {
        let provider = FakeProvider {
            text: "\"修复字间隔\"\n多余的第二行".to_string(),
        };
        let model = fake_model();
        let title = generate_title("帮我修复对话历史字间隔过宽", &provider, &model)
            .await
            .unwrap();
        assert_eq!(title, "修复字间隔");
    }
}
