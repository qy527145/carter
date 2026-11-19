//! NdjsonSink —— 实现 `UiSink`，把每条 UiEvent 转成 NDJSON 行写到 stdout。
//!
//! 与 TUI 的 `ChannelSink` 平级；本 sink 把 `UiEvent::AskUser` 的 oneshot Sender
//! **存进 pending 表**（按 id 索引），并向 stdout 发只含 id+question+options 的 `Event::AskUser`。
//! stdin 读取任务收到匹配 id 的 `Command::AskResponse` 后，从 pending 表取出 Sender 把答案发回，
//! AskUserQuestionTool 内部的 `rx.await` 即返回。
//!
//! 写盘：每事件一次 `writeln!` + flush（保证 host 即时收到流式 delta，不被 IO buffer 黏起来）。

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::agent::ui::{UiEvent, UiSink};

use super::protocol::{Event, WireAgent, WireTodo, WireUsage, to_line};

/// 等待 host 回复的 AskUser 表：id → 已脱离 UiEvent 的 oneshot Sender。
/// 回传的是「第 i 个问题 → 选中的若干项」。
pub type PendingAsks = Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<Vec<String>>>>>>;

/// 写入 stdout 的 NDJSON sink。
pub struct NdjsonSink {
    /// 共享给 stdin 读取任务的 pending ask 表。
    asks: PendingAsks,
    /// 没有 host 在 stdin 那头回话（`--output-format stream-json` 的一次性模式）：
    /// `ask_user` 事件照发，但答案由本 sink 当场按首选项填上，否则工具会一直等。
    auto_answer: bool,
}

impl NdjsonSink {
    pub fn new(asks: PendingAsks) -> Self {
        Self {
            asks,
            auto_answer: false,
        }
    }

    /// 单向版本：只往 stdout 写事件，不指望 stdin 有人回。
    pub fn detached() -> Self {
        Self {
            asks: Arc::new(Mutex::new(HashMap::new())),
            auto_answer: true,
        }
    }

    fn write_event(&self, event: &Event) {
        let line = to_line(event);
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        // 失败（pipe 关闭）说明 host 跑路了；忽略 —— 进程下一轮自然退出。
        let _ = handle.write_all(line.as_bytes());
        let _ = handle.flush();
    }
}

impl UiSink for NdjsonSink {
    fn emit(&mut self, ev: UiEvent) {
        let event = match ev {
            UiEvent::AssistantTextDelta(t) => Event::TextDelta { text: t },
            UiEvent::ThinkingDelta(t) => Event::ThinkingDelta { text: t },
            UiEvent::AssistantTextEnd => Event::TextEnd,
            UiEvent::ToolCallStarted { name, args_preview } => {
                Event::ToolCallStarted { name, args_preview }
            }
            UiEvent::ToolResult { ok, summary } => Event::ToolResult { ok, summary },
            UiEvent::TodoUpdated(todos) => Event::TodoUpdated {
                todos: todos.iter().map(WireTodo::from).collect(),
            },
            UiEvent::Notice(message) => Event::Notice { message },
            UiEvent::Title(title) => Event::Title { title },
            UiEvent::ModelChanged(model) => Event::ModelChanged { model },
            UiEvent::Divider(label) => Event::Divider { label },
            UiEvent::Idle => Event::Idle,
            UiEvent::TurnUsage { usage, cost, model } => Event::TurnUsage {
                usage: WireUsage::from(&usage),
                cost,
                model,
            },
            // ReplayHistory 在 NDJSON 模式没有特别用处（host 可自己读 JSONL），
            // 但仍以 Notice 形式透出便于调试。
            UiEvent::ReplayHistory(items) => Event::Notice {
                message: format!("[replay] {} entries", items.len()),
            },
            // AskUser：抢下 oneshot Sender 存入 pending 表，发 ask_user event 给 host。
            UiEvent::AskUser {
                id,
                questions,
                response_tx,
            } => {
                // 提取 Sender —— UiEvent::AskUser.response_tx 是 Arc<Mutex<Option<Sender>>>，
                // 设计上"只有一个 sink 应该消费"。这里 take 出来移交 pending 表。
                if let Ok(mut guard) = response_tx.lock()
                    && let Some(tx) = guard.take()
                {
                    if self.auto_answer {
                        // 与 StdoutSink 的 oneshot 策略一致：每题取第一个选项，无选项则跳过。
                        let answers: Vec<Vec<String>> = questions
                            .iter()
                            .map(|q| {
                                q.options
                                    .first()
                                    .map(|o| vec![o.label.clone()])
                                    .unwrap_or_default()
                            })
                            .collect();
                        let _ = tx.send(answers);
                    } else if let Ok(mut pending) = self.asks.lock() {
                        pending.insert(id, tx);
                    }
                }
                Event::AskUser { id, questions }
            }
            // 子 agent 进度：NDJSON host 按 `agent_progress` 画那张卡片。
            UiEvent::AgentProgress { call_id, agents } => Event::AgentProgress {
                call_id,
                agents: agents.iter().map(WireAgent::from).collect(),
            },
        };
        self.write_event(&event);
    }
}

/// 直接写一个 Event 到 stdout（供启动 / 错误 / 显式状态使用）。
pub fn emit_event(event: &Event) {
    let line = to_line(event);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(line.as_bytes());
    let _ = handle.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::TodoItem;

    fn new_sink() -> NdjsonSink {
        NdjsonSink::new(Arc::new(Mutex::new(HashMap::new())))
    }

    /// 仅验证不 panic + 不阻塞；输出 stdout 在测试里难直接断言，
    /// 重点用 protocol::to_line / parse_line 的单测覆盖序列化正确性。
    #[test]
    fn emit_does_not_panic_on_various_events() {
        let mut s = new_sink();
        s.emit(UiEvent::AssistantTextDelta("hi".into()));
        s.emit(UiEvent::AssistantTextEnd);
        s.emit(UiEvent::Notice("note".into()));
        s.emit(UiEvent::Idle);
        s.emit(UiEvent::TodoUpdated(vec![TodoItem {
            status: crate::tools::TodoStatus::Pending,
            content: "x".into(),
            active_form: "doing x".into(),
        }]));
    }

    #[test]
    fn ask_user_event_stores_sender_in_pending_table() {
        let asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let mut sink = NdjsonSink::new(asks.clone());

        let (tx, _rx) = oneshot::channel::<Vec<Vec<String>>>();
        let response_tx = Arc::new(Mutex::new(Some(tx)));
        sink.emit(UiEvent::AskUser {
            id: 99,
            questions: vec![crate::agent::AskQuestion {
                question: "ok?".into(),
                header: None,
                multi_select: false,
                options: vec![crate::agent::AskOption {
                    label: "yes".into(),
                    description: None,
                }],
            }],
            response_tx,
        });

        let pending = asks.lock().unwrap();
        assert!(pending.contains_key(&99));
    }

    /// 端到端：sink emit AskUser → pending 表存 Sender；
    /// reader dispatch AskResponse(同 id) → Sender 收到 host 的答案。
    #[tokio::test]
    async fn ask_response_roundtrip_through_pending_table() {
        use crate::agent::CancelToken;
        use crate::ndjson::protocol::Command;
        use tokio::sync::mpsc::unbounded_channel;

        let asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let mut sink = NdjsonSink::new(asks.clone());

        // 模拟 AskUserQuestionTool 内部建的 oneshot：tool 端持 rx，sink 端拿 tx。
        let (tool_tx, tool_rx) = oneshot::channel::<Vec<Vec<String>>>();
        sink.emit(UiEvent::AskUser {
            id: 42,
            questions: vec![crate::agent::AskQuestion {
                question: "do?".into(),
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
            response_tx: Arc::new(Mutex::new(Some(tool_tx))),
        });

        // host 回复（走 reader 的 dispatch 路径）。
        let (input_tx, _input_rx) = unbounded_channel::<String>();
        let cancel = CancelToken::new();
        crate::ndjson::reader::dispatch_for_test(
            Command::AskResponse {
                id: 42,
                answers: Some(vec![vec!["yes".to_string()]]),
                answer: None,
            },
            &input_tx,
            &crate::agent::SteerInbox::new(),
            &cancel,
            &asks,
        )
        .await;

        // 工具端应收到答案。
        let answer = tool_rx.await.unwrap();
        assert_eq!(answer, vec![vec!["yes".to_string()]]);
        // pending 表已清空。
        assert!(asks.lock().unwrap().is_empty());
    }
}
