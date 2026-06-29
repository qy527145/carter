//! 渲染反转：agent 层产出 `UiEvent`，由 sink 消费（stdout 或 TUI 通道）。
//! 纪律：本文件不得 import 任何 `ratatui`/`crossterm`/`genai` 类型——agent 层与终端解耦。

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::provider::{Message, ToolCall, Usage};
use crate::tools::{TodoItem, TodoStatus};

/// resume/fork 历史回放的一条消息（仅用于可视化，不发给模型）。
#[derive(Debug, Clone)]
pub struct ReplayMsg {
    pub role: ReplayRole,
    pub text: String,
}

/// 回放消息的角色（决定 TUI 着色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayRole {
    User,
    Assistant,
    ToolCall,
    ToolResult,
}

/// 把会话历史消息转成可回放的展示项（工具输出截断为单行预览）。
pub fn replay_from_messages(msgs: &[Message]) -> Vec<ReplayMsg> {
    let mut out = Vec::new();
    for m in msgs {
        match m {
            Message::User(s) => out.push(ReplayMsg {
                role: ReplayRole::User,
                text: s.clone(),
            }),
            Message::Assistant(s) => out.push(ReplayMsg {
                role: ReplayRole::Assistant,
                text: s.clone(),
            }),
            Message::ToolCalls(calls) => {
                for c in calls {
                    out.push(ReplayMsg {
                        role: ReplayRole::ToolCall,
                        text: format!("⚙ {}({})", c.name, args_preview(&c.args)),
                    });
                }
            }
            Message::Tool { content, .. } => {
                let first = content.lines().next().unwrap_or("");
                out.push(ReplayMsg {
                    role: ReplayRole::ToolResult,
                    text: truncate_inline(first, 120),
                });
            }
            Message::System(_) => {}
        }
    }
    out
}

/// agent loop 渲染事件。turn.rs / context.rs 只 emit 这些，不直接碰终端。
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// 模型正文增量。
    AssistantTextDelta(String),
    /// 思考增量。
    ThinkingDelta(String),
    /// 一个 assistant 文本块结束（换行/收尾），便于 sink 分块。
    AssistantTextEnd,
    /// 工具调用开始（执行前）。
    ToolCallStarted { name: String, args_preview: String },
    /// 工具结果（执行后）。
    ToolResult { ok: bool, summary: String },
    /// 子 agent / 主 agent 通过 `ask_user_question` 工具向 TUI 弹出选择题。
    /// `response_tx` 为 oneshot channel：TUI 选完把答案文本（option label 或自由输入）发回。
    AskUser {
        id: u64,
        question: String,
        options: Vec<String>,
        /// 用 Mutex<Option<...>> 包裹 oneshot Sender，因为 UiEvent 必须 Clone（多个 sink），
        /// 但 oneshot Sender 不是 Clone —— 实际只有一个 sink 应该消费 Sender。
        response_tx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    },
    /// todo 列表更新。
    TodoUpdated(Vec<TodoItem>),
    /// 系统提示（压缩日志等，替代裸 eprintln）。
    Notice(String),
    /// 会话标题（fast 模型生成，整个会话一次）。
    Title(String),
    /// 当前模型变更（启动 / `/model` 切换）——状态栏立即更新，不等首轮 usage。
    ModelChanged(String),
    /// 一条带标签的分隔线（如「上下文已清空」「已恢复会话」），可视化标记上下文边界。
    Divider(String),
    /// resume/fork 后把历史消息回放到视图（不发给模型，仅可视化）。
    ReplayHistory(Vec<ReplayMsg>),
    /// 一轮（或一次斜杠命令）处理结束 —— 让 UI 退出 streaming 态、恢复可交互。
    Idle,
    /// 一轮结束的用量 + 成本。
    TurnUsage {
        usage: Usage,
        cost: f64,
        model: String,
    },
}

/// 渲染出口。两实现：`StdoutSink`（裸 ANSI，向后兼容 / 管道）、`ChannelSink`（TUI）。
pub trait UiSink: Send {
    fn emit(&mut self, ev: UiEvent);
}

/// 协作式取消令牌（轻量）。TUI 在 Esc/Ctrl+C 时 set；turn.rs 流式循环里轮询 **或** await。
/// `Notify` 让取消能即时唤醒卡住的 `stream.next()`（不再依赖事件间隙轮询）。
#[derive(Debug, Clone)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
        // 唤醒所有等待者（卡住的 stream select 臂）。
        self.notify.notify_waiters();
    }

    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// await 到被取消为止。若已取消则立即返回。
    /// 注意：必须在进入可能阻塞的 await 前调用 `cancelled()` 拿到 future，
    /// 以避免 set→await 之间的丢失唤醒（Notify 仅唤醒已注册的等待者）。
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

/// 裸 ANSI sink —— 把 M3 之前 turn.rs 里的打印逻辑搬到这里。
/// 供 `--no-tui` / 一次性 / 管道场景。
#[derive(Default)]
pub struct StdoutSink {
    in_thinking: bool,
}

impl StdoutSink {
    pub fn new() -> Self {
        Self::default()
    }
}

impl UiSink for StdoutSink {
    fn emit(&mut self, ev: UiEvent) {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        match ev {
            UiEvent::ThinkingDelta(t) => {
                if !self.in_thinking {
                    let _ = write!(out, "\x1b[90m[thinking] ");
                    self.in_thinking = true;
                }
                let _ = write!(out, "{t}");
                let _ = out.flush();
            }
            UiEvent::AssistantTextDelta(t) => {
                if self.in_thinking {
                    let _ = write!(out, "\x1b[0m\n");
                    self.in_thinking = false;
                }
                let _ = write!(out, "{t}");
                let _ = out.flush();
            }
            UiEvent::AssistantTextEnd => {
                if self.in_thinking {
                    let _ = write!(out, "\x1b[0m");
                    self.in_thinking = false;
                }
                let _ = writeln!(out);
                let _ = out.flush();
            }
            UiEvent::ToolCallStarted { name, args_preview } => {
                let _ = writeln!(out, "\x1b[36m⚙ {name}({args_preview})\x1b[0m");
            }
            UiEvent::ToolResult { ok, summary } => {
                let (mark, color) = if ok { ("✓", "\x1b[32m") } else { ("✗", "\x1b[31m") };
                let _ = writeln!(out, "{color}  {mark} {summary}\x1b[0m");
            }
            UiEvent::TodoUpdated(todos) => {
                for t in &todos {
                    let (mark, color, text) = match t.status {
                        TodoStatus::Completed => ("[x]", "\x1b[32m", &t.content),
                        TodoStatus::InProgress => ("[~]", "\x1b[33m", &t.active_form),
                        TodoStatus::Pending => ("[ ]", "\x1b[90m", &t.content),
                    };
                    let _ = writeln!(out, "{color}  {mark} {text}\x1b[0m");
                }
            }
            UiEvent::Notice(msg) => {
                let _ = writeln!(out, "\x1b[90m{msg}\x1b[0m");
            }
            // oneshot 不生成标题；穷尽 match 需此臂。
            UiEvent::Title(_) => {}
            UiEvent::ModelChanged(_) => {}
            UiEvent::Divider(label) => {
                let _ = writeln!(out, "\x1b[90m──── {label} ────\x1b[0m");
            }
            // oneshot 不 resume，无历史可回放。
            UiEvent::ReplayHistory(_) => {}
            // oneshot 无 streaming 态可恢复，忽略。
            UiEvent::Idle => {}
            UiEvent::AskUser {
                question,
                options,
                response_tx,
                ..
            } => {
                // oneshot 模式（无 TUI）：打印问题，并把第一个 option（缺省）发回作为答案。
                // 让 LLM 在 oneshot 流程里不会卡死。
                let _ = writeln!(out, "\x1b[33m[ask] {question}\x1b[0m");
                if !options.is_empty() {
                    let _ = writeln!(out, "\x1b[33m  options: {}\x1b[0m", options.join(" | "));
                }
                let auto = options.first().cloned().unwrap_or_default();
                if let Ok(mut guard) = response_tx.lock() {
                    if let Some(tx) = guard.take() {
                        let _ = tx.send(auto);
                    }
                }
            }
            UiEvent::TurnUsage { usage, cost, model } => {
                let _ = writeln!(
                    out,
                    "\n[tokens] in={} out={} cache_read={} cache_write={} reasoning={} | cost=${:.4} ({})",
                    usage.input,
                    usage.output,
                    usage.cache_read,
                    usage.cache_write,
                    usage.reasoning,
                    cost,
                    model,
                );
            }
        }
    }
}

/// 把工具调用参数压成单行预览（供 `ToolCallStarted.args_preview`）。
pub fn args_preview(args: &serde_json::Value) -> String {
    match args.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, v)| {
                let vs = match v {
                    serde_json::Value::String(s) => truncate_inline(s, 40),
                    other => other.to_string(),
                };
                format!("{k}={vs}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => truncate_inline(&args.to_string(), 80),
    }
}

/// 单行截断（按 char 边界）。
pub fn truncate_inline(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}

/// 工具调用 → 单行预览，组装 `ToolCallStarted`。
pub fn tool_call_started(tc: &ToolCall) -> UiEvent {
    UiEvent::ToolCallStarted {
        name: tc.name.clone(),
        args_preview: args_preview(&tc.args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token_set_reset() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        t.set();
        assert!(t.is_cancelled());
        t.reset();
        assert!(!t.is_cancelled());
    }

    #[test]
    fn cancel_token_clone_shares_state() {
        let a = CancelToken::new();
        let b = a.clone();
        a.set();
        assert!(b.is_cancelled());
    }

    #[test]
    fn stdout_sink_emit_does_not_panic() {
        let mut sink = StdoutSink::new();
        sink.emit(UiEvent::AssistantTextDelta("hi".into()));
        sink.emit(UiEvent::AssistantTextEnd);
        sink.emit(UiEvent::ToolResult {
            ok: true,
            summary: "done".into(),
        });
        sink.emit(UiEvent::Notice("note".into()));
    }

    #[test]
    fn args_preview_compacts_object() {
        let v = serde_json::json!({"path": "a.txt", "n": 3});
        let p = args_preview(&v);
        assert!(p.contains("path="));
        assert!(p.contains("n=3"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate_inline("abc", 5), "abc");
        assert_eq!(truncate_inline("abcdef", 3), "abc…");
    }

    #[test]
    fn replay_maps_roles_and_truncates_tool_output() {
        use crate::provider::ToolCall;
        use serde_json::json;
        let msgs = vec![
            Message::System("sys".into()), // 跳过
            Message::User("问题".into()),
            Message::Assistant("回答".into()),
            Message::ToolCalls(vec![ToolCall {
                id: "1".into(),
                name: "read".into(),
                args: json!({"path": "a.rs"}),
            }]),
            Message::Tool {
                call_id: "1".into(),
                content: format!("第一行\n{}", "x".repeat(500)),
            },
        ];
        let out = replay_from_messages(&msgs);
        // System 被跳过 → 4 条。
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].role, ReplayRole::User);
        assert_eq!(out[1].role, ReplayRole::Assistant);
        assert_eq!(out[2].role, ReplayRole::ToolCall);
        assert!(out[2].text.contains("read"));
        assert_eq!(out[3].role, ReplayRole::ToolResult);
        // 工具结果只取首行且截断（不含第二行的海量 x）。
        assert!(out[3].text.starts_with("第一行"));
        assert!(out[3].text.chars().count() <= 121);
    }
}
