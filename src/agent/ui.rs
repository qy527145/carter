//! 渲染反转：agent 层产出 `UiEvent`，由 sink 消费（stdout 或 TUI 通道）。
//! 纪律：本文件不得 import 任何 `ratatui`/`crossterm`/`genai` 类型——agent 层与终端解耦。

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::provider::{Block, Role, ToolCall, Usage, WireMessage};
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
///
/// 一条消息里可能有多种块（用户消息里混着工具结果、助手消息里文本与工具调用交错），
/// 所以按块展开而不是按消息展开 —— 否则回放出来的顺序会和当初看到的对不上。
pub fn replay_from_messages(msgs: &[WireMessage]) -> Vec<ReplayMsg> {
    let mut out = Vec::new();
    for m in msgs {
        for b in &m.content {
            match (m.role, b) {
                (Role::User, Block::Text { text, .. }) => out.push(ReplayMsg {
                    role: ReplayRole::User,
                    text: text.clone(),
                }),
                (Role::Assistant, Block::Text { text, .. }) => out.push(ReplayMsg {
                    role: ReplayRole::Assistant,
                    text: text.clone(),
                }),
                (_, Block::ToolUse { name, input, .. }) => out.push(ReplayMsg {
                    role: ReplayRole::ToolCall,
                    text: format!("⚙ {}({})", name, args_preview(input)),
                }),
                (_, Block::ToolResult { content, .. }) => {
                    let text = content.as_text();
                    let first = text.lines().next().unwrap_or("");
                    out.push(ReplayMsg {
                        role: ReplayRole::ToolResult,
                        text: truncate_inline(first, 120),
                    });
                }
                _ => {}
            }
        }
    }
    out
}

/// `AskUser` 的应答通道：`Mutex<Option<oneshot::Sender>>` 让事件可 Clone，
/// 而真正的 Sender 只能被一个 sink 取走一次。
pub type AskResponder =
    std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Vec<Vec<String>>>>>>;

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
    /// 子 agent / 主 agent 通过 `ask_user_question` 工具向 TUI 弹出问题表单。
    ///
    /// 一次可以问**一组**问题（见 [`crate::agent::ask_user`]）：一次问完比分几轮
    /// 问好 —— 每多一轮就多一次完整的模型请求。`response_tx` 回传的是
    /// 「第 i 个问题 → 用户选中的若干项」，与 `questions` 一一对应。
    AskUser {
        id: u64,
        questions: Vec<crate::agent::AskQuestion>,
        /// 用 Mutex<Option<...>> 包裹 oneshot Sender，因为 UiEvent 必须 Clone（多个 sink），
        /// 但 oneshot Sender 不是 Clone —— 实际只有一个 sink 应该消费 Sender。
        response_tx: AskResponder,
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
    /// 模型请求遇到暂时性错误，等 `delay_ms` 之后自动重发（第 `attempt` 次，共 `max` 次）。
    ///
    /// 收到这条时，**这一次请求**已经吐出来的正文 / 思考增量作废：它们不进历史，
    /// 重发之后模型会从头再说一遍。能撤回的宿主（GUI）应该把那半截撤掉。
    Retrying {
        attempt: u32,
        max: u32,
        delay_ms: u64,
        /// 这次是为什么没成（单行，已截断）。
        error: String,
    },
    /// `task` 工具里几个子 agent 的进度（整表快照，按 `call_id` 配对那张卡片）。
    ///
    /// 发整表而不是增量：并发写时增量很容易次序错乱，而这张表本来就只有几行。
    AgentProgress {
        call_id: String,
        agents: Vec<AgentRun>,
    },
}

/// 一个子 agent 的进度（`task` 工具那张卡片的一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRun {
    /// 这个子 agent 的 id（`Thread::agent_id`，每次派生新生成）。
    ///
    /// 宿主靠它把子 agent 自己的工具调用（[`crate::agent::current_agent_id`] 标出来的）
    /// 归到这一行底下 —— 描述会重名，位置会随并发乱，只有 id 是稳的。
    pub id: String,
    /// 主对话给这一行的短标题。
    pub description: String,
    /// `running` | `done` | `failed` | `cancelled`。
    pub status: String,
    /// 已经发起了几次工具调用 —— 「一条命令跑了很久」和「根本没动」能看出区别。
    pub steps: u32,
    /// 此刻正在跑的工具名。`None` = 在思考或已经结束。
    pub tool: Option<String>,
    /// 最近一段输出（尾部截断）。
    pub text: String,
}

/// [`UiEvent::Retrying`] 的一句话版本，给只会打字的出口（终端、TUI）用。
pub fn retry_notice(attempt: u32, max: u32, delay_ms: u64, error: &str) -> String {
    let secs = delay_ms.div_ceil(1000);
    format!("[retry] 请求失败，{secs} 秒后重试（{attempt}/{max}）：{error}")
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
    pub async fn cancelled(&self) {
        // 先登记再查标志：反过来的话，查完到登记之间的那次 `set` 会被漏掉
        // （`notify_waiters` 只叫得醒已登记的），这里就再也醒不过来。
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// 裸 ANSI sink —— 把 M3 之前 turn.rs 里的打印逻辑搬到这里。
/// 供 `--no-tui` / 一次性 / 管道场景。
///
/// `color = false` 时一个转义序列都不写：输出要进文件、进评测脚本的时候，
/// 夹着 ANSI 码的文本得先洗一遍才能用。
pub struct StdoutSink {
    in_thinking: bool,
    color: bool,
}

impl Default for StdoutSink {
    fn default() -> Self {
        Self {
            in_thinking: false,
            color: true,
        }
    }
}

impl StdoutSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否输出 ANSI 颜色。
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// 颜色开着就返回转义序列，否则空串。
    fn c(&self, code: &'static str) -> &'static str {
        if self.color { code } else { "" }
    }
}

impl UiSink for Box<dyn UiSink> {
    fn emit(&mut self, ev: UiEvent) {
        (**self).emit(ev)
    }
}

/// 什么都不输出的 sink。JSON 结果模式用：过程不打印，只要最后那份汇总。
#[derive(Default)]
pub struct NullSink;

impl UiSink for NullSink {
    fn emit(&mut self, _ev: UiEvent) {}
}

/// 包一层：事件原样转给内层 sink，顺手记下**最后一段** assistant 正文。
///
/// 一次性 / 评测场景要的是「模型最终说了什么」，而 `run_turn` 只返回 outcome 和用量；
/// 正文只经过 sink 流过去。与其让 turn 层多返回一个字段，不如在出口处截一份。
pub struct Capture<S: UiSink> {
    inner: S,
    current: String,
    last: String,
}

impl<S: UiSink> Capture<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            current: String::new(),
            last: String::new(),
        }
    }

    /// 最后一个已结束的 assistant 文本块；若最后一块没收到 `AssistantTextEnd`
    /// （中途取消）则返回那半截。
    pub fn final_text(&self) -> &str {
        if self.current.is_empty() {
            &self.last
        } else {
            &self.current
        }
    }
}

impl<S: UiSink> UiSink for Capture<S> {
    fn emit(&mut self, ev: UiEvent) {
        match &ev {
            UiEvent::AssistantTextDelta(t) => self.current.push_str(t),
            UiEvent::AssistantTextEnd if !self.current.is_empty() => {
                self.last = std::mem::take(&mut self.current);
            }
            _ => {}
        }
        self.inner.emit(ev);
    }
}

impl UiSink for StdoutSink {
    fn emit(&mut self, ev: UiEvent) {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let (reset, dim, cyan, green, red, yellow, magenta) = (
            self.c("\x1b[0m"),
            self.c("\x1b[90m"),
            self.c("\x1b[36m"),
            self.c("\x1b[32m"),
            self.c("\x1b[31m"),
            self.c("\x1b[33m"),
            self.c("\x1b[35m"),
        );
        match ev {
            UiEvent::ThinkingDelta(t) => {
                if !self.in_thinking {
                    let _ = write!(out, "{dim}[thinking] ");
                    self.in_thinking = true;
                }
                let _ = write!(out, "{t}");
                let _ = out.flush();
            }
            UiEvent::AssistantTextDelta(t) => {
                if self.in_thinking {
                    let _ = writeln!(out, "{reset}");
                    self.in_thinking = false;
                }
                let _ = write!(out, "{t}");
                let _ = out.flush();
            }
            UiEvent::AssistantTextEnd => {
                if self.in_thinking {
                    let _ = write!(out, "{reset}");
                    self.in_thinking = false;
                }
                let _ = writeln!(out);
                let _ = out.flush();
            }
            UiEvent::ToolCallStarted { name, args_preview } => {
                let _ = writeln!(out, "{cyan}⚙ {name}({args_preview}){reset}");
            }
            UiEvent::ToolResult { ok, summary } => {
                let (mark, color) = if ok { ("✓", green) } else { ("✗", red) };
                let _ = writeln!(out, "{color}  {mark} {summary}{reset}");
            }
            UiEvent::TodoUpdated(todos) => {
                for t in &todos {
                    let (mark, color, text) = match t.status {
                        TodoStatus::Completed => ("[x]", green, &t.content),
                        TodoStatus::InProgress => ("[~]", yellow, &t.active_form),
                        TodoStatus::Pending => ("[ ]", dim, &t.content),
                    };
                    let _ = writeln!(out, "{color}  {mark} {text}{reset}");
                }
            }
            UiEvent::Notice(msg) => {
                let _ = writeln!(out, "{dim}{msg}{reset}");
            }
            // oneshot 不生成标题；穷尽 match 需此臂。
            UiEvent::Title(_) => {}
            UiEvent::ModelChanged(_) => {}
            UiEvent::Divider(label) => {
                let _ = writeln!(out, "{dim}──── {label} ────{reset}");
            }
            // oneshot 不 resume，无历史可回放。
            UiEvent::ReplayHistory(_) => {}
            // 子 agent 进度：只在有变化的那一行打一句，别把终端刷满。
            UiEvent::AgentProgress { agents, .. } => {
                for a in &agents {
                    let tail = match a.status.as_str() {
                        "running" => a
                            .tool
                            .as_deref()
                            .map(|t| format!(" · {t}"))
                            .unwrap_or_default(),
                        other => format!(" · {other}"),
                    };
                    let _ = writeln!(out, "{magenta}  · {}{tail}{reset}", a.description);
                }
            }
            // oneshot 无 streaming 态可恢复，忽略。
            UiEvent::Idle => {}
            // 已经打出去的半截收不回来，另起一行说明接下来是重说一遍。
            UiEvent::Retrying {
                attempt,
                max,
                delay_ms,
                error,
            } => {
                let msg = retry_notice(attempt, max, delay_ms, &error);
                let _ = writeln!(out, "\n{yellow}{msg}{reset}");
            }
            UiEvent::AskUser {
                questions,
                response_tx,
                ..
            } => {
                // oneshot 模式（无 TUI）：打印问题，每题把第一个选项（缺省）发回作为答案。
                // 让 LLM 在 oneshot 流程里不会卡死。没有选项的题回空（= 跳过）。
                let mut answers: Vec<Vec<String>> = Vec::with_capacity(questions.len());
                for q in &questions {
                    let head = q.header.as_deref().unwrap_or("");
                    let _ = writeln!(
                        out,
                        "{yellow}[ask]{} {}{reset}",
                        if head.is_empty() {
                            String::new()
                        } else {
                            format!(" {head}")
                        },
                        q.question
                    );
                    if !q.options.is_empty() {
                        let labels: Vec<&str> =
                            q.options.iter().map(|o| o.label.as_str()).collect();
                        let _ = writeln!(out, "{yellow}  options: {}{reset}", labels.join(" | "));
                    }
                    answers.push(
                        q.options
                            .first()
                            .map(|o| vec![o.label.clone()])
                            .unwrap_or_default(),
                    );
                }
                if let Ok(mut guard) = response_tx.lock()
                    && let Some(tx) = guard.take()
                {
                    let _ = tx.send(answers);
                }
            }
            UiEvent::TurnUsage { usage, cost, model } => {
                let _ = writeln!(
                    out,
                    "\n[tokens] in={} out={} cache_read={} cache_write={} reasoning={} | cost=${:.4} ({})",
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache.read_tokens,
                    usage.cache.write.total,
                    usage.thinking_tokens,
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
        use serde_json::json;
        let msgs = vec![
            WireMessage::new(Role::System, vec![Block::text("sys")]), // 跳过
            WireMessage::user_text("问题"),
            WireMessage::new(Role::Assistant, vec![Block::text("回答")]),
            WireMessage::new(
                Role::Assistant,
                vec![Block::ToolUse {
                    id: "1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                    cache: None,
                }],
            ),
            WireMessage::new(
                Role::User,
                vec![Block::ToolResult {
                    tool_use_id: "1".into(),
                    content: crate::provider::ToolResultContent::Text {
                        text: format!("第一行\n{}", "x".repeat(500)),
                    },
                    is_error: false,
                    cache: None,
                }],
            ),
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
