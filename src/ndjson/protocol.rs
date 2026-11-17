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
    ///
    /// `answers` 与 `ask_user` 里的 `questions` 一一对应；每一项是该题选中的若干答案
    /// （空数组 = 这道题跳过了）。`answer` 是旧的单问题形态，兼容 host 的老实现：
    /// 只给了它时，答案整体算作第一题的回答。
    AskResponse {
        id: u64,
        #[serde(default)]
        answers: Option<Vec<Vec<String>>>,
        #[serde(default)]
        answer: Option<String>,
    },
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
    /// 反向 RPC：carter 弹出问题表单，host 必须用 `ask_response` 回复。
    ///
    /// 一次可以带多个问题（见 `agent::AskQuestion`）；host 回传同长度的 `answers`。
    AskUser {
        id: u64,
        questions: Vec<crate::agent::AskQuestion>,
    },
    /// 一轮结束的用量 + 成本。
    TurnUsage {
        usage: WireUsage,
        cost: f64,
        model: String,
    },
    /// `task` 工具里几个子 agent 的进度（整表快照，按 `call_id` 配对那张卡片）。
    AgentProgress {
        call_id: String,
        agents: Vec<WireAgent>,
    },
    /// 致命错误（一般紧随其后 carter 退出）。
    #[allow(dead_code)] // 暂未在 runner 主动构造；保留供将来 fatal error 透传给 host
    Error { message: String },
}

/// 一个子 agent 的进度（NDJSON 形态）。
#[derive(Debug, Clone, Serialize)]
pub struct WireAgent {
    pub description: String,
    /// `running` | `done` | `failed` | `cancelled`。
    pub status: String,
    pub steps: u32,
    pub tool: Option<String>,
    pub text: String,
}

impl From<&crate::agent::ui::AgentRun> for WireAgent {
    fn from(a: &crate::agent::ui::AgentRun) -> Self {
        Self {
            description: a.description.clone(),
            status: a.status.clone(),
            steps: a.steps,
            tool: a.tool.clone(),
            text: a.text.clone(),
        }
    }
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
    /// 底座的用量口径 → NDJSON 协议里那套字段名。
    ///
    /// 这个映射看着多余，但它是**协议兼容边界**：host 程序按 `input` / `cache_read`
    /// 这些名字解析，改了就是破坏性变更。底座换了（genai → llm_adapt），
    /// 对外那套名字不能跟着变。
    fn from(u: &Usage) -> Self {
        Self {
            input: u.input_tokens as u64,
            output: u.output_tokens as u64,
            cache_read: u.cache.read_tokens as u64,
            cache_write: u.cache.write.total as u64,
            reasoning: u.thinking_tokens as u64,
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

    /// 旧的单问题形态仍然认：`answer` 整体算作第一题的回答。
    #[test]
    fn parses_the_legacy_single_answer_ask_response() {
        let cmd = parse_line(r#"{"type":"ask_response","id":42,"answer":"yes"}"#).unwrap();
        match cmd {
            Command::AskResponse {
                id,
                answers,
                answer,
                ..
            } => {
                assert_eq!(id, 42);
                assert_eq!(answer.as_deref(), Some("yes"));
                assert!(answers.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    /// 新形态：`answers` 与 questions 一一对应。
    #[test]
    fn parses_multi_question_ask_response() {
        let cmd = parse_line(
            r#"{"type":"ask_response","id":7,"answers":[["Postgres"],["登录","导出"]]}"#,
        )
        .unwrap();
        match cmd {
            Command::AskResponse { id, answers, .. } => {
                assert_eq!(id, 7);
                assert_eq!(
                    answers.unwrap(),
                    vec![
                        vec!["Postgres".to_string()],
                        vec!["登录".to_string(), "导出".to_string()]
                    ]
                );
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
            questions: vec![crate::agent::AskQuestion {
                question: "ok?".into(),
                header: None,
                multi_select: false,
                options: vec![
                    crate::agent::AskOption {
                        label: "yes".into(),
                        description: None,
                    },
                    crate::agent::AskOption {
                        label: "no".into(),
                        description: None,
                    },
                ],
            }],
        };
        let line = to_line(&ev);
        assert!(line.contains("\"id\":7"));
        assert!(line.contains("\"question\":\"ok?\""));
        assert!(line.contains("\"yes\""));
    }
}
