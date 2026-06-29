//! Hook 系统 —— 用户可在 `config.toml` 配置 shell 命令钩子，让 carter 在 11 个生命周期事件
//! 处自动执行外部脚本，可读到事件上下文（JSON via stdin）、可改写参数（JSON via stdout）、
//! 可阻断（exit_code≠0 + 特定退出码语义）。
//!
//! ## 11 个事件
//!
//! | 事件 | 何时触发 | payload |
//! |---|---|---|
//! | `session_start` | 新会话开启或 resume | `{ session_id, cwd, model }` |
//! | `session_end` | 会话退出 | `{ session_id, total_in, total_out }` |
//! | `user_prompt_submit` | user 输入即将进 thread | `{ prompt }` ← 可改写 |
//! | `pre_turn` | run_turn 进入前 | `{ turn, message_count }` |
//! | `post_turn` | run_turn 完成 | `{ turn, outcome, usage }` |
//! | `pre_tool_use` | 单个工具调用前 | `{ tool, args }` ← 可改写、可阻断 |
//! | `post_tool_use` | 单个工具调用后 | `{ tool, args, ok, content }` |
//! | `pre_compact` | 上下文压缩前 | `{ tier, message_count }` ← 可阻断 |
//! | `subagent_stop` | task 子 agent 结束 | `{ description, ok, output }` |
//! | `notification` | UI 通知（如成本告警） | `{ message }` |
//! | `stop` | 整个 agent loop 自然停止 | `{ reason }` |
//!
//! ## Hook 协议（shell command 类型）
//!
//! - 子进程在 cwd 下执行；event payload 经 **stdin 以 JSON** 喂入
//! - 退出码：
//!   - `0` = 通过，stdout 若是 valid JSON 且符合 schema 则**用作改写后的 payload**
//!   - `2` = **阻断**（pre_* 事件生效，把 ToolResult/turn 改成结构化拒绝）
//!   - 其它非零 = 错误，记 warn，按"通过"处理（不让坏 hook 杀会话）
//! - stderr 始终透传到 `tracing::warn`，便于调试

mod registry;
mod runner;
mod types;

pub use registry::HookRegistry;
#[allow(unused_imports)] // 单测里用
pub use runner::run_hook;
pub use types::{HookConfig, HookDecision, HookEvent};

/// 触发 `UserPromptSubmit` hook：
/// - 没有 hook 注册 → 原样返回 `Some(text)`，零开销
/// - hook 返回 Rewrite → 用改写后的 prompt
/// - hook 返回 Block → 返回 `None`，调用方应丢弃本轮
///
/// 抽到这里供 TUI / oneshot / NDJSON 三种入口共用（避免重复实现）。
pub async fn run_user_prompt_submit(
    hooks: &std::sync::Arc<HookRegistry>,
    text: String,
) -> Option<String> {
    if !hooks.has(HookEvent::UserPromptSubmit) {
        return Some(text);
    }
    let payload = serde_json::json!({ "prompt": text });
    match hooks.dispatch(HookEvent::UserPromptSubmit, payload).await {
        HookDecision::Continue => Some(text),
        HookDecision::Rewrite(v) => v
            .get("prompt")
            .and_then(|p| p.as_str())
            .map(str::to_string)
            .or(Some(text)),
        HookDecision::Block { reason } => {
            tracing::info!("hooks: user prompt blocked: {reason}");
            None
        }
    }
}
