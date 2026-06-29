//! Hook 类型定义。用户配置 + 事件枚举 + 执行结果。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Hook 触发的生命周期事件。在 `config.toml` 中以 kebab-case 配置：
/// `event = "pre-tool-use"` / `"user-prompt-submit"` 等。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreTurn,
    PostTurn,
    PreToolUse,
    PostToolUse,
    PreCompact,
    SubagentStop,
    Notification,
    Stop,
}

impl HookEvent {
    /// 该事件是否允许阻断（pre_* 类）。仅这些事件认 exit_code=2。
    pub fn is_blockable(&self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse | HookEvent::PreCompact | HookEvent::UserPromptSubmit
        )
    }

    /// 该事件是否允许改写 payload（pre_* 类 + user_prompt_submit）。
    pub fn is_rewritable(&self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse | HookEvent::UserPromptSubmit
        )
    }
}

/// 单条 hook 配置（来自 `[[hooks]]` TOML 段）。
#[derive(Debug, Clone, Deserialize)]
pub struct HookConfig {
    /// 触发事件（kebab-case）。
    pub event: HookEvent,
    /// shell 命令（在 `sh -c "<command>"` 下执行；Windows 走 `cmd /C`）。
    pub command: String,
    /// 可选的 matcher：仅当 payload 满足条件才触发。
    /// 当前支持 `tool` 字段（pre_tool_use / post_tool_use 时按工具名过滤）。
    #[serde(default)]
    pub r#match: Option<HookMatcher>,
    /// 超时秒数，缺省 30。
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 启用开关，默认 true。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// hook 触发条件过滤器。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HookMatcher {
    /// 工具名精确匹配（pre_tool_use / post_tool_use）。
    pub tool: Option<String>,
    /// 工具名 glob 模式，如 `*_file`（仅在 tool 字段缺省时启用）。
    pub tool_glob: Option<String>,
}

fn default_timeout() -> u64 {
    30
}
fn default_true() -> bool {
    true
}

/// 事件载荷：传给 hook 的 JSON。具体字段按事件类型差异。
/// 序列化形态是 `{ "event": "pre-tool-use", "data": { ... } }`，
/// `data` 子对象由各事件构造方决定。
#[derive(Debug, Clone, Serialize)]
pub struct HookPayload {
    pub event: HookEvent,
    pub data: Value,
}

impl HookPayload {
    pub fn new(event: HookEvent, data: Value) -> Self {
        Self { event, data }
    }
}

/// hook 执行后的决策。调用方据此决定继续 / 阻断 / 用改写后的 payload 接力。
#[derive(Debug, Clone)]
pub enum HookDecision {
    /// 通过，无改动。
    Continue,
    /// 通过，且 payload 被 hook 改写为新值（仅可改写事件生效）。
    Rewrite(Value),
    /// 阻断（仅可阻断事件生效）。`reason` 用于回灌给模型/用户。
    Block { reason: String },
}

/// 单次 hook 调用的最终结果（含执行细节，便于调试 / 日志）。
#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub decision: HookDecision,
    /// hook 命令的原始输出（截断后），仅供 tracing 用。
    #[allow(dead_code)] // 当前 tracing 直接打到 warn；保留字段便于未来透传到 UI。
    pub stdout_preview: String,
    #[allow(dead_code)]
    pub stderr_preview: String,
    #[allow(dead_code)]
    pub exit_code: i32,
}
