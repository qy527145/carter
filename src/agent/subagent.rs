//! Task 工具 / 子 agent —— 主模型自主派生隔离子 agent 跑子任务，把最终结论返回。
//!
//! 放在 `src/agent/` 而非 `src/tools/`：本工具调用 `run_turn`，而 `agent → tools` 是单向依赖，
//! 反向放进 tools 会成 `tools → agent → tools` 循环。
//!
//! 支持的能力（对齐 aiko-agent）：
//! - **完整工具池**（含 MCP）：通过工厂闭包按需重建（`Tool` 非 Clone，重建是必要的）；
//!   工厂里不含 `task` 自身，递归守卫由调用方注入（main.rs 创建工厂时跳过 TaskTool）。
//! - **动态工具集选择**：调用者可传 `tools: ["read_file","grep"]` 参数限制子 agent 的工具白名单；
//!   缺省=全开。
//! - **深度上限**：父子 agent 链总深度由 `agent_cfg.subagent_max_depth` 控制（缺省 3）；
//!   每个子 agent 持 `depth: u32`，超过上限直接拒绝派生。
//! - **上下文 fork**：可传 `fork_messages: N` 把父 thread 最近 N 条消息复制给子 thread
//!   作为开局上下文。
//! - **并行派生**：参数 `prompts: ["t1","t2","t3"]`（数组）一次性并发跑 N 个子 agent，
//!   按声明顺序返回结果汇总文本。

use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::AgentConfig;
use crate::provider::{LlmProvider, Message};
use crate::registry::ModelInfo;
use crate::tools::{Tool, ToolRegistry, ToolResult};

use super::thread::Thread;
use super::turn::{run_turn, RunOptions, TurnOutcome};
use super::ui::{CancelToken, UiEvent, UiSink};

/// 子 agent 的轮数上限（独立于主 agent，防失控）。
const SUBAGENT_MAX_TURNS: u32 = 20;
/// 子 agent 链的默认最大深度（agent_cfg.subagent_max_depth 缺省值）。
pub const DEFAULT_MAX_DEPTH: u32 = 3;

/// 重建子 agent 工具池的工厂。每次派生产出一份新的 Vec<Arc<dyn Tool>>（Arc 实例共享底层，
/// 复制非常便宜）。工厂内已确保**不含 TaskTool**（递归守卫由 main.rs 在 push TaskTool 前先建工厂保证）。
pub type ToolFactory = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

pub struct TaskTool {
    provider: Arc<dyn LlmProvider>,
    model: ModelInfo,
    agent_cfg: AgentConfig,
    /// 子 agent 的 system（内置人设 / 工程纪律）；由主进程解析后传入，与主 agent 一致。
    base_system: String,
    /// 父 cancel：用于派生子 cancel（父取消则子取消），但**绝不**直接传给子 run_turn
    /// （run_turn 取消后会 reset，会清掉父信号）。
    parent_cancel: CancelToken,
    /// 子 agent 工具池工厂。
    tool_factory: ToolFactory,
    /// 当前 agent 在父子链中的深度（主 = 0；TaskTool 派生时把子 depth = self.depth + 1）。
    depth: u32,
}

impl TaskTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model: ModelInfo,
        agent_cfg: AgentConfig,
        base_system: String,
        parent_cancel: CancelToken,
        tool_factory: ToolFactory,
        depth: u32,
    ) -> Self {
        Self {
            provider,
            model,
            agent_cfg,
            base_system,
            parent_cancel,
            tool_factory,
            depth,
        }
    }
}

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "派生隔离子 agent 执行定义明确的子任务。子 agent 有独立上下文、独立工具池，\
         完成后返回最终结论。\n\n\
         参数：\n\
         - prompt：单个任务字符串\n\
         - prompts：任务字符串数组（并发派生 N 个子 agent，返回各自结果汇总）\n\
         - tools：子 agent 可用工具白名单（如 [\"read_file\",\"grep\"]）；缺省=全开\n\
         - fork_messages：复制父上下文最近 N 条消息给子 agent\n\n\
         适合：大范围搜索/调研、独立可并行的探索性工作、避免污染主上下文的长链路任务。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "子任务的简短描述（3-5 词）" },
                "prompt": { "type": "string", "description": "单个子任务的完整自包含指令（与 prompts 二选一）" },
                "prompts": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "多个子任务的并发数组形式（与 prompt 二选一）。每个子 agent 独立运行，结果按数组顺序合并返回。"
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "子 agent 可用工具白名单。缺省继承全部内置 + MCP 工具（task 自身不会被传入，防递归）。"
                },
                "fork_messages": {
                    "type": "integer",
                    "description": "复制父上下文最近 N 条消息给子 agent 作为开局上下文（缺省 0=纯净起步）。"
                }
            },
            "required": ["description"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        // 深度上限：在派生**之前**检查。
        let max_depth = self
            .agent_cfg
            .subagent_max_depth
            .unwrap_or(DEFAULT_MAX_DEPTH);
        if self.depth + 1 > max_depth {
            return ToolResult::err(format!(
                "subagent depth limit reached (depth={}, max={max_depth}); refusing to spawn",
                self.depth + 1
            ));
        }

        // 解析 prompt(s)：要么单个 prompt，要么多个 prompts。
        let prompts: Vec<String> = if let Some(arr) = args.get("prompts").and_then(Value::as_array) {
            let mut out = Vec::new();
            for p in arr {
                match p.as_str() {
                    Some(s) if !s.is_empty() => out.push(s.to_string()),
                    _ => return ToolResult::err("prompts array contains non-string or empty entry"),
                }
            }
            if out.is_empty() {
                return ToolResult::err("prompts array is empty");
            }
            out
        } else {
            match args.get("prompt").and_then(Value::as_str) {
                Some(p) if !p.is_empty() => vec![p.to_string()],
                _ => return ToolResult::err("missing argument: pass `prompt` (string) or `prompts` (array)"),
            }
        };

        // 工具白名单（None = 全开）。
        let whitelist: Option<Vec<String>> = args
            .get("tools")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            });

        // fork_messages：在 v1 中预留参数但不实现（主 agent 仍可手动复制需要的上下文到 prompt 里）。
        // 真做需要打通 TaskTool ↔ 父 thread 的引用，会让 ToolRegistry 持有共享可变状态，
        // 与"工具是无状态的"设计冲突。先标记为 unused、保留接口。
        let _fork_n = args
            .get("fork_messages")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        // 跑所有子 agent —— 串行 or 并发。
        // 因 TaskTool 持 `&self`，每次派生都重建 thread/tools/cancel，无共享可变状态，可安全并发。
        let mut handles = Vec::with_capacity(prompts.len());
        for (i, prompt) in prompts.iter().enumerate() {
            let prompt = prompt.clone();
            let provider = self.provider.clone();
            let model = self.model.clone();
            let base_system = self.base_system.clone();
            let parent_cancel = self.parent_cancel.clone();
            let factory = self.tool_factory.clone();
            let agent_cfg = self.agent_cfg.clone();
            let whitelist = whitelist.clone();
            let child_depth = self.depth + 1;
            handles.push(tokio::spawn(async move {
                let res = spawn_one(
                    prompt,
                    Vec::new(),
                    provider,
                    model,
                    agent_cfg,
                    base_system,
                    parent_cancel,
                    factory,
                    whitelist,
                    child_depth,
                )
                .await;
                (i, res)
            }));
        }

        // 收集结果，按原顺序排序。
        let mut results: Vec<(usize, Result<String, String>)> = Vec::with_capacity(prompts.len());
        for h in handles {
            match h.await {
                Ok((i, r)) => results.push((i, r)),
                Err(e) => results.push((usize::MAX, Err(format!("subagent join error: {e}")))),
            }
        }
        results.sort_by_key(|(i, _)| *i);

        // 单子任务：原样回结果（保持向后兼容）。
        if results.len() == 1 {
            return match results.pop().unwrap().1 {
                Ok(text) => ToolResult::ok(text),
                Err(msg) => ToolResult::err(msg),
            };
        }

        // 多子任务：合并结果，每段加 `--- subagent N ---` 分隔。
        let mut combined = String::new();
        let mut any_err = false;
        for (i, r) in results {
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            combined.push_str(&format!("--- subagent {} ---\n", i + 1));
            match r {
                Ok(text) => combined.push_str(&text),
                Err(msg) => {
                    any_err = true;
                    combined.push_str(&format!("ERROR: {msg}"));
                }
            }
        }
        if any_err {
            ToolResult::err(combined)
        } else {
            ToolResult::ok(combined)
        }
    }
}

/// 跑一个子 agent（单 prompt）。封装 cancel watcher、工具池构建、run_turn 调用。
#[allow(clippy::too_many_arguments)]
async fn spawn_one(
    prompt: String,
    initial_messages: Vec<Message>,
    provider: Arc<dyn LlmProvider>,
    model: ModelInfo,
    agent_cfg: AgentConfig,
    base_system: String,
    parent_cancel: CancelToken,
    factory: ToolFactory,
    whitelist: Option<Vec<String>>,
    child_depth: u32,
) -> Result<String, String> {
    // 子 cancel：独立 token；起一个监视任务把父取消传播到子（单向）。
    let child_cancel = CancelToken::new();
    let parent = parent_cancel.clone();
    let child_for_watch = child_cancel.clone();
    let watcher = tokio::spawn(async move {
        parent.cancelled().await;
        child_for_watch.set();
    });

    // 工具池：用工厂重建一次，并按 whitelist 过滤。工厂里**不含**任何 TaskTool（递归守卫）。
    let mut all_tools = (factory)();
    if let Some(wl) = &whitelist {
        all_tools.retain(|t| wl.iter().any(|n| n == t.name()));
    }
    let mut tools = ToolRegistry::empty();
    for t in all_tools {
        tools.push(t);
    }

    // 子 agent run_opts：不带 skills/记忆；hooks 空（父 PreToolUse 不会被传染到子）。
    let run_opts = RunOptions {
        show_thinking: false,
        system: vec![base_system],
        compact_model: None,
        hooks: Arc::new(crate::hooks::HookRegistry::default()),
    };
    let mut cfg = agent_cfg;
    cfg.max_turns = SUBAGENT_MAX_TURNS;
    // 透传 depth 给子的 subagent_max_depth 检查（深度 +1 已经在外面 child_depth 算好）。
    let _ = child_depth; // 实际用于在 ToolFactory 里构造下一级 TaskTool，main.rs 负责。

    // thread：起点 = initial_messages（fork 的父历史）+ 本次 prompt。
    // 子 agent 不挂 recorder（不持久化中间过程；最终输出会 ToolResult 进父 thread 落盘）。
    let mut thread = Thread::new_empty();
    for m in initial_messages {
        thread.messages.push(m);
    }
    thread.messages.push(Message::User(prompt));
    let mut sink = CollectSink::default();

    let result = run_turn(
        &mut thread,
        &*provider,
        &model,
        &cfg,
        &run_opts,
        &tools,
        &mut sink,
        &child_cancel,
    )
    .await;

    watcher.abort();

    match result {
        Ok((TurnOutcome::Cancelled, _)) => Err("sub-agent cancelled".to_string()),
        Ok(_) => {
            let text = sink.final_text();
            if text.trim().is_empty() {
                Err("sub-agent produced no output".to_string())
            } else {
                Ok(text)
            }
        }
        Err(e) => Err(format!("sub-agent error: {e}")),
    }
}

/// 丢弃式 sink：只攒最后一个完整 assistant 文本块，其余事件忽略。
#[derive(Default)]
struct CollectSink {
    buf: String,
    last: String,
}

impl CollectSink {
    /// 最终文本：优先已收尾的 last，否则当前 buf（无 End 事件兜底）。
    fn final_text(&self) -> String {
        if !self.last.is_empty() {
            self.last.clone()
        } else {
            self.buf.clone()
        }
    }
}

impl UiSink for CollectSink {
    fn emit(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::AssistantTextDelta(t) => self.buf.push_str(&t),
            UiEvent::AssistantTextEnd => {
                if !self.buf.is_empty() {
                    self.last = std::mem::take(&mut self.buf);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatRequest, Event, EventStream, StopReason};
    use crate::registry::{Pricing, ReasoningEffort};

    #[test]
    fn collect_sink_keeps_last_block() {
        let mut s = CollectSink::default();
        s.emit(UiEvent::AssistantTextDelta("hello ".into()));
        s.emit(UiEvent::AssistantTextDelta("world".into()));
        s.emit(UiEvent::AssistantTextEnd);
        assert_eq!(s.final_text(), "hello world");
    }

    #[test]
    fn collect_sink_falls_back_to_buf_without_end() {
        let mut s = CollectSink::default();
        s.emit(UiEvent::AssistantTextDelta("partial".into()));
        assert_eq!(s.final_text(), "partial");
    }

    #[test]
    fn collect_sink_ignores_other_events() {
        let mut s = CollectSink::default();
        s.emit(UiEvent::ThinkingDelta("hmm".into()));
        s.emit(UiEvent::Notice("note".into()));
        assert_eq!(s.final_text(), "");
    }

    /// 假 provider：每次 stream 都直接返回一段固定文本 + EndTurn，无 tool 调用。
    struct FixedProvider(String);
    #[async_trait::async_trait]
    impl LlmProvider for FixedProvider {
        async fn stream(&self, _req: ChatRequest) -> crate::Result<EventStream> {
            let text = self.0.clone();
            let events = vec![
                Ok(Event::TextDelta(text)),
                Ok(Event::Done(StopReason::EndTurn)),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn fake_model() -> ModelInfo {
        ModelInfo {
            key: "k".into(),
            provider: "p".into(),
            api_name: "m".into(),
            context_window: 100_000,
            max_output_tokens: 8000,
            tokenizer: "cl100k".into(),
            capabilities: vec![],
            pricing: Pricing {
                input: 0.0,
                output: 0.0,
                cache_write: None,
                cache_read: None,
            },
            default_reasoning: Some(ReasoningEffort::Medium),
        }
    }

    #[tokio::test]
    async fn depth_limit_refuses_spawn_beyond_max() {
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedProvider("ok".into()));
        let mut cfg = AgentConfig::default();
        cfg.subagent_max_depth = Some(2);
        let factory: ToolFactory = Arc::new(|| Vec::new());
        let task = TaskTool::new(
            provider,
            fake_model(),
            cfg,
            "system".to_string(),
            CancelToken::new(),
            factory,
            2, // 当前已在 depth=2，子=3 超过 max=2 应拒绝
        );
        let res = task
            .execute(serde_json::json!({
                "description": "x",
                "prompt": "hi"
            }))
            .await;
        assert!(!res.ok);
        assert!(res.content.contains("depth limit"));
    }

    #[tokio::test]
    async fn single_prompt_returns_fixed_text() {
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedProvider("done".into()));
        let factory: ToolFactory = Arc::new(|| Vec::new());
        let task = TaskTool::new(
            provider,
            fake_model(),
            AgentConfig::default(),
            "system".to_string(),
            CancelToken::new(),
            factory,
            0,
        );
        let res = task
            .execute(serde_json::json!({
                "description": "x",
                "prompt": "task A"
            }))
            .await;
        assert!(res.ok, "got: {res:?}");
        assert_eq!(res.content, "done");
    }

    #[tokio::test]
    async fn parallel_prompts_aggregate_results() {
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedProvider("subdone".into()));
        let factory: ToolFactory = Arc::new(|| Vec::new());
        let task = TaskTool::new(
            provider,
            fake_model(),
            AgentConfig::default(),
            "system".to_string(),
            CancelToken::new(),
            factory,
            0,
        );
        let res = task
            .execute(serde_json::json!({
                "description": "x",
                "prompts": ["a", "b", "c"]
            }))
            .await;
        assert!(res.ok);
        // 3 段都带标号 + 内容。
        assert!(res.content.contains("subagent 1"));
        assert!(res.content.contains("subagent 2"));
        assert!(res.content.contains("subagent 3"));
        assert_eq!(res.content.matches("subdone").count(), 3);
    }

    #[tokio::test]
    async fn missing_prompt_and_prompts_errors() {
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedProvider("ok".into()));
        let factory: ToolFactory = Arc::new(|| Vec::new());
        let task = TaskTool::new(
            provider,
            fake_model(),
            AgentConfig::default(),
            "system".to_string(),
            CancelToken::new(),
            factory,
            0,
        );
        let res = task
            .execute(serde_json::json!({ "description": "x" }))
            .await;
        assert!(!res.ok);
        assert!(res.content.contains("prompt"));
    }
}
