mod agent;
mod config;
mod cost;
mod error;
mod mcp;
mod provider;
mod registry;
mod skills;
mod tools;
mod tui;

pub use error::{CarterError, Result};

use clap::{Parser, Subcommand};

use std::sync::Arc;

use crate::agent::{run_turn, CancelToken, RunOptions, StdoutSink, TaskTool, Thread, TurnOutcome};
use crate::config::Config;
use crate::provider::genai_provider::GenaiProvider;
use crate::provider::LlmProvider;
use crate::registry::fetch;
use crate::tools::ToolRegistry;
use crate::tui::ChannelSink;

#[derive(Parser, Debug)]
#[command(name = "carter", about = "Carter — Rust agent CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 从 models.dev 拉取模型元数据，缓存到 ~/.carter/models.json。
    Update,
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// 一次性提示词（提供则走一次性 stdout 模式；缺省进交互式 TUI REPL）。
    prompt: Option<String>,

    /// 模型引用 `provider/model`（缺省取配置 agent.model）。
    #[arg(long)]
    model: Option<String>,

    /// 关闭思考内容输出。
    #[arg(long)]
    no_thinking: bool,

    /// 强制一次性 stdout 模式（即使无 prompt 也不进 TUI）。
    #[arg(long)]
    no_tui: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    init_tracing();

    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// tracing 写日志文件 `~/.carter/carter.log`（append、无 ANSI），避免 warn 喷到终端
/// 污染 inline TUI viewport / 退出后残留。文件打不开则回落 stderr（oneshot 仍可见）。
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    let log_path = crate::config::paths::log_path();
    if let Some(dir) = log_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(file)
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }
}

async fn run() -> Result<std::process::ExitCode> {
    let cli = Cli::parse();

    // 子命令分发。
    if let Some(Command::Update) = cli.command {
        return run_update().await;
    }

    let args = cli.run;
    let config = Config::load()?;

    // 模型元数据：读 ~/.carter/models.json 缓存（缺失给友好提示）。
    let cache_json = fetch::read_cache()?;

    let model_name = args
        .model
        .clone()
        .unwrap_or_else(|| config.agent.model.clone());
    let model = crate::registry::resolve_model(&config, &cache_json, &model_name)?;

    // 按 model.provider 查 [providers.*]；命中则配置驱动接入，否则回落 genai 默认。
    let provider: Arc<dyn LlmProvider> = match config.providers.get(&model.provider) {
        Some(pcfg) => Arc::new(GenaiProvider::from_provider_config(pcfg)?),
        None => Arc::new(GenaiProvider::new()),
    };

    // Fast 模型（压缩 / 标题）：配了 agent.fast_model 则解析，否则回落主模型/主 provider。
    let (fast_model, fast_provider): (crate::registry::ModelInfo, Arc<dyn LlmProvider>) =
        match &config.agent.fast_model {
            Some(reference) => {
                let fm = crate::registry::resolve_model(&config, &cache_json, reference)?;
                let fp: Arc<dyn LlmProvider> = match config.providers.get(&fm.provider) {
                    Some(pcfg) => Arc::new(GenaiProvider::from_provider_config(pcfg)?),
                    None => Arc::new(GenaiProvider::new()),
                };
                (fm, fp)
            }
            None => (model.clone(), provider.clone()),
        };

    let show_thinking = config.reasoning.show_thinking && !args.no_thinking;

    // Skills 目录：发现后注入 system prompt；正文由 `skill` 工具按需加载。
    let skills_dir = crate::config::paths::skills_dir();
    let skill_metas = if config.skills.enabled {
        skills::discover(&skills_dir)
    } else {
        Vec::new()
    };
    let system_prompt = if skill_metas.is_empty() {
        None
    } else {
        Some(skills::render_catalog(&skill_metas))
    };

    // 模式分发：有 prompt 或 --no-tui → 一次性 stdout；否则交互式 TUI REPL。
    if args.prompt.is_some() || args.no_tui {
        let prompt = args.prompt.clone().unwrap_or_default();
        run_oneshot(
            prompt,
            provider,
            model,
            fast_provider,
            fast_model,
            config,
            show_thinking,
            system_prompt,
        )
        .await
    } else {
        run_tui(
            provider,
            model,
            fast_provider,
            fast_model,
            config,
            show_thinking,
            model_name,
            system_prompt,
        )
        .await
    }
}

/// `carter update`：拉取 models.dev → 缓存 → 打印计数。
async fn run_update() -> Result<std::process::ExitCode> {
    let json = fetch::fetch_models_dev().await?;
    let (providers, models) = count_models(&json);
    fetch::write_cache(&json)?;
    let path = crate::config::paths::models_cache_path();
    println!(
        "已更新 {providers} providers / {models} models 到 {}",
        path.display()
    );
    Ok(std::process::ExitCode::SUCCESS)
}

/// 统计缓存 JSON 的 provider / model 数量（仅供打印）。
fn count_models(json: &str) -> (usize, usize) {
    let root: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return (0, 0),
    };
    let obj = match root.as_object() {
        Some(o) => o,
        None => return (0, 0),
    };
    let providers = obj.len();
    let models = obj
        .values()
        .filter_map(|p| p.get("models").and_then(|m| m.as_object()))
        .map(|m| m.len())
        .sum();
    (providers, models)
}

/// 组装工具注册表：内置工具 + （启用时）Skills 的 `skill` 工具 + 子 agent 的 `task` 工具
/// + MCP server 透明并入的工具。
/// `task` 只在主 registry 注入（子 agent 用 `builtin()`，递归守卫 R3）。
fn build_tools(
    skills_enabled: bool,
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    agent_cfg: crate::config::AgentConfig,
    cancel: CancelToken,
    mcp_tools: Vec<Box<dyn crate::tools::Tool>>,
) -> ToolRegistry {
    let mut tools = ToolRegistry::builtin();
    if skills_enabled {
        tools.push(Box::new(crate::skills::SkillTool::new(
            crate::config::paths::skills_dir(),
        )));
    }
    tools.push(Box::new(TaskTool::new(provider, model, agent_cfg, cancel)));
    for t in mcp_tools {
        tools.push(t);
    }
    tools
}

/// 一次性 stdout 模式（向后兼容 M1–M3）。
async fn run_oneshot(
    prompt: String,
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    fast_provider: Arc<dyn LlmProvider>,
    fast_model: crate::registry::ModelInfo,
    config: Config,
    show_thinking: bool,
    system_prompt: Option<String>,
) -> Result<std::process::ExitCode> {
    let mut thread = Thread::new(prompt);
    let run_opts = RunOptions {
        show_thinking,
        system_prompt,
        compact_model: Some(crate::agent::CompactModel {
            provider: fast_provider,
            model: fast_model,
        }),
    };
    let cancel = CancelToken::new();
    let (mcp_mgr, mcp_tools) = crate::mcp::McpManager::start(&config.mcp).await;
    let tools = build_tools(
        config.skills.enabled,
        provider.clone(),
        model.clone(),
        config.agent.clone(),
        cancel.clone(),
        mcp_tools,
    );
    let mut sink = StdoutSink::new();

    let run_res = run_turn(
        &mut thread,
        &*provider,
        &model,
        &config.agent,
        &run_opts,
        &tools,
        &mut sink,
        &cancel,
    )
    .await;

    // 回收 MCP 子进程（无论成功失败都先 shutdown，再传播错误）。
    mcp_mgr.shutdown().await;
    let (outcome, _usage) = run_res?;

    Ok(outcome_to_code(outcome))
}

/// 交互式 TUI REPL 模式。agent loop 跑在独立任务里，经通道与 TUI 通信。
async fn run_tui(
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    fast_provider: Arc<dyn LlmProvider>,
    fast_model: crate::registry::ModelInfo,
    config: Config,
    show_thinking: bool,
    model_label: String,
    system_prompt: Option<String>,
) -> Result<std::process::ExitCode> {
    use tokio::sync::mpsc::unbounded_channel;

    let (ui_tx, ui_rx) = unbounded_channel::<crate::agent::UiEvent>();
    let (input_tx, mut input_rx) = unbounded_channel::<String>();
    let cancel = CancelToken::new();

    // MCP：会话级 manager 留在外层 scope（活过整个 agent 任务）；工具（持克隆 peer）移入任务。
    let (mcp_mgr, mcp_tools) = crate::mcp::McpManager::start(&config.mcp).await;

    // agent 任务：连续多轮，每轮一个 prompt → run_turn → emit 事件。
    let agent_cancel = cancel.clone();
    let agent_task = tokio::spawn(async move {
        let mut thread = Thread::new_empty();
        let run_opts = RunOptions {
            show_thinking,
            system_prompt,
            compact_model: Some(crate::agent::CompactModel {
                provider: fast_provider.clone(),
                model: fast_model.clone(),
            }),
        };
        let tools = build_tools(
            config.skills.enabled,
            provider.clone(),
            model.clone(),
            config.agent.clone(),
            agent_cancel.clone(),
            mcp_tools,
        );
        let mut sink = ChannelSink::new(ui_tx);

        // 整个会话只在首条 prompt 后生成一次标题。
        let mut first = true;
        while let Some(prompt) = input_rx.recv().await {
            thread.append_user(prompt.clone());
            if first {
                first = false;
                // fast 模型短输出；失败静默忽略（错误即数据，不阻断会话）。
                if let Ok(title) =
                    crate::agent::generate_title(&prompt, &*fast_provider, &fast_model).await
                {
                    if !title.is_empty() {
                        use crate::agent::UiSink;
                        sink.emit(crate::agent::UiEvent::Title(title));
                    }
                }
            }
            let _ = run_turn(
                &mut thread,
                &*provider,
                &model,
                &config.agent,
                &run_opts,
                &tools,
                &mut sink,
                &agent_cancel,
            )
            .await;
            // 错误即数据：run_turn 内部已 emit；这里忽略 Result 让 REPL 继续。
        }
    });

    // 跑 TUI 主循环（阻塞直到用户退出）。
    let tui_res = tui::run(model_label, ui_rx, input_tx, cancel.clone()).await;

    // TUI 退出 → input_tx 已 drop → agent 任务 while 循环结束。
    // 但若 agent 正卡在一个请求里，则先 set cancel 让它即时断开，再 abort 兜底，
    // 避免优雅退出时 await 永久阻塞。
    cancel.set();
    agent_task.abort();
    let _ = agent_task.await;

    // agent 任务结束（peer 克隆随 tools 一并 drop）后，回收 MCP 子进程。
    mcp_mgr.shutdown().await;

    tui_res.map_err(crate::error::CarterError::Io)?;
    Ok(std::process::ExitCode::SUCCESS)
}

fn outcome_to_code(outcome: TurnOutcome) -> std::process::ExitCode {
    match outcome {
        TurnOutcome::Assistant | TurnOutcome::Limit => std::process::ExitCode::SUCCESS,
        TurnOutcome::Cancelled => std::process::ExitCode::from(130),
        TurnOutcome::Error(msg) => {
            eprintln!("turn error: {msg}");
            std::process::ExitCode::FAILURE
        }
    }
}
