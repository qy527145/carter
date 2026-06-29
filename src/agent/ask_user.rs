//! AskUserQuestion 工具 —— 模型自主向用户/TUI 弹问题，等用户选择后把答案回灌。
//!
//! 与 TaskTool 同理：调用 ui 通道发 `UiEvent::AskUser`，配上 oneshot Sender；
//! TUI 收到事件弹窗、用户按数字/Enter 后 send 答案；工具 await 拿到答案返回模型。
//!
//! oneshot 模式（无 TUI）：StdoutSink 接到 AskUser 后自动用第一个 option 作答，
//! 避免一次性执行被卡死。

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::tools::{Tool, ToolResult};

use super::ui::UiEvent;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 工具实例：持有一个把事件发到 TUI 的 sender。
pub struct AskUserQuestionTool {
    ui_tx: UnboundedSender<UiEvent>,
}

impl AskUserQuestionTool {
    pub fn new(ui_tx: UnboundedSender<UiEvent>) -> Self {
        Self { ui_tx }
    }
}

#[async_trait::async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "ask_user_question"
    }

    fn description(&self) -> &str {
        "向用户弹出一个多选/自由文本问题，等用户选择或输入后把答案返回。\
         适合：需求歧义、需要用户决策、敏感操作前确认。\n\
         用户可按数字键直接选 options，或键入自由答案后回车，或 Esc 跳过 = 选第一项。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "要问的问题（自然语言、明确具体）"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "候选答案数组（缺省也可让用户自由输入）"
                }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let question = match args.get("question").and_then(Value::as_str) {
            Some(q) if !q.is_empty() => q.to_string(),
            _ => return ToolResult::err("missing argument: question"),
        };
        let options: Vec<String> = args
            .get("options")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let (tx, rx) = oneshot::channel::<String>();
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let event = UiEvent::AskUser {
            id,
            question,
            options,
            response_tx: std::sync::Arc::new(std::sync::Mutex::new(Some(tx))),
        };
        if self.ui_tx.send(event).is_err() {
            return ToolResult::err("ask_user_question: UI channel closed");
        }
        match rx.await {
            Ok(answer) if !answer.is_empty() => ToolResult::ok(answer),
            Ok(_) => ToolResult::ok("(no answer)"),
            Err(_) => ToolResult::err("ask_user_question: oneshot dropped (UI gone?)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_question_errors() {
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = AskUserQuestionTool::new(ui_tx);
        let res = tool.execute(json!({})).await;
        assert!(!res.ok);
    }

    #[tokio::test]
    async fn emits_event_and_waits_for_response() {
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = AskUserQuestionTool::new(ui_tx);
        // 模拟 TUI：另起一个任务接事件、发回答案。
        tokio::spawn(async move {
            if let Some(UiEvent::AskUser { response_tx, .. }) = ui_rx.recv().await {
                if let Ok(mut guard) = response_tx.lock() {
                    if let Some(tx) = guard.take() {
                        let _ = tx.send("yes".to_string());
                    }
                }
            }
        });
        let res = tool
            .execute(json!({
                "question": "OK?",
                "options": ["yes", "no"]
            }))
            .await;
        assert!(res.ok);
        assert_eq!(res.content, "yes");
    }
}
