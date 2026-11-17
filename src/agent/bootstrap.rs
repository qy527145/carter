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
use crate::tools::{
    BackgroundShells, BashOutputTool, KillShellTool, PlatformBash, ShellCwd, Tool, ToolRegistry,
};

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
    shells: Option<Arc<BackgroundShells>>,
    cwd: Option<std::path::PathBuf>,
) -> ToolRegistry {
    // 工具开关（`[agent].disabled_tools`）。在装配期就筛掉，而不是执行时回一句
    // 「这个工具被关掉了」—— 后者模型会反复试，因为它的工具表里还看得见它。
    // MCP 工具同样按名筛：宿主能关的名字就该一致地生效。
    let disabled = &agent_cfg.disabled_tools;
    let on = |name: &str| !disabled.iter().any(|d| d == name);

    let cwd = cwd.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });
    // `cd` 跨调用保留：这份 `ShellCwd` 活着，同一个工具实例就能接上一条命令的目录。
    let shell_cwd = ShellCwd::new(cwd);
    let session_id = String::new();

    let mut tools = ToolRegistry::empty();
    for t in ToolRegistry::builtin().tools() {
        if !on(t.name()) {
            continue;
        }
        // `bash` 换成本 crate 的跨平台实现：探测 shell、跟 `cd`、能转后台。
        // carter 里那份 `Bash` 只是占位（`builtin()` 给子 agent 快照与展示用）。
        if t.name() == "bash" {
            tools.push(Arc::new(PlatformBash::new(
                shell_cwd.clone(),
                cancel.clone(),
                shells.clone(),
                session_id.clone(),
            )));
            continue;
        }
        tools.push(t.clone());
    }
    if skills_enabled && on("skill") {
        tools.push(Arc::new(crate::skills::SkillTool::new(
            crate::config::paths::skills_dir(),
        )));
    }
    if let Some(tx) = &ui_tx
        && on("ask_user_question")
    {
        tools.push(Arc::new(crate::agent::AskUserQuestionTool::new(tx.clone())));
    }
    // 后台命令的读取与结束。只有配了后台命令表才挂 —— 没有它的话，`bash` 的
    // `run_in_background` 也无从谈起。
    if let Some(shells) = &shells {
        if on("bash_output") {
            tools.push(Arc::new(BashOutputTool::new(shells.clone())));
        }
        if on("kill_shell") {
            tools.push(Arc::new(KillShellTool::new(shells.clone())));
        }
    }

    // MCP 工具放进主 registry，与内置工具共用一个命名空间。
    for t in &mcp_tools {
        if on(t.name()) {
            tools.push(t.clone());
        }
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
