//! Turn 状态机 + 多轮 agent 循环（M2：模型调工具→执行→回灌→再调，直到无 tool_call 或达 max_turns）。
//! 纪律：本文件不得 import 任何 `genai::*`/`ratatui`/`crossterm` 类型，只依赖能力抽象层 + ui sink。

use futures::stream::FuturesOrdered;
use futures::StreamExt;

use crate::config::AgentConfig;
use crate::hooks::{HookDecision, HookEvent, HookRegistry};
use crate::provider::{ChatRequest, Event, LlmProvider, Message, StopReason, ToolCall, Usage};
use crate::registry::{Capability, ModelInfo};
use crate::tools::{is_concurrent_safe, parse_todos, TodoItem, TodoStatus, ToolRegistry, ToolResult};

use super::context;
use super::thread::Thread;
use super::ui::{CancelToken, UiEvent, UiSink};

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
    /// 注入到每轮请求的 system 分段（按序：人设 + skills + 多层记忆 + 运行环境）。
    /// 空 = 不发 system。
    pub system: Vec<String>,
    /// 上下文压缩专用模型；None = 复用主 provider+model。
    pub compact_model: Option<CompactModel>,
    /// Hook 注册表（pre/post tool use 等事件触发）。默认为空注册表 = 全 Continue。
    pub hooks: std::sync::Arc<HookRegistry>,
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
            system: run_opts.system.clone(),
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

        // 工具批量执行：连续的"并发安全"工具一起跑；遇到 unsafe 工具切断、串行跑。
        // 顺序保持：派发顺序与回灌顺序与 model 给的 pending_calls 完全一致。
        // hook 在每个工具前后触发（PreToolUse 可改写 args / 阻断；PostToolUse 仅观测）。
        let results = execute_tool_calls(&pending_calls, tools, thread, ui, &run_opts.hooks).await;

        // 写盘 + 特判 todo_write（成功则把 todo 写入 Thread.todos）。
        for (tc, result) in pending_calls.iter().zip(results.iter()) {
            if tc.name == "todo_write" && result.ok {
                if let Ok(todos) = parse_todos(&tc.args) {
                    thread.set_todos(todos);
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

/// 把同一轮的所有工具调用按"安全/不安全"分批执行，保持原顺序。
///
/// 切批规则：
/// - 连续的 `is_concurrent_safe` 工具组成一个并发批，用 `FuturesOrdered` 并发派发、按序收集；
/// - 遇到 unsafe 工具就关闭当前批、单独串行执行该 unsafe 调用；
/// - 然后继续下一批。
///
/// Hook：每个工具调用前触发 `PreToolUse`（可改写 args、可阻断）、后触发 `PostToolUse`（仅观测）。
/// 阻断时跳过真正的工具执行，直接返回一个结构化的 ToolResult::err。
///
/// UI/checkpoint：每个调用在被派发前 emit `ToolCallStarted` + 抓写前快照；
/// 结果回收后 emit `ToolResult`。
async fn execute_tool_calls(
    calls: &[ToolCall],
    tools: &ToolRegistry,
    thread: &mut super::thread::Thread,
    ui: &mut dyn UiSink,
    hooks: &HookRegistry,
) -> Vec<ToolResult> {
    // 第 1 阶段：对每个调用过一遍 PreToolUse hook，得到一个"待执行"列表
    // （可能改写过 args，可能直接被阻断为 ToolResult::err）。
    // 注：hook 调用是 async + 顺序敏感，故在并发派发**之前**统一过一遍。
    let mut prepared: Vec<PreparedCall> = Vec::with_capacity(calls.len());
    for tc in calls {
        let mut effective = tc.clone();
        let mut blocked: Option<String> = None;
        if hooks.has(HookEvent::PreToolUse) {
            let payload = serde_json::json!({
                "tool": tc.name,
                "args": tc.args,
            });
            match hooks.dispatch(HookEvent::PreToolUse, payload).await {
                HookDecision::Continue => {}
                HookDecision::Rewrite(v) => {
                    // 仅接受 `{"args": {...}}` 形态的改写；其它字段忽略。
                    if let Some(new_args) = v.get("args") {
                        effective.args = new_args.clone();
                    }
                }
                HookDecision::Block { reason } => {
                    blocked = Some(reason);
                }
            }
        }
        prepared.push(PreparedCall {
            call: effective,
            blocked,
        });
    }

    // 第 2 阶段：按"安全/不安全"分批执行，被阻断的直接当 err 结果回灌。
    let mut out: Vec<ToolResult> = Vec::with_capacity(prepared.len());
    let mut i = 0;
    while i < prepared.len() {
        // 被阻断的：直接合成 err 结果，不进任何批次。
        if let Some(reason) = prepared[i].blocked.clone() {
            ui.emit(super::ui::tool_call_started(&prepared[i].call));
            let res = ToolResult::err(format!("blocked by hook: {reason}"));
            emit_tool_result(ui, &res);
            // 触发 PostToolUse 让观测类 hook 也能看到（payload 含 blocked 标记）。
            post_tool_hook(hooks, &prepared[i].call, &res).await;
            out.push(res);
            i += 1;
            continue;
        }

        if is_concurrent_safe(&prepared[i].call.name) {
            // 收集本批连续 safe 且非阻断的调用。
            let start = i;
            while i < prepared.len()
                && prepared[i].blocked.is_none()
                && is_concurrent_safe(&prepared[i].call.name)
            {
                i += 1;
            }
            let batch = &prepared[start..i];
            for p in batch {
                ui.emit(super::ui::tool_call_started(&p.call));
            }
            // 并发派发、按序收集。
            let mut fo: FuturesOrdered<_> = batch
                .iter()
                .map(|p| {
                    let name = p.call.name.clone();
                    let args = p.call.args.clone();
                    async move { tools.dispatch(&name, args).await }
                })
                .collect();
            let mut batch_results: Vec<ToolResult> = Vec::with_capacity(batch.len());
            while let Some(res) = fo.next().await {
                emit_tool_result(ui, &res);
                batch_results.push(res);
            }
            // PostToolUse 在并发返回后串行触发（hook 顺序与原调用顺序一致）。
            for (p, res) in batch.iter().zip(batch_results.iter()) {
                post_tool_hook(hooks, &p.call, res).await;
            }
            out.extend(batch_results);
        } else {
            // 串行：unsafe 工具单独跑，先抓写前快照。
            let tc = &prepared[i].call;
            ui.emit(super::ui::tool_call_started(tc));
            let paths = super::checkpoint::mutating_paths(&tc.name, &tc.args);
            if !paths.is_empty() {
                thread
                    .checkpoints
                    .snapshot(format!("{} {}", tc.name, paths[0].display()), &paths);
            }
            let res = tools.dispatch(&tc.name, tc.args.clone()).await;
            emit_tool_result(ui, &res);
            post_tool_hook(hooks, tc, &res).await;
            out.push(res);
            i += 1;
        }
    }
    out
}

/// 单个工具调用的"经 PreToolUse hook 处理后"的状态。
struct PreparedCall {
    /// 改写后的调用（args 可能已被 hook 修改）。
    call: ToolCall,
    /// 若被 hook 阻断，此处为阻断原因；否则 None。
    blocked: Option<String>,
}

/// 触发单个工具的 PostToolUse hook（fire-and-forget 语义，仅观测，不修改结果）。
async fn post_tool_hook(hooks: &HookRegistry, call: &ToolCall, res: &ToolResult) {
    if !hooks.has(HookEvent::PostToolUse) {
        return;
    }
    let payload = serde_json::json!({
        "tool": call.name,
        "args": call.args,
        "ok": res.ok,
        "content": res.content,
    });
    // PostToolUse 不可改写、不可阻断；忽略 Rewrite/Block。
    let _ = hooks.dispatch(HookEvent::PostToolUse, payload).await;
}

fn emit_tool_result(ui: &mut dyn UiSink, result: &ToolResult) {
    let first = result.content.lines().next().unwrap_or("");
    ui.emit(UiEvent::ToolResult {
        ok: result.ok,
        summary: super::ui::truncate_inline(first, 120),
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use serde_json::json;

    /// 收集 sink：把所有事件按序攒起来供断言。
    #[derive(Default)]
    struct VecSink(Vec<UiEvent>);
    impl UiSink for VecSink {
        fn emit(&mut self, ev: UiEvent) {
            self.0.push(ev);
        }
    }

    #[tokio::test]
    async fn safe_and_unsafe_batches_emit_in_call_order() {
        // 混合调用：grep(safe), bash(unsafe), read_file(safe)。
        // 期望：started 事件按原顺序 emit，结果数量与 calls 一致。
        let calls = vec![
            ToolCall { id: "1".into(), name: "grep".into(), args: json!({"pattern":"x"}) },
            ToolCall { id: "2".into(), name: "bash".into(), args: json!({"command":"true"}) },
            ToolCall { id: "3".into(), name: "read_file".into(), args: json!({"path":"Cargo.toml"}) },
        ];
        let reg = ToolRegistry::builtin();
        let mut thread = crate::agent::Thread::new("test");
        let mut sink = VecSink::default();
        let hooks = HookRegistry::default();
        let results = execute_tool_calls(&calls, &reg, &mut thread, &mut sink, &hooks).await;
        assert_eq!(results.len(), 3);
        let starts: Vec<&str> = sink.0.iter().filter_map(|e| match e {
            UiEvent::ToolCallStarted { name, .. } => Some(name.as_str()),
            _ => None,
        }).collect();
        assert_eq!(starts, vec!["grep", "bash", "read_file"]);
    }

    #[tokio::test]
    async fn all_safe_calls_run_concurrently_and_preserve_order() {
        // 多个 grep（全部 safe）一同派发；结果顺序 = 调用顺序。
        let calls: Vec<ToolCall> = (0..4)
            .map(|i| ToolCall {
                id: format!("g{i}"),
                name: "grep".into(),
                args: json!({"pattern": format!("p{i}"), "path": "."}),
            })
            .collect();
        let reg = ToolRegistry::builtin();
        let mut thread = crate::agent::Thread::new("test");
        let mut sink = VecSink::default();
        let hooks = HookRegistry::default();
        let results = execute_tool_calls(&calls, &reg, &mut thread, &mut sink, &hooks).await;
        assert_eq!(results.len(), 4);
        // ToolCallStarted 事件个数 = 4。
        let started_n = sink.0.iter().filter(|e| matches!(e, UiEvent::ToolCallStarted{..})).count();
        assert_eq!(started_n, 4);
    }

    #[tokio::test]
    async fn pre_tool_use_hook_blocks_tool_execution() {
        use crate::hooks::HookConfig;
        // 注册一个对 bash 阻断的 PreToolUse hook。
        let hooks = HookRegistry::from_configs(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: "echo nope; exit 2".to_string(),
            r#match: None,
            timeout_secs: 5,
            enabled: true,
        }]);
        let calls = vec![ToolCall {
            id: "1".into(),
            name: "bash".into(),
            args: json!({"command":"echo should-not-run"}),
        }];
        let reg = ToolRegistry::builtin();
        let mut thread = crate::agent::Thread::new("test");
        let mut sink = VecSink::default();
        let results = execute_tool_calls(&calls, &reg, &mut thread, &mut sink, &hooks).await;
        assert_eq!(results.len(), 1);
        assert!(!results[0].ok, "hook 应阻断 bash 执行");
        assert!(results[0].content.contains("blocked by hook"));
        // 结果里不应包含真实命令的 stdout。
        assert!(!results[0].content.contains("should-not-run"));
    }

    #[tokio::test]
    async fn pre_tool_use_hook_can_rewrite_args() {
        use crate::hooks::HookConfig;
        // hook 改写 read_file 的 path 为另一个文件。
        // 用 jq 不可用 → 改用 sh + sed 简化：直接 emit 改写后 JSON。
        let hooks = HookRegistry::from_configs(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: r#"echo '{"decision":"rewrite","data":{"args":{"path":"Cargo.toml"}}}'"#.to_string(),
            r#match: None,
            timeout_secs: 5,
            enabled: true,
        }]);
        let calls = vec![ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            args: json!({"path":"/nonexistent/should/fail"}),
        }];
        let reg = ToolRegistry::builtin();
        let mut thread = crate::agent::Thread::new("test");
        let mut sink = VecSink::default();
        let results = execute_tool_calls(&calls, &reg, &mut thread, &mut sink, &hooks).await;
        assert_eq!(results.len(), 1);
        // 改写后读 Cargo.toml 应该成功（项目根有这个文件）。
        assert!(results[0].ok, "hook 改写 path → Cargo.toml 应该读成功");
        assert!(results[0].content.contains("carter") || results[0].content.contains("[package]"));
    }
}
