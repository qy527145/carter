//! 真实 token 计数。按 model.tokenizer 字段选 tiktoken 编码器；其它走 chars/4 兜底。
//!
//! - cl100k_base / cl100k → OpenAI GPT-3.5 / GPT-4 全系，Anthropic 用作合理估算（差 5-15%）
//! - o200k_base / o200k / o1 → GPT-4o / o1 / o3
//! - p50k_base → text-davinci-003
//! - 其它（gemini / 自定义） → chars/4 启发式兜底
//!
//! 编码器**全局缓存**（OnceLock），首次访问惰性初始化。
//!
//! 这个估算只用于"是否触发上下文压缩"的判定；真实计费仍以 provider 返回的 Usage 为准。
//! 因此即使 Anthropic 用 cl100k 估算偏差 ±15%，对压缩时机决策影响有限——阈值
//! `compact_threshold_ratio` 默认 0.75 留了余量。

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

use crate::provider::Message;

static CL100K: OnceLock<Option<CoreBPE>> = OnceLock::new();
static O200K: OnceLock<Option<CoreBPE>> = OnceLock::new();
static P50K: OnceLock<Option<CoreBPE>> = OnceLock::new();

/// 用模型 tokenizer 字段选 BPE 编码器；返回 None → 调用方走 chars/4 兜底。
fn encoder_for(tokenizer: &str) -> Option<&'static CoreBPE> {
    let t = tokenizer.to_ascii_lowercase();
    match t.as_str() {
        "cl100k" | "cl100k_base" | "anthropic" | "claude" => CL100K
            .get_or_init(|| tiktoken_rs::cl100k_base().ok())
            .as_ref(),
        "o200k" | "o200k_base" | "o1" | "o3" => O200K
            .get_or_init(|| tiktoken_rs::o200k_base().ok())
            .as_ref(),
        "p50k" | "p50k_base" => P50K
            .get_or_init(|| tiktoken_rs::p50k_base().ok())
            .as_ref(),
        _ => None,
    }
}

/// 估算单条消息的 token 数。失败/未知 tokenizer 走 chars/4。
pub fn estimate_message(msg: &Message, tokenizer: &str) -> u64 {
    let text = render_for_counting(msg);
    count_text(&text, tokenizer)
}

/// 估算一组消息的 token 数（含每条 +4 结构化开销，模拟 OpenAI 消息封包成本）。
pub fn estimate_messages(messages: &[Message], tokenizer: &str) -> u64 {
    messages
        .iter()
        .map(|m| estimate_message(m, tokenizer) + 4)
        .sum()
}

/// 估算任意文本的 token 数。
pub fn count_text(text: &str, tokenizer: &str) -> u64 {
    match encoder_for(tokenizer) {
        Some(enc) => enc.encode_with_special_tokens(text).len() as u64,
        None => fallback_estimate(text),
    }
}

/// chars/4 启发式兜底（不能拿到合适的 BPE 时）。
fn fallback_estimate(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// 把单条消息渲染成适合 BPE 计数的纯文本。图片引用 `[img:...]` 当字面量计入（约 8 token），
/// 因为真实图片的 token 成本由 provider 计算（一张图大约 1568px 边 → 1.5k tokens），
/// 我们按字面量估算只是为了**触发压缩决策**而非精确计费。
fn render_for_counting(msg: &Message) -> String {
    match msg {
        Message::System(s) | Message::User(s) | Message::Assistant(s) => s.clone(),
        Message::Tool { content, call_id } => format!("[tool:{call_id}] {content}"),
        Message::ToolCalls(calls) => calls
            .iter()
            .map(|c| format!("[call:{}] {}({})", c.id, c.name, c.args))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cl100k_returns_reasonable_count_for_english() {
        // "Hello, world" ≈ 3 tokens with cl100k_base。
        let n = count_text("Hello, world", "cl100k");
        assert!(n > 0 && n < 10, "got {n}");
    }

    #[test]
    fn cl100k_handles_chinese() {
        // 4 汉字"中文测试"在 cl100k_base 下大约 3-8 tokens（cl100k 有 CJK BPE 合并）。
        let n = count_text("中文测试", "cl100k");
        assert!(n >= 2 && n <= 12, "got {n}");
    }

    #[test]
    fn unknown_tokenizer_falls_back_to_chars_over_4() {
        let n = count_text("abcdefgh", "unknown-tokenizer");
        assert_eq!(n, 2); // 8 chars / 4 = 2
    }

    #[test]
    fn anthropic_alias_uses_cl100k() {
        // anthropic 别名要能拿到编码器（不应回落 chars/4）。
        let a = count_text("Hello, world", "anthropic");
        let c = count_text("Hello, world", "cl100k");
        assert_eq!(a, c);
    }

    #[test]
    fn estimate_messages_sums_with_envelope_overhead() {
        let msgs = vec![Message::User("hi".to_string()), Message::Assistant("ok".to_string())];
        let n = estimate_messages(&msgs, "cl100k");
        // 每条 +4 envelope，两条至少 8 + 内容 token。
        assert!(n >= 8);
    }
}
