//! Hook 注册表：按事件分桶持有所有配置；分发时按事件取桶、依次执行。
//!
//! 多个 hook 命中同一事件时**串行**执行，前一个的 Rewrite 决策会作为后一个的输入 payload
//! （chain），任一返回 Block 则立即停止链并向上传播。

use std::collections::HashMap;

use serde_json::Value;

use super::runner::run_hook;
use super::types::{HookConfig, HookDecision, HookEvent, HookPayload};

/// hook 注册表：按事件分桶。
#[derive(Debug, Default)]
pub struct HookRegistry {
    by_event: HashMap<HookEvent, Vec<HookConfig>>,
}

impl HookRegistry {
    /// 从一组扁平 hook 配置（来自 `[[hooks]]` TOML）建注册表。
    pub fn from_configs(hooks: Vec<HookConfig>) -> Self {
        let mut by_event: HashMap<HookEvent, Vec<HookConfig>> = HashMap::new();
        for h in hooks {
            by_event.entry(h.event).or_default().push(h);
        }
        Self { by_event }
    }

    /// 该事件是否注册了任何 hook（轻量探测，避免 dispatch 时不必要的 JSON 序列化）。
    pub fn has(&self, event: HookEvent) -> bool {
        self.by_event
            .get(&event)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// 串行触发该事件的所有 hook，返回最终决策（链式）。
    /// 任一 hook 返回 `Block` → 立刻终止链，传 Block 向上。
    /// `Rewrite` → 用改写后的 data 接力下一个 hook。
    /// `Continue` → 沿用当前 data 接力。
    pub async fn dispatch(&self, event: HookEvent, initial_data: Value) -> HookDecision {
        let Some(hooks) = self.by_event.get(&event) else {
            return HookDecision::Continue;
        };
        if hooks.is_empty() {
            return HookDecision::Continue;
        }

        let mut current = HookPayload::new(event, initial_data);
        let mut rewritten = false;
        for cfg in hooks {
            let out = run_hook(cfg, &current).await;
            match out.decision {
                HookDecision::Continue => continue,
                HookDecision::Rewrite(new_data) => {
                    current = HookPayload::new(event, new_data);
                    rewritten = true;
                }
                HookDecision::Block { reason } => return HookDecision::Block { reason },
            }
        }

        if rewritten {
            HookDecision::Rewrite(current.data)
        } else {
            HookDecision::Continue
        }
    }

    /// 观察型 dispatch：不读返回值、不阻断，仅触发副作用 hook（PostToolUse / PreTurn /
    /// PostTurn / SessionStart / SessionEnd / SubagentStop / Stop / Notification 等）。
    /// 内部仍是 fire-and-block（同一 task 内 await 完），但 Rewrite / Block 都被忽略。
    /// 用于不希望被 hook 改变控制流的事件。
    pub async fn emit(&self, event: HookEvent, data: Value) {
        if !self.has(event) {
            return;
        }
        let _ = self.dispatch(event, data).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make(event: HookEvent, command: &str) -> HookConfig {
        HookConfig {
            event,
            command: command.to_string(),
            r#match: None,
            timeout_secs: 5,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn empty_registry_continues() {
        let r = HookRegistry::default();
        let d = r.dispatch(HookEvent::PreToolUse, json!({})).await;
        assert!(matches!(d, HookDecision::Continue));
    }

    #[tokio::test]
    async fn block_short_circuits_chain() {
        // 第一个 block，第二个永远不会跑（无法可靠观测，但至少 Block 被传出来）。
        let r = HookRegistry::from_configs(vec![
            make(HookEvent::PreToolUse, "exit 2"),
            make(HookEvent::PreToolUse, "echo never; exit 0"),
        ]);
        let d = r.dispatch(HookEvent::PreToolUse, json!({})).await;
        assert!(matches!(d, HookDecision::Block { .. }));
    }

    #[tokio::test]
    async fn rewrite_chains_through() {
        let r = HookRegistry::from_configs(vec![
            make(
                HookEvent::UserPromptSubmit,
                r#"echo '{"prompt":"step1"}'"#,
            ),
            make(
                HookEvent::UserPromptSubmit,
                r#"echo '{"prompt":"step2"}'"#,
            ),
        ]);
        let d = r.dispatch(HookEvent::UserPromptSubmit, json!({"prompt":"orig"})).await;
        match d {
            HookDecision::Rewrite(v) => {
                assert_eq!(v.get("prompt").and_then(|x| x.as_str()), Some("step2"));
            }
            other => panic!("{other:?}"),
        }
    }
}
