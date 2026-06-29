//! NDJSON 模式入口 —— host driver 通过 stdin/stdout 与 carter 双向通信。
//!
//! 与 TUI / oneshot 平级，但**无终端 UI**。架构：
//! - stdin reader 任务：解析 host 命令，分发到 input_tx / cancel / pending ask
//! - agent 任务：跑 run_turn 循环（与 TUI 同款），sink = NdjsonSink → stdout
//! - main：等 stdin EOF / Stop 命令 → 收尾退出
//!
//! 协议见 [`protocol`]，类型 1:1 镜像 UiEvent。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::oneshot;

use crate::agent::{run_turn, CancelToken, CompactModel, RunOptions, ToolFactory};
use crate::config::Config;
use crate::provider::LlmProvider;

use super::protocol::Event as WireEvent;
use super::sink::NdjsonSink;

/// 启动 NDJSON 模式。返回时（stdin EOF / Stop 命令 / agent 退出）进程即可退出。
///
/// 注意：本函数尽量与 main::run_tui 的形态对称（同 build_tools / RunOptions），
/// 但不依赖任何终端 API，所以可在 daemon / IDE 子进程里跑。
#[allow(clippy::too_many_arguments)]
pub async fn run_ndjson(
    mut thread: crate::agent::Thread,
    session_meta: crate::session::SessionMeta,
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    fast_provider: Arc<dyn LlmProvider>,
    fast_model: crate::registry::ModelInfo,
    config: Config,
    show_thinking: bool,
    model_label: String,
    system: Vec<String>,
    cwd: std::path::PathBuf,
) -> crate::Result<std::process::ExitCode> {
    // 通道 & 共享状态。
    let (ui_tx, mut ui_rx) = unbounded_channel::<crate::agent::UiEvent>();
    let (input_tx, mut input_rx) = unbounded_channel::<String>();
    let cancel = CancelToken::new();
    let asks: super::sink::PendingAsks = Arc::new(Mutex::new(HashMap::new()));

    // 1. 提前 Ready 事件 —— 让 host 知道 carter 已就绪。
    super::sink::emit_event(&WireEvent::Ready {
        session_id: session_meta.id.clone(),
        model: model_label.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        resumed: session_meta.title.is_some(),
    });

    // 2. 构建工具池（含 ask_user_question —— ui_tx 透传，让模型能反向问 host）。
    let (mcp_mgr, mcp_tools) = crate::mcp::McpManager::start(&config.mcp).await;
    let hooks = Arc::new(crate::hooks::HookRegistry::from_configs(config.hooks.clone()));
    let agent_hooks = hooks.clone();

    // 3. 启动 stdin reader（独立 task，与 agent 并发）。
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let reader_input_tx = input_tx.clone();
    let reader_cancel = cancel.clone();
    let reader_asks = asks.clone();
    let reader_task = tokio::spawn(async move {
        super::reader::run_stdin_reader(reader_input_tx, reader_cancel, reader_asks, stop_tx).await;
    });

    // 4. 启动 agent 任务（与 TUI 同款的 while-let 循环）。
    let agent_cancel = cancel.clone();
    let agent_asks = asks.clone();
    let agent_task = tokio::spawn(async move {
        let base_system = system.first().cloned().unwrap_or_default();
        let run_opts = RunOptions {
            show_thinking,
            system,
            compact_model: Some(CompactModel {
                provider: fast_provider.clone(),
                model: fast_model.clone(),
            }),
            hooks: agent_hooks.clone(),
        };

        let tools = build_tools_for_ndjson(
            config.skills.enabled,
            provider.clone(),
            model.clone(),
            config.agent.clone(),
            base_system,
            agent_cancel.clone(),
            mcp_tools,
            ui_tx.clone(),
        );

        // SessionStart hook。
        agent_hooks
            .emit(
                crate::hooks::HookEvent::SessionStart,
                serde_json::json!({
                    "mode": "ndjson",
                    "session_id": session_meta.id,
                    "cwd": cwd.to_string_lossy(),
                    "model": model_label,
                    "resumed": session_meta.title.is_some(),
                }),
            )
            .await;

        let mut sink = NdjsonSink::new(agent_asks);

        // 续接：回放状态 = 让 host 知道当前消息数（host 可自行读 JSONL 取完整历史）。
        use crate::agent::UiSink;
        sink.emit(crate::agent::UiEvent::ModelChanged(model.key.clone()));
        if let Some(t) = &session_meta.title {
            sink.emit(crate::agent::UiEvent::Title(t.clone()));
        }

        let mut session_usage = crate::provider::Usage::default();
        while let Some(prompt) = input_rx.recv().await {
            // 跨会话输入历史也记录（让 carter sessions ↑↓ 列表可用）。
            crate::session::history::append(&session_meta.id, &prompt);
            // 内联 @图片附件转 token（与 TUI 一致）。
            let prompt = crate::media::inline_user_attachments(&prompt, &cwd);
            // UserPromptSubmit hook。
            let prompt = match crate::hooks::run_user_prompt_submit(&agent_hooks, prompt).await {
                Some(p) => p,
                None => {
                    sink.emit(crate::agent::UiEvent::Notice(
                        "[hook] prompt blocked by user_prompt_submit hook".into(),
                    ));
                    sink.emit(crate::agent::UiEvent::Idle);
                    continue;
                }
            };
            thread.append_user(prompt);

            if let Ok((_, usage)) = run_turn(
                &mut thread,
                &*provider,
                &model,
                &config.agent,
                &run_opts,
                &tools,
                &mut sink,
                &agent_cancel,
            )
            .await
            {
                session_usage.add(&usage);
            }
            sink.emit(crate::agent::UiEvent::Idle);
        }

        // SessionEnd hook。
        agent_hooks
            .emit(
                crate::hooks::HookEvent::SessionEnd,
                serde_json::json!({
                    "mode": "ndjson",
                    "input_tokens": session_usage.input,
                    "output_tokens": session_usage.output,
                }),
            )
            .await;
    });

    // 5. 主循环只做一件事：等 stop 信号 或 agent_task 结束。
    tokio::select! {
        _ = stop_rx => {
            // stdin EOF / Stop 命令 / 致命错误：通知 agent，等它收尾。
            drop(input_tx);
            cancel.set();
            agent_task.abort();
            let _ = agent_task.await;
        }
        _ = async {
            // 把 ui_rx 中残留事件吃干（防 channel 关闭前丢事件）。
            while ui_rx.recv().await.is_some() {}
        } => {}
    }
    reader_task.abort();
    mcp_mgr.shutdown().await;

    Ok(std::process::ExitCode::SUCCESS)
}

/// 构建工具池 —— 与 main::build_tools 几乎一样，但**始终**注册 ask_user_question
/// （NDJSON 模式下 host 端是"用户"，永远能接 RPC）。
#[allow(clippy::too_many_arguments)]
fn build_tools_for_ndjson(
    skills_enabled: bool,
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    agent_cfg: crate::config::AgentConfig,
    base_system: String,
    cancel: CancelToken,
    mcp_tools: Vec<Arc<dyn crate::tools::Tool>>,
    ui_tx: tokio::sync::mpsc::UnboundedSender<crate::agent::UiEvent>,
) -> crate::tools::ToolRegistry {
    let mut tools = crate::tools::ToolRegistry::builtin();
    if skills_enabled {
        tools.push(Arc::new(crate::skills::SkillTool::new(
            crate::config::paths::skills_dir(),
        )));
    }
    tools.push(Arc::new(crate::agent::AskUserQuestionTool::new(ui_tx)));
    for t in &mcp_tools {
        tools.push(t.clone());
    }

    // 子 agent 工厂（不含 task 自身，递归守卫）。
    let snapshot: Vec<Arc<dyn crate::tools::Tool>> = tools.tools().to_vec();
    let factory: ToolFactory = Arc::new(move || snapshot.clone());

    tools.push(Arc::new(crate::agent::TaskTool::new(
        provider,
        model,
        agent_cfg,
        base_system,
        cancel,
        factory,
        0,
    )));
    tools
}
