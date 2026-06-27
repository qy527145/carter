//! Turn 状态机 + 多轮 agent 循环（M2：模型调工具→执行→回灌→再调，直到无 tool_call 或达 max_turns）。
//! 纪律：本文件不得 import 任何 `genai::*`/`ratatui`/`crossterm` 类型，只依赖能力抽象层 + ui sink。

use futures::StreamExt;

use crate::config::AgentConfig;
use crate::provider::{ChatRequest, Event, LlmProvider, Message, StopReason, ToolCall, Usage};
use crate::registry::{Capability, ModelInfo};
use crate::tools::{parse_todos, TodoItem, TodoStatus, ToolRegistry};

use super::context;
use super::thread::Thread;
use super::ui::{tool_call_started, CancelToken, UiEvent, UiSink};

/// Turn 状态（M1 子集，预留 ToolPending/Approving/Executing/Observing 给后续）。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnState {
    Idle,
    Building,
    Streaming,
    Finalizing,
    Done,
    Cancelled,
}

/// 单 turn 结果。
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// 以 assistant message（无 tool_call）正常收尾。
    Assistant,
    /// 触达 max_turns / budget 等上限。
    Limit,
    /// 用户中断。
    Cancelled,
    /// 致命错误。
    #[allow(dead_code)]
    Error(String),
}

/// 控制流式输出的渲染选项。
pub struct RunOptions {
    pub show_thinking: bool,
    /// 注入到每轮请求的 system prompt（如 Skills 目录）。None = 不发 system。
    pub system_prompt: Option<String>,
    /// 上下文压缩专用模型；None = 复用主 provider+model。
    pub compact_model: Option<CompactModel>,
}

/// 压缩专用模型句柄：owned + Clone 友好，避免给 run_turn 加 `&dyn` 参数。
#[derive(Clone)]
pub struct CompactModel {
    pub provider: std::sync::Arc<dyn LlmProvider>,
    pub model: ModelInfo,
}

/// 跑一个 turn —— 多轮工具循环。
/// 每轮：构建请求 → 流式 → emit text/thinking、收集 tool_call → 累计 usage。
/// 无 tool_call → 收尾；有 → 回灌 assistant tool-call 消息 + 串行执行工具 + 回灌结果，再循环。
/// 渲染经 `ui` sink；`cancel` 命中则丢弃本轮、返回 Cancelled。
/// 返回 (结果, 跨轮累计 usage)。
pub async fn run_turn(
    thread: &mut Thread,
    provider: &dyn LlmProvider,
    model: &ModelInfo,
    agent_cfg: &AgentConfig,
    run_opts: &RunOptions,
    tools: &ToolRegistry,
    ui: &mut dyn UiSink,
    cancel: &CancelToken,
) -> crate::Result<(TurnOutcome, Usage)> {
    // 仅支持工具的模型才下发；循环内保持稳定（cache 友好）。
    let tool_specs = if model.supports(&Capability::Tools) {
        tools.specs()
    } else {
        Vec::new()
    };

    let mut total_usage = Usage::default();

    // 压缩阈值：真实 input token 超过 context_window * ratio 时触发。
    let compact_threshold =
        (model.context_window as f64 * agent_cfg.compact_threshold_ratio) as u64;
    let mut needs_compaction = false;

    loop {
        // 进入下一轮前，若上一轮真实 token 超阈值则先压缩。
        if needs_compaction {
            // 压缩择源：配了 fast 模型用之，否则复用主 provider+model。
            let (cprov, cmodel): (&dyn LlmProvider, &ModelInfo) = match &run_opts.compact_model {
                Some(cm) => (&*cm.provider, &cm.model),
                None => (provider, model),
            };
            // 压缩内部已降级处理失败，错误仅记录不中断。
            if let Err(e) = context::compact(thread, cprov, cmodel, compact_threshold, ui).await {
                ui.emit(UiEvent::Notice(format!("[compact] error: {e}")));
            }
            needs_compaction = false;
        }

        // 构建请求：clone 历史 + 注入 todo 复诵（推到最近注意力区，不进 append-only 历史）。
        let mut messages = thread.messages.clone();
        if !thread.todos.is_empty() {
            messages.push(Message::User(render_todos(&thread.todos)));
        }

        let req = ChatRequest {
            model_api_name: model.api_name.clone(),
            system: run_opts.system_prompt.clone(),
            messages,
            tools: tool_specs.clone(),
            reasoning: model.default_reasoning.clone(),
            max_output_tokens: agent_cfg.max_output_tokens,
        };

        let mut stream = provider.stream(req).await?;

        let mut assistant_text = String::new();
        let mut pending_calls: Vec<ToolCall> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut had_text_or_thinking = false;
        let mut cancelled = false;

        while let Some(ev) = {
            // 竞速：流的下一个事件 vs 取消通知。取消即时唤醒，断开卡住的 HTTP。
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    cancelled = true;
                    None
                }
                ev = stream.next() => ev,
            }
        } {
            match ev? {
                Event::ThinkingDelta(t) => {
                    if run_opts.show_thinking {
                        ui.emit(UiEvent::ThinkingDelta(t));
                        had_text_or_thinking = true;
                    }
                }
                Event::TextDelta(t) => {
                    assistant_text.push_str(&t);
                    ui.emit(UiEvent::AssistantTextDelta(t));
                    had_text_or_thinking = true;
                }
                Event::ToolCall(tc) => {
                    pending_calls.push(tc);
                }
                Event::Usage(u) => {
                    // 多轮累加（非覆盖）。
                    total_usage.add(&u);
                    // 用真实 input token（该轮完整 prompt）判定是否需压缩。
                    if agent_cfg.compact_enabled && u.input > compact_threshold {
                        needs_compaction = true;
                    }
                }
                Event::Done(reason) => {
                    stop_reason = reason;
                    break;
                }
            }
        }

        // 显式 drop 流，确保取消时立即断连。
        drop(stream);

        if had_text_or_thinking {
            ui.emit(UiEvent::AssistantTextEnd);
        }

        // 取消命中 → 重置 token、收尾历史（保留已生成文本）、返回 Cancelled。
        if cancelled {
            if !assistant_text.is_empty() {
                thread.append(Message::Assistant(assistant_text));
            }
            cancel.reset();
            return Ok((TurnOutcome::Cancelled, total_usage));
        }

        // 无工具调用 → 本 turn 收尾。
        if pending_calls.is_empty() {
            thread.turns += 1;
            if !assistant_text.is_empty() {
                thread.append(Message::Assistant(assistant_text));
            }
            emit_usage(ui, &total_usage, model);
            let outcome = match stop_reason {
                StopReason::MaxTokens => TurnOutcome::Limit,
                _ => TurnOutcome::Assistant,
            };
            return Ok((outcome, total_usage));
        }

        // 达上限 → 不再执行工具，提前收尾。
        if thread.turns >= agent_cfg.max_turns {
            emit_usage(ui, &total_usage, model);
            return Ok((TurnOutcome::Limit, total_usage));
        }

        // 回灌历史：先 append 前置文本（若有），再 append assistant tool-call 消息。
        if !assistant_text.is_empty() {
            thread.append(Message::Assistant(assistant_text));
        }
        thread.append(Message::ToolCalls(pending_calls.clone()));

        // 串行执行每个工具调用，结果按序回灌。
        for tc in &pending_calls {
            ui.emit(tool_call_started(tc));
            let result = tools.dispatch(&tc.name, tc.args.clone()).await;
            let first = result.content.lines().next().unwrap_or("");
            ui.emit(UiEvent::ToolResult {
                ok: result.ok,
                summary: super::ui::truncate_inline(first, 120),
            });
            // todo_write 特判：成功则把 todo 写入 Thread.todos（工具本身只校验+回显）。
            if tc.name == "todo_write" && result.ok {
                if let Ok(todos) = parse_todos(&tc.args) {
                    thread.todos = todos;
                    ui.emit(UiEvent::TodoUpdated(thread.todos.clone()));
                }
            }
            thread.append(Message::Tool {
                call_id: tc.id.clone(),
                content: result.to_model_string(),
            });
        }

        thread.turns += 1;
        // 回到循环顶部，带新历史再 stream。
    }
}

/// 组装并 emit 一轮的用量 + 成本。
fn emit_usage(ui: &mut dyn UiSink, usage: &Usage, model: &ModelInfo) {
    let cost = crate::cost::compute(usage, &model.pricing);
    ui.emit(UiEvent::TurnUsage {
        usage: usage.clone(),
        cost,
        model: model.api_name.clone(),
    });
}

/// 渲染 todo 列表为复诵文本（注入 context 末尾，推到最近注意力区）。
fn render_todos(todos: &[TodoItem]) -> String {
    let mut out = String::from("当前待办（持续推进，完成即更新状态）：\n");
    for t in todos {
        match t.status {
            TodoStatus::Completed => out.push_str(&format!("[x] {}\n", t.content)),
            // 进行中用 active_form 复诵当前动作。
            TodoStatus::InProgress => out.push_str(&format!("[~] {}\n", t.active_form)),
            TodoStatus::Pending => out.push_str(&format!("[ ] {}\n", t.content)),
        }
    }
    out
}
