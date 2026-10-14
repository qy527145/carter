//! 启动期装配：把工具池拼起来。
//!
//! 放在库里而不是命令外壳里，是因为它被三个入口共用：命令行（oneshot / TUI）、
//! NDJSON host、以及嵌入方（GUI）。此前 CLI 与 NDJSON 各有一份几乎逐字相同的实现，
//! 差别只在 `ask_user_question` 是否注册 —— 那种重复最容易在两处悄悄长歪，
//! 而工具池长歪是「某个模式下模型少了个工具」这类很难查的问题。

use std::sync::Arc;

use crate::agent::{CancelToken, TaskTool, ToolFactory, UiEvent};
use crate::config::AgentConfig;
use crate::provider::LlmProvider;
use crate::registry::ModelInfo;
use crate::tools::{Tool, ToolRegistry};

/// 组装工具注册表：内置工具 + （启用时）Skills 的 `skill` 工具 + 子 agent 的 `task` 工具
/// + MCP server 透明并入的工具。
///
/// `ask_user_question` 只在给了 `ui_tx` 时注册。没有 UI 等着作答时把它挂上去，
/// 只会让模型调出一个永不返回的工具；不给它描述，模型也就不会调 —— 这比
/// 「能调到但卡死」好得多。
///
/// `task` 工具持有「工厂闭包」用于子 agent 派生时重建工具池：
/// - 工厂里**不含** `task`（递归守卫由这里保证）
/// - 工厂里包含所有 builtin + skill + MCP 工具（子 agent 也能用 MCP）
pub fn build_tools(
    skills_enabled: bool,
    provider: Arc<dyn LlmProvider>,
    model: ModelInfo,
    agent_cfg: AgentConfig,
    base_system: String,
    cancel: CancelToken,
    mcp_tools: Vec<Arc<dyn Tool>>,
    ui_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
) -> ToolRegistry {
    let mut tools = ToolRegistry::builtin();
    if skills_enabled {
        tools.push(Arc::new(crate::skills::SkillTool::new(
            crate::config::paths::skills_dir(),
        )));
    }
    if let Some(tx) = &ui_tx {
        tools.push(Arc::new(crate::agent::AskUserQuestionTool::new(tx.clone())));
    }
    // MCP 工具放进主 registry，与内置工具共用一个命名空间。
    for t in &mcp_tools {
        tools.push(t.clone());
    }

    // 子 agent 工厂：产出「主 registry 中的所有工具（去掉 task）」。
    // `tools.tools()` 此刻还不含 TaskTool（下面才 push），所以这个快照天然不含它 ——
    // 递归守卫就落在这个顺序上。
    let snapshot: Vec<Arc<dyn Tool>> = tools.tools().to_vec();
    let factory: ToolFactory = Arc::new(move || snapshot.clone());

    tools.push(Arc::new(TaskTool::new(
        provider,
        model,
        agent_cfg,
        base_system,
        cancel,
        factory,
        0, // 主 agent depth = 0
    )));
    tools
}
