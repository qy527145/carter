//! NDJSON 协议类型 —— host ↔ carter 双向消息。
//!
//! 每行 = 一个完整 JSON 对象（`\n` 分隔，UTF-8）。
//! - **Command**：host → carter（stdin）
//! - **Event**：carter → host（stdout）
//!
//! 设计原则：
//! 1. 与 `agent::UiEvent` 一一对应（不重新发明事件类型）—— Event 是 UiEvent 的"序列化镜像"，
//!    去掉 oneshot Sender 等不可序列化字段。
//! 2. Command 简短：仅 host 主动驱动的几个动作（user_prompt / cancel / set_model /
//!    ask_response / stop）。
//! 3. AskUser 反向 RPC：carter 发 `ask_user`（带 id），host 必须用 `ask_response`（同 id）回复，
//!    否则子 agent 的工具调用会一直 await。

use serde::{Deserialize, Serialize};

use crate::provider::Usage;
use crate::tools::{TodoItem, TodoStatus};

/// host → carter。每行一个 Command。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// 提交 user prompt 给 agent loop（与 TUI 回车等价）。
    UserPrompt { text: String },
    /// 取消当前流式请求（与 TUI Esc 等价）。
    Cancel,
    /// 热切换模型（与 TUI `/model <ref>` 等价）。
    SetModel { model: String },
    /// 回答之前发出的 AskUser（id 必须对应）。
    AskResponse { id: u64, answer: String },
    /// 优雅退出（agent 任务收尾后 carter 进程退出）。
    Stop,
}

/// carter → host。每行一个 Event。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// 启动完成 / 会话就绪。host 收到后可以开始发 user_prompt。
    Ready {
        session_id: String,
        model: String,
        cwd: String,
        resumed: bool,
    },
    /// 模型正文增量。
    TextDelta { text: String },
    /// 思考增量。
    ThinkingDelta { text: String },
    /// 一个 assistant 文本块结束。
    TextEnd,
    /// 工具调用开始。
    ToolCallStarted { name: String, args_preview: String },
    /// 工具结果。
    ToolResult { ok: bool, summary: String },
    /// todo 列表更新。
    TodoUpdated { todos: Vec<WireTodo> },
    /// 系统通知（压缩日志、错误等）。
    Notice { message: String },
    /// 会话标题（fast 模型生成一次）。
    Title { title: String },
    /// 当前模型变更。
    ModelChanged { model: String },
    /// 上下文边界分隔线。
    Divider { label: String },
    /// 一轮 / 斜杠命令处理结束 —— host 据此判断何时显示"输入空闲"。
    Idle,
    /// 反向 RPC：carter 弹出选择题，host 必须用 `ask_response` 回复。
    AskUser {
        id: u64,
        question: String,
        options: Vec<String>,
    },
    /// 一轮结束的用量 + 成本。
    TurnUsage {
        usage: WireUsage,
        cost: f64,
        model: String,
    },
    /// 致命错误（一般紧随其后 carter 退出）。
    #[allow(dead_code)] // 暂未在 runner 主动构造；保留供将来 fatal error 透传给 host
    Error { message: String },
}

/// 序列化用的 todo（用 lowercase enum 字段名，便于 host 直接 match）。
#[derive(Debug, Clone, Serialize)]
pub struct WireTodo {
    pub status: WireTodoStatus,
    pub content: String,
    pub active_form: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireTodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl From<&TodoItem> for WireTodo {
    fn from(t: &TodoItem) -> Self {
        Self {
            status: match t.status {
                TodoStatus::Pending => WireTodoStatus::Pending,
                TodoStatus::InProgress => WireTodoStatus::InProgress,
                TodoStatus::Completed => WireTodoStatus::Completed,
            },
            content: t.content.clone(),
            active_form: t.active_form.clone(),
        }
    }
}

/// 序列化用的 usage（与 internal Usage 同字段，去掉 derive 依赖）。
#[derive(Debug, Clone, Serialize)]
pub struct WireUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl From<&Usage> for WireUsage {
    fn from(u: &Usage) -> Self {
        Self {
            input: u.input,
            output: u.output,
            cache_read: u.cache_read,
            cache_write: u.cache_write,
            reasoning: u.reasoning,
        }
    }
}

/// 序列化任意 Value 到一行 JSON（保留行末 `\n`）。供 sink 直接写 stdout。
pub fn to_line(event: &Event) -> String {
    match serde_json::to_string(event) {
        Ok(s) => format!("{s}\n"),
        Err(e) => {
            // 极少发生（Event 全是 owned 字段），仍兜底以 error 形态发出。
            format!(
                "{{\"type\":\"error\",\"message\":\"event serialize failed: {}\"}}\n",
                e.to_string().replace('"', "'")
            )
        }
    }
}

/// 解析一行 stdin。失败返回 Err（host 写出错误，host 自己处理）。
pub fn parse_line(line: &str) -> serde_json::Result<Command> {
    serde_json::from_str(line.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_text_delta() {
        let ev = Event::TextDelta { text: "hi".into() };
        let line = to_line(&ev);
        assert!(line.starts_with("{"));
        assert!(line.ends_with("\n"));
        assert!(line.contains("\"type\":\"text_delta\""));
        assert!(line.contains("\"text\":\"hi\""));
    }

    #[test]
    fn parses_user_prompt_command() {
        let cmd = parse_line(r#"{"type":"user_prompt","text":"hello"}"#).unwrap();
        match cmd {
            Command::UserPrompt { text } => assert_eq!(text, "hello"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_ask_response_with_id() {
        let cmd = parse_line(r#"{"type":"ask_response","id":42,"answer":"yes"}"#).unwrap();
        match cmd {
            Command::AskResponse { id, answer } => {
                assert_eq!(id, 42);
                assert_eq!(answer, "yes");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn malformed_returns_err() {
        assert!(parse_line("not json").is_err());
        assert!(parse_line(r#"{"type":"unknown"}"#).is_err());
    }

    /// 防回归：协议的 tag 名是 snake_case。host 端可能依赖具体字符串。
    #[test]
    fn event_type_tags_are_snake_case() {
        let cases = [
            (Event::TextEnd, "text_end"),
            (Event::Idle, "idle"),
            (
                Event::ToolCallStarted {
                    name: "x".into(),
                    args_preview: "".into(),
                },
                "tool_call_started",
            ),
        ];
        for (ev, expected) in cases {
            let line = to_line(&ev);
            assert!(
                line.contains(&format!("\"type\":\"{expected}\"")),
                "missing tag {expected} in {line}",
            );
        }
    }

    /// Value 字段也要原样直传。
    #[test]
    fn ask_user_carries_question_and_options() {
        let ev = Event::AskUser {
            id: 7,
            question: "ok?".into(),
            options: vec!["yes".into(), "no".into()],
        };
        let line = to_line(&ev);
        assert!(line.contains("\"id\":7"));
        assert!(line.contains("\"question\":\"ok?\""));
        assert!(line.contains("\"yes\""));
    }
}
