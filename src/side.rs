//! 侧边聊天（`/btw`）：带着主对话的上下文问一句，但不接着干它的任务。
//!
//! ## 它是什么
//!
//! 用户在主对话里跑着一个长任务，忽然想插一句「顺便问下，`SessionHandle` 是干嘛的」。
//! 打断主任务要付代价（丢掉正在生成的内容），而把这句话插进主对话会污染后面所有轮次
//! 的上下文。所以给一个**侧边的临时会话**：主对话以转录的形式作为参考、独立跑一轮、
//! 不落盘、关掉即丢。
//!
//! ## 三条硬约束
//!
//! 1. **不落盘。** 侧边的 `Thread` 不挂 recorder，会话文件不存在，于是它也不出现在
//!    `carter sessions` 里、不会被导出、不会被误当成一条真对话。
//! 2. **主对话以文字转录的形式给出。** 见 [`transcript`] —— 每轮现取，不做「种进
//!    Thread」：原样种进去模型认不出那是主对话，而且只能种在干净边界上（悬空的
//!    `tool_use` 会被上游 400），主对话正在跑长任务时恰恰是最想开侧边的时候。
//! 3. **主对话里的计划不接着执行。** 那里面可能有「做完这个再做那个」，侧边只回答问题
//!    —— 由 [`INSTRUCTIONS`] 写给模型。
//!
//! ## 宿主怎么用
//!
//! ```ignore
//! let mut system = assemble_system(/* 与主对话同一套 */);
//! system.push(carter::side::INSTRUCTIONS.to_string());
//! system.push(carter::side::transcript(&parent_messages));
//! // 新建一个不挂 recorder 的 Thread，把用户那句话 append_user，然后 run_turn。
//! ```
//!
//! GUI 宿主还能把「进行中那一轮」也渲染进去（[`LiveTurn`]），命令行没有这个概念，
//! 传空切片即可。

use crate::provider::{Block, Role, WireMessage};

/// 侧边会话每次都会带上的一段说明。
///
/// 写得直白是刻意的：模型看到一大段以「用户」开头的历史，很自然会把它当成当前任务
/// 接着干（尤其历史尾部正是「还没做完的事」）。这段话必须把这件事说破。
pub const INSTRUCTIONS: &str = "\
## 这是一次侧边聊天

用户在主对话旁边开了这个临时会话，用来随手问几句 —— 通常就是问主对话的事：
「它现在在干嘛」「刚才那个改动是什么意思」「这个思路靠谱吗」。主对话的内容就在
下面「主对话」一节里（每次用户发消息时现取的最新状态，可能还在进行中），
**你能看到它，要据此回答**，不要说自己看不到主对话。

但主对话里的任务不是你的任务：

- 不要接着执行里面提到的计划、请求、工具调用或待办 —— 那边自己会接着干；
- 只有用户在这条侧边会话里说的话，才是你的当前指令。

默认只做只读的查看与问答。除非用户明确要求改动，否则不要改文件、不要动 git 状态、
不要改配置。也不要派生子 agent —— 这条会话里没有这个工具。

这条会话是临时的：不会写进会话列表，关掉就没了。需要落地的改动，回主对话去说。";

/// 侧边会话可以捎上的「主对话正在进行中的那一轮」。
///
/// GUI 宿主有这一层（一轮跑着的时候用户开了侧边），命令行没有；传空切片即可。
#[derive(Debug, Clone)]
pub enum LiveTurn {
    /// 模型已经说出来的话。
    Text(String),
    /// 这一轮中途用户插的话。
    User(String),
    /// 一次工具调用。`result` 为 `None` = 还在跑。
    Tool {
        name: String,
        args: String,
        result: Option<String>,
    },
}

/// 转录的总长上限（字符）。超了从**最早**的消息开始丢 —— 问「现在在干嘛」，
/// 要的是尾巴。
const BUDGET: usize = 120_000;
/// 单个工具参数 / 结果的上限。全文留着的话，几次 `read` 就能把预算吃光，
/// 而侧边问答要的是「它干了什么」，不是文件内容本身。
const TOOL_ARGS_MAX: usize = 600;
const TOOL_RESULT_MAX: usize = 1_500;

/// 把主对话渲染成系统提示里的一段。`live` 传空切片 = 没有进行中的一轮。
pub fn transcript(history: &[WireMessage], live: &[LiveTurn]) -> String {
    // 先按条渲染，再从尾部往前装预算。
    let mut entries: Vec<String> = history.iter().filter_map(render_message).collect();
    if !live.is_empty() {
        let mut s = String::from("【进行中：主对话这一轮还没跑完，以下是到目前为止的进展】\n");
        for item in live {
            match item {
                LiveTurn::Text(t) => s.push_str(&format!("助手：{}\n", t.trim())),
                LiveTurn::User(t) => s.push_str(&format!("用户（插话）：{}\n", t.trim())),
                LiveTurn::Tool { name, args, result } => {
                    s.push_str(&format!(
                        "助手调用工具 {name}({})\n",
                        clip(args, TOOL_ARGS_MAX)
                    ));
                    match result {
                        Some(r) => s.push_str(&format!("  → {}\n", clip(r, TOOL_RESULT_MAX))),
                        None => s.push_str("  → （还在执行）\n"),
                    }
                }
            }
        }
        entries.push(s);
    }

    let mut out = String::from("## 主对话\n\n");
    if entries.is_empty() {
        out.push_str("（主对话目前还是空的，用户还没在那边说过话。）");
        return out;
    }

    let mut kept = Vec::new();
    let mut used = 0;
    for e in entries.iter().rev() {
        let n = e.chars().count();
        if used + n > BUDGET && !kept.is_empty() {
            break;
        }
        used += n;
        kept.push(e.as_str());
    }
    if kept.len() < entries.len() {
        out.push_str(&format!(
            "（更早的 {} 条消息过长，已省略）\n\n",
            entries.len() - kept.len()
        ));
    }
    kept.reverse();
    out.push_str(&kept.join("\n"));
    out
}

fn render_message(m: &WireMessage) -> Option<String> {
    let who = match m.role {
        Role::User => "用户",
        Role::Assistant => "助手",
        Role::System => "系统",
    };
    let mut s = String::new();
    for b in &m.content {
        match b {
            Block::Text { text, .. } if !text.trim().is_empty() => {
                s.push_str(&format!("{who}：{}\n", text.trim()));
            }
            Block::ToolUse { name, input, .. } => {
                s.push_str(&format!(
                    "助手调用工具 {name}({})\n",
                    clip(&input.to_string(), TOOL_ARGS_MAX)
                ));
            }
            Block::ToolResult {
                content, is_error, ..
            } => {
                let mark = if *is_error { "（出错）" } else { "" };
                s.push_str(&format!(
                    "  → {mark}{}\n",
                    clip(&content.as_text(), TOOL_RESULT_MAX)
                ));
            }
            Block::Image { .. } => s.push_str(&format!("{who}：[图片]\n")),
            // 思考与服务端块对「主对话在干嘛」帮助不大，还很长。
            _ => {}
        }
    }
    (!s.is_empty()).then_some(s)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…（已截断）")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_text(text: &str) -> WireMessage {
        WireMessage::new(Role::Assistant, vec![Block::text(text)])
    }

    fn user(text: &str) -> WireMessage {
        WireMessage::user_text(text)
    }

    /// 主对话的问答要原样出现在转录里 —— 这正是「侧边看不到主对话」那个 bug。
    #[test]
    fn the_transcript_carries_the_main_conversation() {
        let t = transcript(&[user("你好，你能干啥"), assistant_text("我能写代码")], &[]);
        assert!(t.starts_with("## 主对话"));
        assert!(t.contains("用户：你好，你能干啥"));
        assert!(t.contains("助手：我能写代码"));
    }

    /// 主对话正在跑长任务时（最想开侧边的时候），已经调过的工具、
    /// 半截的工具往返都要看得到。
    #[test]
    fn an_unfinished_turn_is_visible() {
        let live = vec![
            LiveTurn::Text("我先看看测试".into()),
            LiveTurn::Tool {
                name: "bash".into(),
                args: r#"{"command":"cargo test"}"#.into(),
                result: None,
            },
        ];
        let t = transcript(&[user("修一下登录")], &live);
        assert!(t.contains("用户：修一下登录"));
        assert!(t.contains("进行中"));
        assert!(t.contains("cargo test"));
        assert!(t.contains("还在执行"));
    }

    /// 超长时丢最早的，留最近的。
    #[test]
    fn an_overlong_transcript_keeps_the_tail() {
        let big = "x".repeat(BUDGET);
        let t = transcript(&[user(&big), assistant_text("最近的回答")], &[]);
        assert!(t.contains("最近的回答"));
        assert!(t.contains("已省略"));
    }

    #[test]
    fn an_empty_parent_says_so() {
        assert!(transcript(&[], &[]).contains("还是空的"));
    }

    /// 侧边提示要同时说清两件事：主对话你看得到（否则模型会说「我看不到」），
    /// 但那边的任务不归你（否则它会接着干主对话里没做完的事）。
    #[test]
    fn the_instructions_disown_the_inherited_history() {
        for needle in ["主对话", "不要说自己看不到", "不是你的任务", "临时"] {
            assert!(INSTRUCTIONS.contains(needle), "缺少「{needle}」");
        }
    }
}
