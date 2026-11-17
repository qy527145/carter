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
//! - **并行派生**：`tasks: [{description, prompt}]` 一次性并发跑最多 8 个子 agent，
//!   按声明顺序返回结果汇总文本。旧的 `prompt` / `prompts` 形态仍然认。
//! - **进度事件**：每个子 agent 的「当前工具 / 步数 / 最近输出」写进共享槽位，节流地
//!   发整表快照（`UiEvent::AgentProgress`）。没有它，界面上就是一个跑二十分钟的转圈卡片。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::AgentConfig;
use crate::provider::{LlmProvider, WireMessage};
use crate::registry::ModelInfo;
use crate::tools::{Tool, ToolRegistry, ToolResult};

use super::thread::Thread;
use super::turn::{RunOptions, TurnOutcome, run_turn};
use super::ui::{AgentRun, CancelToken, UiEvent, UiSink};

/// 子 agent 的轮数上限。`u32::MAX` = 不设限：与主对话一样，什么时候停下由它自己
/// 决定（20 在「翻一批文件逐个核对」的子任务里很容易撞上，撞上就半路收尾、结果不全）。
const SUBAGENT_MAX_TURNS: u32 = u32::MAX;
/// 进度事件的最小间隔。文本增量一个 token 一条，全转发会把通道打满。
const PROGRESS_GAP: Duration = Duration::from_millis(120);
/// 卡片里显示的「最近输出」长度。
const PROGRESS_TAIL: usize = 600;
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
    /// 进度事件的出口。`None` = 宿主不接（oneshot / 测试），不发事件。
    progress: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    /// 没有 `call_id` 时现编编号用。
    seq: Arc<AtomicU64>,
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
            progress: None,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 接上宿主的事件通道：子 agent 的进度会以 `UiEvent::AgentProgress` 发出去。
    pub fn with_progress(mut self, tx: tokio::sync::mpsc::UnboundedSender<UiEvent>) -> Self {
        self.progress = Some(tx);
        self
    }
}

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "派生多个隔离子 agent 并行完成任务。每个子 agent 有独立的上下文与完整的工具池，\
         自己读文件、跑命令，最后只把结论交回来 —— 主对话不必装下它们翻过的东西。\n\n\
         什么时候用：\n\
         - 好几个互不依赖的调研 / 搜索 / 改造可以同时做（一次 tasks 里全给它们）；\n\
         - 某件事要翻很多文件、试很多次，过程对主对话没用，只要结论。\n\
         什么时候别用：几件事有先后依赖（那就一件一件做，或者自己直接做）。\n\n\
         每个子任务的 prompt 要**自包含**：它看不到主对话，不知道你们刚聊过什么。\
         把需要的路径、约定、输出格式写清楚。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": {
                                "type": "string",
                                "description": "这一行的短标题（几字，显示在界面上）"
                            },
                            "prompt": {
                                "type": "string",
                                "description": "完整、自包含的指令：要看哪些文件、做什么、最后输出什么"
                            }
                        },
                        "required": ["description", "prompt"]
                    },
                    "description": "要并行跑的子任务。只有一个时也用它。"
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "子 agent 可用的工具白名单（如 [\"read_file\",\"grep\"]）。缺省 = 全部。"
                }
            },
            "required": ["tasks"]
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

        let tasks = match parse_tasks(&args) {
            Ok(t) => t,
            Err(e) => return e,
        };
        // 白名单按名字过滤；空数组视为不限制（模型偶尔会传 [] 表示「默认」）。
        let whitelist: Option<Vec<String>> = args
            .get("tools")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            });
        let tools: Vec<Arc<dyn Tool>> = match &whitelist {
            Some(wl) => (self.tool_factory)()
                .into_iter()
                .filter(|t| wl.iter().any(|n| n == t.name()))
                .collect(),
            None => (self.tool_factory)(),
        };
        // 白名单是模型自己写的，写错名字时静默跑一个没工具的子 agent 最难查。
        // 主工具池本身为空（紧凑模式之类）不算错 —— 纯推理的子任务照样能跑。
        if whitelist.is_some() && tools.is_empty() {
            return ToolResult::err("tools 白名单里的工具一个都不存在，子 agent 无事可做");
        }

        // 界面那张卡片按这个编号配对；不在 `run_turn` 派发里时现编一个。
        let call_id = crate::agent::current_call_id()
            .unwrap_or_else(|| format!("task{}", self.seq.fetch_add(1, Ordering::Relaxed)));

        let runs: Arc<Mutex<Vec<AgentRun>>> = Arc::new(Mutex::new(
            tasks
                .iter()
                .map(|t| AgentRun {
                    description: t.description.clone(),
                    status: "running".into(),
                    steps: 0,
                    tool: None,
                    text: String::new(),
                })
                .collect(),
        ));
        let publish = |runs: &Arc<Mutex<Vec<AgentRun>>>| {
            if let Some(tx) = &self.progress {
                let agents = runs.lock().map(|v| v.clone()).unwrap_or_default();
                let _ = tx.send(UiEvent::AgentProgress {
                    call_id: call_id.clone(),
                    agents,
                });
            }
        };
        publish(&runs);

        // 子 agent 的请求挂在父会话下（各自再带一个 agent id 单独成线）。
        // 必须在 spawn 之前取：任务局部变量不会跟进新任务。
        let session_id = crate::agent::current_session_id();
        let count = tasks.len();
        // `self` 的生命周期不进 spawn：要用的几样先拷出来。
        let parent_cancel = self.parent_cancel.clone();
        let child_depth = self.depth + 1;
        let base_system = self.base_system.clone();
        let progress = self.progress.clone();
        let mut handles = Vec::with_capacity(count);
        for (i, task) in tasks.into_iter().enumerate() {
            let provider = self.provider.clone();
            let model = self.model.clone();
            let mut cfg = self.agent_cfg.clone();
            cfg.max_turns = SUBAGENT_MAX_TURNS;
            let mut system = base_system.clone();
            // 子 agent 看不到主对话，得先知道自己是谁、在哪 —— 补一句，也免得它
            // 以为「刚才说的那几个文件」还在上下文里。
            system.push_str(
                "\n\n## 当前身份\n\n\
                 你是一个被派出来的子 agent。你看不到主对话的历史，只有下面这一段任务。\
                 做完就停，最后一条消息要能直接当作结论交回去（写清做了什么、看到了什么、\
                 还差什么），不要问问题 —— 没有人能回答你。",
            );
            let tools = tools.clone();
            let runs = runs.clone();
            let progress = progress.clone();
            let parent_cancel = parent_cancel.clone();
            let call_id = call_id.clone();
            let session_id = session_id.clone();
            handles.push(tokio::spawn(async move {
                let mut sink = ProgressSink {
                    runs: runs.clone(),
                    index: i,
                    progress,
                    call_id,
                    steps: 0,
                    buf: String::new(),
                    last: String::new(),
                    last_emit: Instant::now() - PROGRESS_GAP,
                };
                let out = spawn_one(
                    task.prompt.clone(),
                    session_id,
                    Vec::new(),
                    provider,
                    model,
                    cfg,
                    system,
                    parent_cancel.clone(),
                    Arc::new(move || tools.clone()),
                    None,
                    child_depth,
                    &mut sink,
                )
                .await;
                // 收尾状态写回表里 —— 失败与中断的也要能看出来，而不是一直转圈。
                let (status, text) = match &out {
                    Ok(t) => ("done", ProgressSink::tail_static(t)),
                    Err(e) => (
                        if e.contains("cancelled") || e.contains("中断") {
                            "cancelled"
                        } else {
                            "failed"
                        },
                        ProgressSink::tail_static(e),
                    ),
                };
                sink.set(|run| {
                    run.status = status.into();
                    run.tool = None;
                    if !text.is_empty() {
                        run.text = text;
                    }
                });
                (i, out)
            }));
        }

        let mut results: Vec<(usize, Result<String, String>)> = Vec::with_capacity(count);
        for h in handles {
            match h.await {
                Ok(v) => results.push(v),
                Err(e) => results.push((usize::MAX, Err(format!("subagent join error: {e}")))),
            }
        }
        results.sort_by_key(|(i, _)| *i);
        publish(&runs);

        // 单个子任务：原样回结果（保持旧契约 —— 多套一层标题只是噪音）。
        if results.len() == 1 {
            return match results.pop().expect("len == 1").1 {
                Ok(text) => ToolResult::ok(text),
                Err(msg) => ToolResult::err(msg),
            };
        }

        let mut combined = String::new();
        let mut any_err = false;
        for (i, r) in results {
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            let label = runs
                .lock()
                .ok()
                .and_then(|v| v.get(i).map(|a| a.description.clone()))
                .unwrap_or_else(|| format!("子任务 {}", i + 1));
            combined.push_str(&format!("--- subagent {}：{label} ---\n", i + 1));
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

/// 一个子任务的入口参数。
#[derive(Debug)]
struct Task {
    description: String,
    prompt: String,
}

fn parse_tasks(args: &Value) -> Result<Vec<Task>, ToolResult> {
    // 新形态：tasks: [{description, prompt}]
    if let Some(arr) = args.get("tasks").and_then(Value::as_array) {
        if arr.is_empty() {
            return Err(ToolResult::err("tasks 是空的，至少要一个子任务"));
        }
        if arr.len() > 8 {
            return Err(ToolResult::err(format!(
                "一次最多派生 8 个子 agent（收到 {} 个）。拆成几轮，或者把相近的合成一个",
                arr.len()
            )));
        }
        let mut out = Vec::with_capacity(arr.len());
        for (i, t) in arr.iter().enumerate() {
            let prompt = t.get("prompt").and_then(Value::as_str).unwrap_or("");
            if prompt.trim().is_empty() {
                return Err(ToolResult::err(format!("第 {} 个子任务没有 prompt", i + 1)));
            }
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("子任务 {}", i + 1));
            out.push(Task {
                description,
                prompt: prompt.to_string(),
            });
        }
        return Ok(out);
    }

    // 旧形态：prompt / prompts（兼容已经这么写的模型与调用方）。
    let prompts: Vec<String> = if let Some(arr) = args.get("prompts").and_then(Value::as_array) {
        let mut out = Vec::new();
        for p in arr {
            match p.as_str() {
                Some(s) if !s.is_empty() => out.push(s.to_string()),
                _ => return Err(ToolResult::err("prompts 里有非字符串或空字符串")),
            }
        }
        if out.is_empty() {
            return Err(ToolResult::err("prompts 是空的"));
        }
        out
    } else {
        match args.get("prompt").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => vec![p.to_string()],
            _ => {
                return Err(ToolResult::err(
                    "缺少 tasks：传 `tasks: [{ \"description\": \"…\", \"prompt\": \"…\" }]`（一个也行）",
                ));
            }
        }
    };
    let fallback = args
        .get("description")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(prompts
        .into_iter()
        .enumerate()
        .map(|(i, prompt)| Task {
            description: match (&fallback, i) {
                (Some(d), 0) => d.clone(),
                _ => format!("子任务 {}", i + 1),
            },
            prompt,
        })
        .collect())
}

/// 把子 agent 的动态写进共享槽位，并节流地推事件。
struct ProgressSink {
    runs: Arc<Mutex<Vec<AgentRun>>>,
    index: usize,
    progress: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    call_id: String,
    steps: u32,
    buf: String,
    last: String,
    last_emit: Instant,
}

impl ProgressSink {
    /// 推一次整表快照。
    fn publish(&self) {
        if let Some(tx) = &self.progress {
            let agents = self.runs.lock().map(|v| v.clone()).unwrap_or_default();
            let _ = tx.send(UiEvent::AgentProgress {
                call_id: self.call_id.clone(),
                agents,
            });
        }
    }

    /// 改一行，立刻推（工具切换、收尾这类关键节点不值得省）。
    fn set(&mut self, f: impl FnOnce(&mut AgentRun)) {
        if let Ok(mut v) = self.runs.lock()
            && let Some(run) = v.get_mut(self.index)
        {
            f(run);
        }
        self.publish();
    }

    /// 改一行，但最多每 [`PROGRESS_GAP`] 推一次（文本增量走这里）。
    fn maybe(&mut self, f: impl FnOnce(&mut AgentRun)) {
        if let Ok(mut v) = self.runs.lock()
            && let Some(run) = v.get_mut(self.index)
        {
            f(run);
        }
        if self.last_emit.elapsed() >= PROGRESS_GAP {
            self.last_emit = Instant::now();
            self.publish();
        }
    }

    /// 最终文本：优先已收尾的 last，否则当前 buf（无 End 事件兜底）。
    fn final_text(&self) -> String {
        if !self.last.is_empty() {
            self.last.clone()
        } else {
            self.buf.clone()
        }
    }

    fn tail_static(text: &str) -> String {
        if text.len() <= PROGRESS_TAIL {
            return text.to_string();
        }
        let mut start = text.len() - PROGRESS_TAIL;
        while start < text.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        format!("…{}", &text[start..])
    }
}

impl UiSink for ProgressSink {
    fn emit(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::AssistantTextDelta(t) => {
                self.buf.push_str(&t);
                let tail = Self::tail_static(&self.buf);
                self.maybe(|run| {
                    run.text = tail;
                });
            }
            UiEvent::AssistantTextEnd if !self.buf.is_empty() => {
                self.last = std::mem::take(&mut self.buf);
                let tail = Self::tail_static(&self.last);
                self.set(|run| run.text = tail);
            }
            UiEvent::ToolCallStarted { name, .. } => {
                self.steps += 1;
                let steps = self.steps;
                self.set(|run| {
                    run.steps = steps;
                    run.tool = Some(name);
                });
            }
            UiEvent::ToolResult { .. } => {
                self.set(|run| run.tool = None);
            }
            UiEvent::Notice(n) => {
                let tail = Self::tail_static(&n);
                self.set(|run| run.text = tail);
            }
            _ => {}
        }
    }
}

/// 跑一个子 agent（单 prompt）。封装 cancel watcher、工具池构建、run_turn 调用。
#[allow(clippy::too_many_arguments)]
async fn spawn_one(
    prompt: String,
    session_id: Option<String>,
    initial_messages: Vec<WireMessage>,
    provider: Arc<dyn LlmProvider>,
    model: ModelInfo,
    agent_cfg: AgentConfig,
    base_system: String,
    parent_cancel: CancelToken,
    factory: ToolFactory,
    whitelist: Option<Vec<String>>,
    child_depth: u32,
    sink: &mut ProgressSink,
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
        purpose: crate::provider::RequestPurpose::Subagent,
        steer: None,
    };
    let mut cfg = agent_cfg;
    cfg.max_turns = SUBAGENT_MAX_TURNS;
    // 透传 depth 给子的 subagent_max_depth 检查（深度 +1 已经在外面 child_depth 算好）。
    let _ = child_depth; // 实际用于在 ToolFactory 里构造下一级 TaskTool，main.rs 负责。

    // thread：起点 = initial_messages（fork 的父历史）+ 本次 prompt。
    // 子 agent 不挂 recorder（不持久化中间过程；最终输出会 ToolResult 进父 thread 落盘）。
    let mut thread = Thread::new_empty();
    thread.session_id = session_id;
    // 每个子 agent 一个新 id：并行派出的几个 system 提示完全一样，只能靠它区分。
    thread.set_agent_id(uuid::Uuid::new_v4().to_string());
    for m in initial_messages {
        thread.messages.push(m);
    }
    thread.messages.push(WireMessage::user_text(prompt));

    let result = run_turn(
        &mut thread,
        &*provider,
        &model,
        &cfg,
        &run_opts,
        &tools,
        sink,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatRequest, EventStream, FinishReason, StreamChunk};
    use crate::registry::{Pricing, ReasoningEffort};

    /// 假 provider：每次 stream 都直接返回一段固定文本 + EndTurn，无 tool 调用。
    struct FixedProvider(String);
    #[async_trait::async_trait]
    impl LlmProvider for FixedProvider {
        async fn stream(&self, _req: ChatRequest) -> crate::Result<EventStream> {
            let text = self.0.clone();
            let events = vec![
                Ok(StreamChunk::TextDelta { index: 0, text }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                }),
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

    /// 记下每次请求的归属信息，回一段固定文本。
    #[derive(Default)]
    struct MetaRecorder(std::sync::Mutex<Vec<crate::provider::RequestMeta>>);
    #[async_trait::async_trait]
    impl LlmProvider for MetaRecorder {
        async fn stream(&self, req: ChatRequest) -> crate::Result<EventStream> {
            self.0.lock().unwrap().push(req.wire.meta);
            FixedProvider("ok".into()).stream(req_placeholder()).await
        }
    }

    fn req_placeholder() -> ChatRequest {
        ChatRequest {
            protocol: None,
            wire: crate::provider::WireRequest::new("m"),
        }
    }

    #[tokio::test]
    async fn parallel_subagents_share_session_but_not_agent_id() {
        let rec = Arc::new(MetaRecorder::default());
        let factory: ToolFactory = Arc::new(|| Vec::new());
        let task = TaskTool::new(
            rec.clone(),
            fake_model(),
            AgentConfig::default(),
            "system".to_string(),
            CancelToken::new(),
            factory,
            0,
        );
        let res = super::super::turn::SESSION_ID
            .scope(
                Some("parent".to_string()),
                task.execute(serde_json::json!({ "prompts": ["a", "b"] })),
            )
            .await;
        assert!(res.ok, "{}", res.content);

        let metas = rec.0.lock().unwrap().clone();
        assert_eq!(metas.len(), 2);
        for m in &metas {
            assert_eq!(m.session_id.as_deref(), Some("parent"));
            assert_eq!(m.purpose, crate::provider::RequestPurpose::Subagent);
            assert!(m.agent_id.is_some());
        }
        assert_ne!(metas[0].agent_id, metas[1].agent_id);
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
