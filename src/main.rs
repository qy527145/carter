mod agent;
mod commands;
mod config;
mod cost;
mod error;
mod hooks;
mod mcp;
mod media;
mod memory;
mod prompt;
mod provider;
mod registry;
mod session;
mod skills;
mod tokens;
mod tools;
mod tui;
mod wizard;

pub use error::{CarterError, Result};

use clap::{CommandFactory, Parser, Subcommand};

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
    /// 列出当前目录的会话（`--all` 跨所有项目）。
    Sessions {
        #[arg(long)]
        all: bool,
    },
    /// 输出指定 shell 的补全脚本（bash/zsh/fish/powershell/elvish）。
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// 压实会话文件（丢弃被压缩覆盖的旧工具输出）；`--all` 跨所有项目。
    Gc {
        #[arg(long)]
        all: bool,
    },
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// 一次性提示词（提供则走一次性 stdout 模式；缺省进交互式 TUI REPL）。
    prompt: Option<String>,

    /// 模型引用 `provider/model`（缺省取配置 agent.model）。
    #[arg(long)]
    model: Option<String>,

    /// 自定义系统提示词文件（覆盖内置「特工卡特」人设 + 配置里的 agent.system_prompt_file）。
    #[arg(long, value_name = "PATH")]
    system_prompt_file: Option<String>,

    /// 本次运行不注入多层记忆（CARTER.md / AGENTS.md）。
    #[arg(long)]
    no_memory: bool,

    /// 关闭思考内容输出。
    #[arg(long)]
    no_thinking: bool,

    /// 强制一次性 stdout 模式（即使无 prompt 也不进 TUI）。
    #[arg(long)]
    no_tui: bool,

    /// 续接当前目录最近一条会话。
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// 加载指定会话续接（UUID 或 `carter sessions` 列表序号）。
    #[arg(short = 'r', long = "resume", value_name = "ID")]
    resume: Option<String>,

    /// 从指定会话派生一个新会话（UUID 或序号）。
    #[arg(long, value_name = "ID")]
    fork: Option<String>,

    /// 给新会话指定 id（首次运行用）。
    #[arg(long = "session-id", value_name = "ID")]
    session_id: Option<String>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
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
/// LLM 请求日志是独立的（见 provider::llm_log），不走这里。
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

/// 启动时把 `[env]` 配置项设进进程环境（如 http_proxy）。须在建任何 HTTP 客户端前调用。
fn apply_env(env: &std::collections::HashMap<String, String>) {
    for (k, v) in env {
        // SAFETY: 启动早期、尚未 spawn 任务 / 建 HTTP 客户端，无并发读 env 者。
        unsafe {
            std::env::set_var(k, v);
        }
    }
}

async fn run() -> Result<std::process::ExitCode> {
    let cli = Cli::parse();

    // 尽早加载配置：先设环境变量（代理等，须早于任何 HTTP 客户端 / models.dev 抓取），
    // 再据此初始化 tracing（含可选的请求调试日志）。
    let mut config = Config::load()?;
    apply_env(&config.env);
    init_tracing();

    // 子命令分发。
    match &cli.command {
        Some(Command::Update) => return run_update().await,
        Some(Command::Sessions { all }) => return run_sessions(*all),
        Some(Command::Gc { all }) => return run_gc(*all),
        Some(Command::Completion { shell }) => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(*shell, &mut cmd, name, &mut std::io::stdout());
            return Ok(std::process::ExitCode::SUCCESS);
        }
        None => {}
    }

    let args = cli.run;

    // 首次运行向导：无 config.toml 且处于交互式终端时，引导生成一份并重载。
    if wizard::ensure_config().await? {
        config = Config::load()?;
        apply_env(&config.env);
    }

    // CLI `--system-prompt-file` 覆盖配置里的 agent.system_prompt_file（优先级最高）。
    if let Some(path) = &args.system_prompt_file {
        config.agent.system_prompt_file = Some(path.clone());
    }

    // 模型元数据：读 ~/.carter/models.json 缓存（缺失给友好提示）。
    let cache_json = fetch::read_cache()?;

    let model_name = args
        .model
        .clone()
        .unwrap_or_else(|| config.agent.model.clone());
    let (model, provider) = resolve_provider(&config, &cache_json, &model_name)?;

    // Fast 模型（压缩 / 标题）：配了 agent.fast_model 则解析，否则回落主模型/主 provider。
    let (fast_model, fast_provider): (crate::registry::ModelInfo, Arc<dyn LlmProvider>) =
        match &config.agent.fast_model {
            Some(reference) => resolve_provider(&config, &cache_json, reference)?,
            None => (model.clone(), provider.clone()),
        };

    let show_thinking = config.reasoning.show_thinking && !args.no_thinking;

    // Skills 目录：发现后注入 system；正文由 `skill` 工具按需加载。
    let skills_dir = crate::config::paths::skills_dir();
    let skill_metas = if config.skills.enabled {
        skills::discover(&skills_dir)
    } else {
        Vec::new()
    };

    // 会话解析：resume/fork/continue/新建。在模式分发前完成，oneshot 与 TUI 共用。
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (thread, session_meta) = resolve_session(&cwd, &model_name, &args)?;

    // system 分段数组（按序：人设 + skills + 多层记忆 + 运行环境）。供 oneshot 与 TUI 共用。
    let system = assemble_system(
        &config,
        &cwd,
        &model.key,
        session_meta.git.as_ref(),
        &skill_metas,
        !args.no_memory,
    );

    // 模式分发：有 prompt 或 --no-tui → 一次性 stdout；否则交互式 TUI REPL。
    if args.prompt.is_some() || args.no_tui {
        let prompt = args.prompt.clone().unwrap_or_default();
        run_oneshot(
            thread,
            prompt,
            provider,
            model,
            fast_provider,
            fast_model,
            config,
            show_thinking,
            system,
        )
        .await
    } else {
        run_tui(
            thread,
            session_meta,
            provider,
            model,
            fast_provider,
            fast_model,
            config,
            cache_json,
            show_thinking,
            model_name,
            system,
            skill_metas,
        )
        .await
    }
}

/// 组装 system 分段数组：人设（内置或自定义文件）→ skills 目录 → 多层记忆 → 运行环境。
/// 每段非空才入数组；底座据此发往 wire 的 system 数组。`memory_enabled=false` 跳过记忆段。
fn assemble_system(
    config: &Config,
    cwd: &std::path::Path,
    model_label: &str,
    git: Option<&session::GitInfo>,
    skill_metas: &[crate::skills::SkillMeta],
    memory_enabled: bool,
) -> Vec<String> {
    let mut system: Vec<String> = Vec::new();
    system.push(crate::prompt::base(config));
    if !skill_metas.is_empty() {
        system.push(crate::skills::render_catalog(skill_metas));
    }
    if memory_enabled {
        let memory = crate::memory::load(cwd);
        if !memory.is_empty() {
            system.push(memory);
        }
    }
    system.push(crate::prompt::project_info(
        cwd,
        model_label,
        git,
        crate::session::now_ms(),
    ));
    system
}

/// 解析模型引用 → (ModelInfo, provider)。按 model.provider 查 `[providers.*]`，
/// 命中则配置驱动接入，否则回落 genai 默认。startup 与会话内 `/model` 共用。
fn resolve_provider(
    config: &Config,
    cache_json: &str,
    model_ref: &str,
) -> Result<(crate::registry::ModelInfo, Arc<dyn LlmProvider>)> {
    let model = crate::registry::resolve_model(config, cache_json, model_ref)?;
    // LLM 请求日志目录：config 覆盖 or 默认 ~/.carter/debug/llm_log。
    let log_dir = config
        .debug
        .llm_log_dir
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::config::paths::llm_log_dir);
    let debug = config.debug.log_requests;
    let provider: Arc<dyn LlmProvider> = match config.providers.get(&model.provider) {
        Some(pcfg) => Arc::new(GenaiProvider::from_provider_config(pcfg, debug, log_dir)?),
        None => Arc::new(GenaiProvider::new(debug, log_dir)),
    };
    Ok((model, provider))
}

/// 按 CLI 标志解析会话：resume / fork / continue / 新建。
fn resolve_session(
    cwd: &std::path::Path,
    model_name: &str,
    args: &RunArgs,
) -> Result<(Thread, session::SessionMeta)> {
    if let Some(sel) = &args.resume {
        let entry = find_session(cwd, sel)?;
        return session::load(&entry);
    }
    if let Some(sel) = &args.fork {
        let entry = find_session(cwd, sel)?;
        return session::fork(&entry);
    }
    if args.continue_session {
        if let Some(entry) = session::latest(cwd) {
            return session::load(&entry);
        }
        // 当前目录无历史 → 起新会话。
    }
    let opts = session::SessionOpts {
        session_id: args.session_id.clone(),
    };
    session::start_new(cwd, model_name, &opts)
}

/// 选择器：按 UUID 精确匹配 → `carter sessions` 的 1 基序号 → id 前缀（支持显示的短 id）。
fn find_session(cwd: &std::path::Path, sel: &str) -> Result<session::SessionEntry> {
    let entries = session::list(cwd, false);
    if let Some(e) = entries.iter().find(|e| e.meta.id == *sel) {
        return Ok(e.clone());
    }
    if let Ok(idx) = sel.parse::<usize>() {
        if idx >= 1 && idx <= entries.len() {
            return Ok(entries[idx - 1].clone());
        }
    }
    // id 前缀（如状态栏/列表显示的 8 位短 id）。
    let prefixed: Vec<&session::SessionEntry> =
        entries.iter().filter(|e| e.meta.id.starts_with(sel)).collect();
    match prefixed.len() {
        1 => Ok(prefixed[0].clone()),
        0 => Err(crate::error::CarterError::Config(format!(
            "未找到会话：{sel}（用 `carter sessions` 查看可用会话）"
        ))),
        n => Err(crate::error::CarterError::Config(format!(
            "会话 id 前缀 `{sel}` 不唯一（匹配 {n} 个），请输入更长前缀"
        ))),
    }
}

/// `carter sessions`：列出会话（序号 / 标题 / 时间 / 模型 / id）。
fn run_sessions(all: bool) -> Result<std::process::ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let entries = session::list(&cwd, all);
    if entries.is_empty() {
        println!("（无会话）");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let now = session::now_ms();
    for (i, e) in entries.iter().enumerate() {
        let title = e.meta.title.as_deref().unwrap_or("（无标题）");
        let age = human_age(now.saturating_sub(e.meta.created_at));
        println!(
            "{:>3}. {title}  ·  {age}前  ·  {}  ·  {}",
            i + 1,
            e.meta.model,
            e.meta.id,
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// `carter gc`：压实会话文件。
fn run_gc(all: bool) -> Result<std::process::ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let entries = session::list(&cwd, all);
    if entries.is_empty() {
        println!("（无会话）");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let (mut total_old, mut total_new) = (0u64, 0u64);
    for e in &entries {
        let label = e.meta.title.clone().unwrap_or_else(|| e.meta.id.clone());
        match session::gc(e) {
            Ok((old, new)) => {
                total_old += old;
                total_new += new;
                println!("  {label}: {} → {}", human_size(old), human_size(new));
            }
            Err(err) => println!("  {label}: gc 失败 — {err}"),
        }
    }
    println!(
        "合计 {} → {}（省 {}）",
        human_size(total_old),
        human_size(total_new),
        human_size(total_old.saturating_sub(total_new)),
    );
    Ok(std::process::ExitCode::SUCCESS)
}

/// 字节数 → 紧凑文本。
fn human_size(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1}MB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.1}KB", bytes as f64 / (1u64 << 10) as f64)
    } else {
        format!("{bytes}B")
    }
}

/// 把毫秒差渲染成粗粒度年龄文本。
fn human_age(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}秒")
    } else if s < 3600 {
        format!("{}分", s / 60)
    } else if s < 86400 {
        format!("{}时", s / 3600)
    } else {
        format!("{}天", s / 86400)
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
///
/// `task` 工具持有"工厂闭包"用于子 agent 派生时重建工具池：
/// - 工厂里**不含** `task`（递归守卫由这里保证）
/// - 工厂里包含所有 builtin + skill + MCP 工具（子 agent 也能用 MCP）
fn build_tools(
    skills_enabled: bool,
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    agent_cfg: crate::config::AgentConfig,
    base_system: String,
    cancel: CancelToken,
    mcp_tools: Vec<Arc<dyn crate::tools::Tool>>,
    ui_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::agent::UiEvent>>,
) -> ToolRegistry {
    let mut tools = ToolRegistry::builtin();
    if skills_enabled {
        tools.push(Arc::new(crate::skills::SkillTool::new(
            crate::config::paths::skills_dir(),
        )));
    }
    // ask_user_question 工具：有 ui_tx 才注册（TUI 模式）。
    // oneshot 模式下没有 UI 等用户输入，工具不可用 —— 模型见不到该工具描述也就不会调。
    if let Some(tx) = &ui_tx {
        tools.push(Arc::new(crate::agent::AskUserQuestionTool::new(tx.clone())));
    }
    // MCP 工具放进主 registry。
    for t in &mcp_tools {
        tools.push(t.clone());
    }

    // 子 agent 工厂：能产出"主 registry 中的所有工具（去掉 task）"。
    // tools.tools() 此刻还不含 TaskTool（下面才 push），所以工厂就是当前快照 + 永不含 task。
    let snapshot: Vec<Arc<dyn crate::tools::Tool>> = tools.tools().to_vec();
    let factory: crate::agent::ToolFactory = Arc::new(move || snapshot.clone());

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

/// 一次性 stdout 模式（向后兼容 M1–M3）。
async fn run_oneshot(
    mut thread: Thread,
    prompt: String,
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    fast_provider: Arc<dyn LlmProvider>,
    fast_model: crate::registry::ModelInfo,
    config: Config,
    show_thinking: bool,
    system: Vec<String>,
) -> Result<std::process::ExitCode> {
    // 续接时 thread 已含历史；追加本次 prompt（并落盘）。
    // `@图片.png` 等内联图片附件先转成 `[img:...]` token，便于 provider 边界组多模态。
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let hooks = std::sync::Arc::new(crate::hooks::HookRegistry::from_configs(config.hooks.clone()));
    let prompt = crate::media::inline_user_attachments(&prompt, &cwd);
    // UserPromptSubmit hook：可改写 prompt，可阻断。
    let prompt = run_user_prompt_submit_hook(&hooks, prompt).await;
    let Some(prompt) = prompt else {
        // 被 hook 阻断。
        return Ok(std::process::ExitCode::SUCCESS);
    };
    thread.append_user(prompt);
    // 子 agent 复用人设段（system[0]）作为其 base system。
    let base_system = system.first().cloned().unwrap_or_default();
    let run_opts = RunOptions {
        show_thinking,
        system,
        compact_model: Some(crate::agent::CompactModel {
            provider: fast_provider,
            model: fast_model,
        }),
        hooks: hooks.clone(),
    };
    let cancel = CancelToken::new();
    let (mcp_mgr, mcp_tools) = crate::mcp::McpManager::start(&config.mcp).await;
    let tools = build_tools(
        config.skills.enabled,
        provider.clone(),
        model.clone(),
        config.agent.clone(),
        base_system,
        cancel.clone(),
        mcp_tools,
        None, // oneshot 模式无 TUI，ask_user_question 工具不可用
    );
    let mut sink = StdoutSink::new();

    // SessionStart hook：oneshot 也算一次 session。
    hooks
        .emit(
            crate::hooks::HookEvent::SessionStart,
            serde_json::json!({
                "mode": "oneshot",
                "cwd": cwd.to_string_lossy(),
                "model": model.key,
            }),
        )
        .await;

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
    let (outcome, usage) = run_res?;

    // SessionEnd hook：返回最终用量。
    hooks
        .emit(
            crate::hooks::HookEvent::SessionEnd,
            serde_json::json!({
                "mode": "oneshot",
                "input_tokens": usage.input,
                "output_tokens": usage.output,
            }),
        )
        .await;

    Ok(outcome_to_code(outcome))
}

/// 交互式 TUI REPL 模式。agent loop 跑在独立任务里，经通道与 TUI 通信。
async fn run_tui(
    thread: Thread,
    session_meta: session::SessionMeta,
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    fast_provider: Arc<dyn LlmProvider>,
    fast_model: crate::registry::ModelInfo,
    config: Config,
    cache_json: String,
    show_thinking: bool,
    model_label: String,
    system: Vec<String>,
    skill_metas: Vec<crate::skills::SkillMeta>,
) -> Result<std::process::ExitCode> {
    use tokio::sync::mpsc::unbounded_channel;

    let (ui_tx, ui_rx) = unbounded_channel::<crate::agent::UiEvent>();
    let (input_tx, mut input_rx) = unbounded_channel::<String>();
    let cancel = CancelToken::new();

    // MCP：会话级 manager 留在外层 scope（活过整个 agent 任务）；工具（持克隆 peer）移入任务。
    let (mcp_mgr, mcp_tools) = crate::mcp::McpManager::start(&config.mcp).await;
    // /mcp 用：移入 build_tools 前先记下已加载的 MCP 工具数。
    let mcp_tool_count = mcp_tools.len();

    // 生命周期 hook 注册表（按事件分桶）。
    let hooks = std::sync::Arc::new(crate::hooks::HookRegistry::from_configs(config.hooks.clone()));

    // 自定义斜杠命令：按 cwd 发现一次，移入 agent 任务。
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let custom_cmds = crate::commands::discover(&cwd);

    // 补全候选 = 内置命令 + 自定义命令（传给 TUI 的 `/` 弹窗）。
    let mut completion_items: Vec<crate::tui::CompletionItem> = crate::commands::BUILTINS
        .iter()
        .map(|b| crate::tui::CompletionItem {
            name: b.name.to_string(),
            description: b.description.to_string(),
            hint: None,
        })
        .collect();
    for c in &custom_cmds {
        completion_items.push(crate::tui::CompletionItem {
            name: c.name.clone(),
            description: c.description.clone().unwrap_or_else(|| "自定义命令".into()),
            hint: c.argument_hint.clone(),
        });
    }

    // agent 任务按 move 捕获 cwd；TUI 也要用它做 `@` 文件补全，故先克隆一份。
    let tui_cwd = cwd.clone();

    // 参数补全候选：/model → 各 provider 的模型引用（静态，config 不变）。
    let mut model_items: Vec<crate::tui::CompletionItem> = Vec::new();
    {
        let mut provs: Vec<&String> = config.providers.keys().collect();
        provs.sort();
        for p in provs {
            let kind = config.providers[p].kind.clone();
            let mut ms: Vec<&String> = config.providers[p].models.keys().collect();
            ms.sort();
            for m in ms {
                model_items.push(crate::tui::CompletionItem {
                    name: format!("{p}/{m}"),
                    description: format!("[{kind}]"),
                    hint: None,
                });
            }
        }
    }
    let arg_completions: Vec<(String, Vec<crate::tui::CompletionItem>)> =
        vec![("model".to_string(), model_items)];

    // /resume·/fork → 会话候选动态来源：每次打开弹窗重新列会话（新建即时可见）。
    let sess_cwd = cwd.clone();
    let session_candidates: Box<dyn Fn() -> Vec<crate::tui::CompletionItem> + Send> =
        Box::new(move || {
            session::list(&sess_cwd, false)
                .iter()
                .map(|e| crate::tui::CompletionItem {
                    name: e.meta.id.chars().take(8).collect(),
                    description: e.meta.title.clone().unwrap_or_else(|| "（无标题）".into()),
                    hint: None,
                })
                .collect()
        });

    // 跨会话输入历史：启动时载入（上下方向键召回）。
    let input_history = session::history::load();

    // SessionStart hook：TUI 起来即触发（resume 也算一次新 session 入口）。
    hooks
        .emit(
            crate::hooks::HookEvent::SessionStart,
            serde_json::json!({
                "mode": "tui",
                "session_id": session_meta.id,
                "cwd": cwd.to_string_lossy(),
                "model": model_label,
                "resumed": session_meta.title.is_some(),
            }),
        )
        .await;

    // agent 任务：连续多轮，每轮一个 prompt → run_turn → emit 事件。
    let agent_cancel = cancel.clone();
    let agent_hooks = hooks.clone();
    let agent_task = tokio::spawn(async move {
        let mut thread = thread;
        // 子 agent 复用人设段（system[0]）作为其 base system。
        let base_system = system.first().cloned().unwrap_or_default();
        let run_opts = RunOptions {
            show_thinking,
            system,
            compact_model: Some(crate::agent::CompactModel {
                provider: fast_provider.clone(),
                model: fast_model.clone(),
            }),
            hooks: agent_hooks.clone(),
        };
        let tools = build_tools(
            config.skills.enabled,
            provider.clone(),
            model.clone(),
            config.agent.clone(),
            base_system,
            agent_cancel.clone(),
            mcp_tools,
            Some(ui_tx.clone()), // TUI 模式：ask_user_question 工具走这个通道发 UiEvent::AskUser
        );
        // 主模型 / provider 设为可变：会话内 `/model` 热切换会重绑它们（run_turn 每轮取最新）。
        // 注：子 agent 的 `task` 工具仍持旧 model（重建 MCP 工具代价高，列为已知限制）。
        let mut model = model;
        let mut provider = provider;
        let mut sink = ChannelSink::new(ui_tx);

        // 续接：回显标题 + 分隔线 + 回放历史上下文到视图（让用户看见已恢复的对话）。
        use crate::agent::UiSink;
        // 状态栏立即显示当前模型（不等首轮 usage 回来再同步）。
        sink.emit(crate::agent::UiEvent::ModelChanged(model.key.clone()));
        if let Some(t) = &session_meta.title {
            sink.emit(crate::agent::UiEvent::Title(t.clone()));
        }
        if !thread.messages.is_empty() {
            sink.emit(crate::agent::UiEvent::Divider(format!(
                "恢复会话 {}",
                session_label(&session_meta)
            )));
            sink.emit(crate::agent::UiEvent::ReplayHistory(
                crate::agent::replay_from_messages(&thread.messages),
            ));
            sink.emit(crate::agent::UiEvent::Divider("以上为历史上下文".into()));
        }

        // 仅当会话还没有标题时，于首条 prompt 后生成一次并落盘。
        let mut needs_title = session_meta.title.is_none();
        // 本会话累计用量（供 /cost）。
        let mut session_usage = crate::provider::Usage::default();
        while let Some(prompt) = input_rx.recv().await {
            // 记录到跨会话输入历史（原始输入，含斜杠命令）。
            session::history::append(&session_meta.id, &prompt);
            // 斜杠命令解析：自定义命令展开为 prompt；内置命令就地分发（无需跑 turn）。
            // `/quit`·`/exit` 已由 TUI 层在提交前拦截。
            let to_run: Option<String> = {
                let trimmed = prompt.trim_start();
                if let Some(rest) = trimmed.strip_prefix('/') {
                    let (name, cmd_args) =
                        rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                    if let Some(c) = custom_cmds.iter().find(|c| c.name == name) {
                        Some(crate::commands::expand(c, cmd_args, &cwd))
                    } else if crate::commands::is_builtin(name) {
                        dispatch_builtin(
                            name,
                            cmd_args,
                            &mut thread,
                            &mut sink,
                            &cwd,
                            &mut model,
                            &mut provider,
                            &fast_provider,
                            &fast_model,
                            &config,
                            &cache_json,
                            &custom_cmds,
                            &skill_metas,
                            mcp_tool_count,
                            &session_usage,
                            &mut needs_title,
                        )
                        .await;
                        None
                    } else {
                        sink.emit(crate::agent::UiEvent::Notice(format!(
                            "未知命令 /{name}，按普通输入处理"
                        )));
                        Some(prompt.clone())
                    }
                } else {
                    Some(prompt.clone())
                }
            };

            if let Some(text) = to_run {
                // `@图片.png` 等内联附件先转成 `[img:...]` token；非图片 `@路径` 不变（保留语义）。
                let text = crate::media::inline_user_attachments(&text, &cwd);
                // UserPromptSubmit hook：可改写 / 可阻断。阻断时本轮 prompt 丢弃。
                let text = match run_user_prompt_submit_hook(&agent_hooks, text).await {
                    Some(t) => t,
                    None => {
                        sink.emit(crate::agent::UiEvent::Notice(
                            "[hook] prompt blocked by user_prompt_submit hook".into(),
                        ));
                        sink.emit(crate::agent::UiEvent::Idle);
                        continue;
                    }
                };
                thread.append_user(text);
                if needs_title {
                    needs_title = false;
                    // fast 模型短输出；失败静默忽略（错误即数据，不阻断会话）。
                    if let Ok(title) =
                        crate::agent::generate_title(&prompt, &*fast_provider, &fast_model).await
                    {
                        if !title.is_empty() {
                            if let Some(rec) = thread.recorder() {
                                rec.record(session::RecordKind::Title {
                                    title: title.clone(),
                                });
                            }
                            sink.emit(crate::agent::UiEvent::Title(title));
                        }
                    }
                }
                if let Ok((_, usage)) = run_turn(
                    &mut thread,
                    &*provider,
                    &model,
                    &config.agent,
                    &run_opts,
                    &tools,
                    &mut sink,
                    &agent_cancel,
                )
                .await
                {
                    session_usage.add(&usage);
                }
                // 错误即数据：run_turn 内部已 emit；这里忽略 Err 让 REPL 继续。
            }

            // 无论本次是跑了一轮、执行了斜杠命令、还是被取消 / 出错，
            // 都发一个 Idle 让 TUI 退出 streaming 态、恢复可交互（修复卡死）。
            sink.emit(crate::agent::UiEvent::Idle);
        }
    });

    // 跑 TUI 主循环（阻塞直到用户退出）。
    let tui_res = tui::run(
        model_label,
        completion_items,
        arg_completions,
        session_candidates,
        input_history,
        tui_cwd,
        ui_rx,
        input_tx,
        cancel.clone(),
    )
    .await;

    // TUI 退出 → input_tx 已 drop → agent 任务 while 循环结束。
    // 但若 agent 正卡在一个请求里，则先 set cancel 让它即时断开，再 abort 兜底，
    // 避免优雅退出时 await 永久阻塞。
    cancel.set();
    agent_task.abort();
    let _ = agent_task.await;

    // agent 任务结束（peer 克隆随 tools 一并 drop）后，回收 MCP 子进程。
    mcp_mgr.shutdown().await;

    // SessionEnd hook：TUI 退出时触发。
    hooks
        .emit(
            crate::hooks::HookEvent::SessionEnd,
            serde_json::json!({ "mode": "tui" }),
        )
        .await;

    tui_res.map_err(crate::error::CarterError::Io)?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// 就地分发内置斜杠命令（agent 任务内，可访问 thread / session / 用量 / 模型）。
#[allow(clippy::too_many_arguments)]
async fn dispatch_builtin(
    name: &str,
    args: &str,
    thread: &mut Thread,
    sink: &mut dyn crate::agent::UiSink,
    cwd: &std::path::Path,
    model: &mut crate::registry::ModelInfo,
    provider: &mut Arc<dyn LlmProvider>,
    fast_provider: &Arc<dyn LlmProvider>,
    fast_model: &crate::registry::ModelInfo,
    config: &Config,
    cache_json: &str,
    custom_cmds: &[crate::commands::SlashCommand],
    skill_metas: &[crate::skills::SkillMeta],
    mcp_tool_count: usize,
    session_usage: &crate::provider::Usage,
    needs_title: &mut bool,
) {
    use crate::agent::UiEvent;
    let notice = |s: String| UiEvent::Notice(s);
    let args = args.trim();
    match name {
        "help" => {
            let mut out = String::from("可用命令：");
            for b in crate::commands::BUILTINS {
                out.push_str(&format!("\n  /{} — {}", b.name, b.description));
            }
            for c in custom_cmds {
                out.push_str(&format!(
                    "\n  /{} — {}",
                    c.name,
                    c.description.as_deref().unwrap_or("自定义命令")
                ));
            }
            sink.emit(notice(out));
        }
        "clear" => {
            thread.messages.clear();
            thread.todos.clear();
            // 落一条 clear 标记（Compacted 空快照），使续接重放到空上下文。
            if let Some(rec) = thread.recorder() {
                rec.record(session::RecordKind::Compacted {
                    tier: "clear".into(),
                    messages: Vec::new(),
                });
            }
            sink.emit(notice("已清空上下文（磁盘原文保留，可 resume 审计）".into()));
            sink.emit(UiEvent::Divider(
                "上下文已清空 · 以下为新对话（之前内容不再发送给模型）".into(),
            ));
        }
        "new" => {
            match session::start_new(cwd, &model.api_name, &session::SessionOpts::default()) {
                Ok((t, _m)) => {
                    *thread = t;
                    *needs_title = true;
                    sink.emit(UiEvent::Divider("新会话".into()));
                }
                Err(e) => sink.emit(notice(format!("新建会话失败：{e}"))),
            }
        }
        "resume" => {
            if args.is_empty() {
                sink.emit(notice(
                    "用法：/resume <id|序号>（`/help` 外可先 `carter sessions` 查看）".into(),
                ));
            } else {
                match find_session(cwd, args).and_then(|e| session::load(&e)) {
                    Ok((t, m)) => swap_thread(thread, sink, needs_title, t, m, "已恢复会话"),
                    Err(e) => sink.emit(notice(format!("恢复失败：{e}"))),
                }
            }
        }
        "fork" => {
            // 有参数 → fork 指定会话；无参数 → fork 当前会话（按当前文件路径）。
            let entry = if args.is_empty() {
                thread.recorder().map(|r| session::SessionEntry {
                    meta: current_meta_placeholder(),
                    path: r.path().to_path_buf(),
                })
            } else {
                find_session(cwd, args).ok()
            };
            match entry {
                Some(e) => match session::fork(&e) {
                    Ok((t, m)) => swap_thread(thread, sink, needs_title, t, m, "已派生新会话"),
                    Err(err) => sink.emit(notice(format!("派生失败：{err}"))),
                },
                None => sink.emit(notice("当前无可派生的会话".into())),
            }
        }
        "model" => {
            if args.is_empty() {
                // 列出当前模型 + 各 provider 下可选模型，方便用户知道能切到什么。
                let mut out = format!(
                    "当前模型：{}\n用法：/model <provider/model>\n可用：",
                    model.key
                );
                let mut provs: Vec<&String> = config.providers.keys().collect();
                provs.sort();
                if provs.is_empty() {
                    out.push_str("\n  （未配置 provider，见 [providers.*]）");
                }
                for p in provs {
                    let mut ms: Vec<&String> = config.providers[p].models.keys().collect();
                    ms.sort();
                    let list = ms
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let kind = &config.providers[p].kind;
                    out.push_str(&format!("\n  {p} [{kind}]: {list}"));
                }
                sink.emit(notice(out));
            } else {
                match resolve_provider(config, cache_json, args) {
                    Ok((m, p)) => {
                        let key = m.key.clone();
                        *model = m;
                        *provider = p;
                        // 状态栏立即更新 + 分隔线标记切换点。
                        sink.emit(UiEvent::ModelChanged(key.clone()));
                        sink.emit(UiEvent::Divider(format!(
                            "已切换模型 {key}（子 agent 仍用旧模型）"
                        )));
                    }
                    Err(e) => sink.emit(notice(format!("切换模型失败：{e}"))),
                }
            }
        }
        "compact" => {
            // 强制压缩（阈值 0）；用 fast 模型。compact 内部已降级处理失败。
            match crate::agent::context::compact(thread, &**fast_provider, fast_model, 0, sink).await
            {
                Ok(()) => sink.emit(UiEvent::Divider("上下文已压缩".into())),
                Err(e) => sink.emit(notice(format!("压缩失败：{e}"))),
            }
        }
        "context" => {
            let est = crate::agent::context::estimate_tokens(&thread.messages);
            let pct = if model.context_window > 0 {
                est as f64 / model.context_window as f64 * 100.0
            } else {
                0.0
            };
            sink.emit(notice(format!(
                "上下文估算 ~{est} tokens / {} 窗口（约 {pct:.1}%）· {} 条消息",
                model.context_window,
                thread.messages.len()
            )));
        }
        "cost" => {
            let cost = crate::cost::compute(session_usage, &model.pricing);
            sink.emit(notice(format!(
                "本会话累计 in={} out={} | 约 ${cost:.4}",
                session_usage.input, session_usage.output
            )));
        }
        "rewind" => {
            if thread.checkpoints.is_empty() {
                sink.emit(notice("无可回滚的文件检查点（本会话尚未改动文件）".into()));
            } else if args.is_empty() {
                let mut out = format!(
                    "文件检查点（{}）· /rewind <序号> 回滚到该点之前：",
                    thread.checkpoints.len()
                );
                for (i, cp) in thread.checkpoints.list().iter().enumerate() {
                    out.push_str(&format!("\n  {}. {}", i + 1, cp.label));
                }
                sink.emit(notice(out));
            } else if let Ok(idx) = args.parse::<usize>() {
                match thread.checkpoints.rewind_to(idx) {
                    Ok(n) => sink.emit(UiEvent::Divider(format!(
                        "已回滚到检查点 {idx} 之前（恢复 {n} 个文件）"
                    ))),
                    Err(e) => sink.emit(notice(format!("回滚失败：{e}"))),
                }
            } else {
                sink.emit(notice("用法：/rewind 或 /rewind <序号>".into()));
            }
        }
        "skills" => {
            if skill_metas.is_empty() {
                sink.emit(notice(
                    "无可用 skills（放在 ~/.carter/skills/<name>/SKILL.md）".into(),
                ));
            } else {
                let mut out = format!("可用 skills（{}）：", skill_metas.len());
                for m in skill_metas {
                    out.push_str(&format!("\n  {} — {}", m.name, m.description));
                }
                sink.emit(notice(out));
            }
        }
        "mcp" => {
            if config.mcp.servers.is_empty() {
                sink.emit(notice("未配置 MCP server（[mcp.servers.*]）".into()));
            } else {
                let mut names: Vec<&String> = config.mcp.servers.keys().collect();
                names.sort();
                let mut out = format!(
                    "MCP servers（{}）· 已加载 {mcp_tool_count} 个工具：",
                    names.len()
                );
                for n in names {
                    let s = &config.mcp.servers[n];
                    let endpoint = s
                        .command
                        .clone()
                        .or_else(|| s.url.clone())
                        .unwrap_or_default();
                    out.push_str(&format!("\n  {n} [{}] {endpoint}", s.transport));
                }
                sink.emit(notice(out));
            }
        }
        _ => sink.emit(notice(format!("/{name}：暂未实现"))),
    }
}

/// 用新 thread 替换当前 thread，回显会话名 + 分隔线 + 回放历史（resume/fork 共用）。
fn swap_thread(
    thread: &mut Thread,
    sink: &mut dyn crate::agent::UiSink,
    needs_title: &mut bool,
    new_thread: Thread,
    meta: session::SessionMeta,
    verb: &str,
) {
    use crate::agent::UiEvent;
    let history = crate::agent::replay_from_messages(&new_thread.messages);
    *thread = new_thread;
    if let Some(t) = &meta.title {
        sink.emit(UiEvent::Title(t.clone()));
        *needs_title = false;
    } else {
        *needs_title = true;
    }
    sink.emit(UiEvent::Divider(format!("{verb} {}", session_label(&meta))));
    if !history.is_empty() {
        sink.emit(UiEvent::ReplayHistory(history));
        sink.emit(UiEvent::Divider("以上为历史上下文".into()));
    }
}

/// 会话的人类可读标签：标题 + 短 id（无标题则仅短 id）。
fn session_label(meta: &session::SessionMeta) -> String {
    let id8: String = meta.id.chars().take(8).collect();
    match &meta.title {
        Some(t) if !t.is_empty() => format!("{t} ({id8})"),
        _ => id8,
    }
}

/// fork 当前会话时的占位 meta（`session::fork` 只用 entry.path，不读 entry.meta）。
fn current_meta_placeholder() -> session::SessionMeta {
    session::SessionMeta {
        id: String::new(),
        parent_id: None,
        forked_from: None,
        cwd: String::new(),
        git: None,
        title: None,
        carter_version: String::new(),
        model: String::new(),
        created_at: 0,
    }
}

/// 跑 `UserPromptSubmit` hook：根据决策返回 `Some(改写后的 prompt)` 或 `None`（被阻断）。
/// 没有任何 hook 注册 → 直接返回 `Some(text)`，无任何开销。
async fn run_user_prompt_submit_hook(
    hooks: &std::sync::Arc<crate::hooks::HookRegistry>,
    text: String,
) -> Option<String> {
    if !hooks.has(crate::hooks::HookEvent::UserPromptSubmit) {
        return Some(text);
    }
    let payload = serde_json::json!({ "prompt": text });
    match hooks
        .dispatch(crate::hooks::HookEvent::UserPromptSubmit, payload)
        .await
    {
        crate::hooks::HookDecision::Continue => Some(text),
        crate::hooks::HookDecision::Rewrite(v) => v
            .get("prompt")
            .and_then(|p| p.as_str())
            .map(str::to_string)
            .or(Some(text)),
        crate::hooks::HookDecision::Block { reason } => {
            tracing::info!("hooks: user prompt blocked: {reason}");
            None
        }
    }
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
