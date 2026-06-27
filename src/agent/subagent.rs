//! Task 工具 / 子 agent —— 主模型自主派生隔离子 agent 跑子任务，只回结果文本。
//!
//! 放在 `src/agent/` 而非 `src/tools/`：本工具调用 `run_turn`，而 `agent → tools` 是单向依赖，
//! 反向放进 tools 会成 `tools → agent → tools` 循环（见 plan R1）。
//! 递归守卫（R3）：子 agent 的工具集 = `ToolRegistry::builtin()`，**永不含 `task`**——
//! `task` 只由 main.rs push 到主 registry，故子 agent 无法再派生子 agent。

use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::AgentConfig;
use crate::provider::LlmProvider;
use crate::registry::ModelInfo;
use crate::tools::{Tool, ToolRegistry, ToolResult};

use super::thread::Thread;
use super::turn::{run_turn, RunOptions, TurnOutcome};
use super::ui::{CancelToken, UiEvent, UiSink};

/// 子 agent 的轮数上限（独立于主 agent，防失控）。
const SUBAGENT_MAX_TURNS: u32 = 20;

pub struct TaskTool {
    provider: Arc<dyn LlmProvider>,
    model: ModelInfo,
    agent_cfg: AgentConfig,
    /// 父 cancel：用于派生子 cancel（父取消则子取消），但**绝不**直接传给子 run_turn
    /// （run_turn 取消后会 reset，会清掉父信号）。
    parent_cancel: CancelToken,
}

impl TaskTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model: ModelInfo,
        agent_cfg: AgentConfig,
        parent_cancel: CancelToken,
    ) -> Self {
        Self {
            provider,
            model,
            agent_cfg,
            parent_cancel,
        }
    }
}

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "派生一个隔离子 agent 执行一个定义明确的子任务。子 agent 有独立上下文与全套内置工具，\
         完成后只把最终结论文本返回给你。适合：大范围搜索/调研、可并行的独立工作，避免污染主上下文。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "子任务的简短描述（3-5 词）" },
                "prompt": { "type": "string", "description": "交给子 agent 的完整自包含任务指令" },
                "subagent_type": { "type": "string", "description": "子 agent 类型（可选，当前仅作标注）" }
            },
            "required": ["description", "prompt"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let prompt = match args.get("prompt").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return ToolResult::err("missing or non-string argument: prompt"),
        };

        // 子 cancel：独立 token；起一个监视任务把父取消传播到子（单向，R2）。
        let child_cancel = CancelToken::new();
        let parent = self.parent_cancel.clone();
        let child_for_watch = child_cancel.clone();
        let watcher = tokio::spawn(async move {
            parent.cancelled().await;
            child_for_watch.set();
        });

        // 子 agent 工具集 = builtin（不含 task，递归守卫 R3）。
        let tools = ToolRegistry::builtin();
        let run_opts = RunOptions {
            show_thinking: false,
            system_prompt: None,
            compact_model: None,
        };
        let mut cfg = self.agent_cfg.clone();
        cfg.max_turns = SUBAGENT_MAX_TURNS;

        let mut thread = Thread::new(prompt);
        let mut sink = CollectSink::default();

        let result = run_turn(
            &mut thread,
            &*self.provider,
            &self.model,
            &cfg,
            &run_opts,
            &tools,
            &mut sink,
            &child_cancel,
        )
        .await;

        watcher.abort();

        match result {
            Ok((TurnOutcome::Cancelled, _)) => ToolResult::err("sub-agent cancelled"),
            Ok((_, _)) => {
                let text = sink.final_text();
                if text.trim().is_empty() {
                    ToolResult::err("sub-agent produced no output")
                } else {
                    ToolResult::ok(text)
                }
            }
            Err(e) => ToolResult::err(format!("sub-agent error: {e}")),
        }
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
}
