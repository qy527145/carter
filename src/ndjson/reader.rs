//! NDJSON 模式的 stdin 命令读取任务 —— 每行解析一个 Command，分发到 agent 通道 / cancel /
//! pending ask 表。
//!
//! 容错：
//! - 解析失败的行只 warn 不退出（host 偶发发垃圾不应杀死 carter）
//! - host 关闭 stdin → 任务结束 → 上层退出
//!
//! 注意：`set_model` 切换模型逻辑复杂（要拿配置 + cache_json + 提供给 agent loop），
//! 当前简化为发 Notice 提示用户改用 `--model` 启动；未来可扩展接入主循环。

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::CancelToken;

use super::protocol::{parse_line, Command};
use super::sink::PendingAsks;

/// 读取 stdin 直到 EOF。每行解析一个 Command 并 dispatch。
/// 返回时（EOF / 致命 IO 错误）调用 stop 句柄让上层退出。
pub async fn run_stdin_reader(
    input_tx: UnboundedSender<String>,
    cancel: CancelToken,
    asks: PendingAsks,
    stop: tokio::sync::oneshot::Sender<()>,
) {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    loop {
        let next = lines.next_line().await;
        let line = match next {
            Ok(Some(l)) => l,
            Ok(None) => {
                tracing::debug!("ndjson: stdin EOF; signalling stop");
                let _ = stop.send(());
                return;
            }
            Err(e) => {
                tracing::warn!("ndjson: stdin read error ({e}); signalling stop");
                let _ = stop.send(());
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(&line) {
            Ok(cmd) => {
                if let Some(should_stop) = dispatch(cmd, &input_tx, &cancel, &asks).await {
                    if should_stop {
                        let _ = stop.send(());
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("ndjson: malformed command line ({e}): {line}");
                // 给 host 一个明确反馈，但不杀进程。
                super::sink::emit_event(&super::protocol::Event::Notice {
                    message: format!("[ndjson] malformed command: {e}"),
                });
            }
        }
    }
}

/// 分发单个 Command。返回 `Some(true)` 表示应停止主循环；`Some(false)` 已处理但继续；
/// `None` 等同 false（语义保持简单）。
/// 仅供同 crate 单测使用的 dispatch 包装（不对外公开真实 dispatch 签名）。
#[cfg(test)]
pub(crate) async fn dispatch_for_test(
    cmd: Command,
    input_tx: &UnboundedSender<String>,
    cancel: &CancelToken,
    asks: &super::sink::PendingAsks,
) -> Option<bool> {
    dispatch(cmd, input_tx, cancel, asks).await
}

async fn dispatch(
    cmd: Command,
    input_tx: &UnboundedSender<String>,
    cancel: &CancelToken,
    asks: &Arc<std::sync::Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<String>>>>,
) -> Option<bool> {
    match cmd {
        Command::UserPrompt { text } => {
            if input_tx.send(text).is_err() {
                tracing::warn!("ndjson: input channel closed; stopping");
                return Some(true);
            }
            Some(false)
        }
        Command::Cancel => {
            cancel.set();
            Some(false)
        }
        Command::SetModel { model } => {
            // /model 热切换在 TUI 路径里走 dispatch_builtin，那里能拿到 config / cache_json /
            // 主循环可变 model+provider。NDJSON 路径暂不开放（避免在通道上再开一条命令链路）。
            // host 若需切模型请重启 carter 并带 --model 参数。
            super::sink::emit_event(&super::protocol::Event::Notice {
                message: format!(
                    "[ndjson] set_model 暂不支持热切换；请用 --model {model} 重启 carter"
                ),
            });
            Some(false)
        }
        Command::AskResponse { id, answer } => {
            let mut pending = match asks.lock() {
                Ok(p) => p,
                Err(p) => p.into_inner(),
            };
            match pending.remove(&id) {
                Some(tx) => {
                    if tx.send(answer).is_err() {
                        tracing::warn!("ndjson: ask_response receiver dropped (id={id})");
                    }
                }
                None => {
                    tracing::warn!("ndjson: ask_response for unknown id={id}; ignoring");
                }
            }
            Some(false)
        }
        Command::Stop => Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::sync::mpsc::unbounded_channel;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn user_prompt_forwards_to_input_channel() {
        let (tx, mut rx) = unbounded_channel::<String>();
        let cancel = CancelToken::new();
        let asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let stop = dispatch(
            Command::UserPrompt { text: "hi".into() },
            &tx,
            &cancel,
            &asks,
        )
        .await;
        assert_eq!(stop, Some(false));
        assert_eq!(rx.try_recv().unwrap(), "hi");
    }

    #[tokio::test]
    async fn cancel_sets_token() {
        let (tx, _rx) = unbounded_channel::<String>();
        let cancel = CancelToken::new();
        let asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        dispatch(Command::Cancel, &tx, &cancel, &asks).await;
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn stop_returns_true() {
        let (tx, _rx) = unbounded_channel::<String>();
        let cancel = CancelToken::new();
        let asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let r = dispatch(Command::Stop, &tx, &cancel, &asks).await;
        assert_eq!(r, Some(true));
    }

    #[tokio::test]
    async fn ask_response_fulfills_pending_oneshot() {
        let (tx, _rx) = unbounded_channel::<String>();
        let cancel = CancelToken::new();
        let asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));

        let (otx, orx) = oneshot::channel::<String>();
        asks.lock().unwrap().insert(7, otx);

        dispatch(
            Command::AskResponse {
                id: 7,
                answer: "yes".into(),
            },
            &tx,
            &cancel,
            &asks,
        )
        .await;

        // pending 表里该 id 已被消费。
        assert!(asks.lock().unwrap().is_empty());
        // oneshot 收到回复。
        assert_eq!(orx.await.unwrap(), "yes");
    }

    #[tokio::test]
    async fn ask_response_unknown_id_is_silently_ignored() {
        let (tx, _rx) = unbounded_channel::<String>();
        let cancel = CancelToken::new();
        let asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let r = dispatch(
            Command::AskResponse {
                id: 999,
                answer: "y".into(),
            },
            &tx,
            &cancel,
            &asks,
        )
        .await;
        assert_eq!(r, Some(false));
    }
}
