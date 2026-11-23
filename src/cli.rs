//! CLI 入口逻辑。
//!
//! 模块可见性与库根统一在 `lib.rs`；这里只管命令行解析与运行流程分发。
//! 嵌入方（GUI / 服务端）不该走这里，应该直接用 `crate::agent` / `crate::provider`
//! 那几层 —— 本文件里的 `run_cli` 会做几件只有独立进程才该做的事（见
//! `apply_env` 与 `init_tracing` 的说明）。

use clap::{CommandFactory, Parser, Subcommand};

use crate::error::Result;

use std::sync::Arc;

use crate::agent::{
    CancelToken, Capture, NullSink, RunOptions, StdoutSink, Thread, TurnOutcome, UiSink, run_turn,
};
use crate::config::Config;
use crate::provider::LlmProvider;
use crate::registry::fetch;
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

/// 一次性模式的输出形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    /// 边跑边打印（默认）。
    Text,
    /// 过程静默，结束时 stdout 只有一个 JSON 对象。
    Json,
    /// 每个事件一行 NDJSON，末尾一行 `result` 事件。
    StreamJson,
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// 一次性提示词（提供则走一次性 stdout 模式；缺省进交互式 TUI REPL）。
    /// 写 `-` 表示从 stdin 读整段 prompt。
    prompt: Option<String>,

    /// 从文件读 prompt（长任务描述写进文件比塞命令行省心）。与位置参数二选一。
    #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
    prompt_file: Option<String>,

    /// 一次性模式的输出形态：
    /// `text` 边跑边打印（默认）；`json` 过程静默、结束时 stdout 只给一个 JSON 结果对象；
    /// `stream-json` 过程按 NDJSON 事件逐行输出，最后一行是 `result` 事件。
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    /// 覆盖配置里的 `agent.max_turns`（单次 prompt 允许的模型调用轮数上限）。
    /// 跑长任务时把它调高，否则 50 轮一到就被截断。
    #[arg(long, value_name = "N")]
    max_turns: Option<u32>,

    /// 一次性模式的总时长上限（秒）。到点取消本轮并以退出码 124 退出。
    #[arg(long, value_name = "SECS")]
    timeout: Option<u64>,

    /// 目标模式：把 prompt 当作长任务目标。模型停下而目标未完成时自动追加一条
    /// 「继续」再跑一轮，直到模型通过 `goal_update` 报告完成 / 被卡住 / 需要用户拍板，
    /// 或连续空转、出错、触顶自动继续次数。无人值守跑长任务用这个。
    #[arg(long)]
    goal: bool,

    /// 目标模式的自动继续次数上限（缺省 20）。
    #[arg(long, value_name = "N", requires = "goal")]
    max_continues: Option<u32>,

    /// 工作目录（缺省当前目录）。会话、记忆文件、工具的相对路径都以它为准。
    #[arg(long, value_name = "DIR")]
    cwd: Option<String>,

    /// 文本输出不带 ANSI 颜色（stdout 不是终端或设了 `NO_COLOR` 时自动关闭）。
    #[arg(long)]
    no_color: bool,

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

    /// NDJSON 模式：通过 stdin/stdout 双向 JSON 流协议供 host 程序驱动
    /// （VSCode 扩展 / 服务端 / 自动化）。stdin 每行一个 Command，stdout 每行一个 Event。
    #[arg(long)]
    ndjson: bool,

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

/// tracing 写日志文件 `~/.carter/carter.log`（append、无 ANSI），避免 warn 喷到终端
/// 污染 inline TUI viewport / 退出后残留。文件打不开则回落 stderr（oneshot 仍可见）。
/// LLM 请求日志是独立的（见 crate::provider::llm_log），不走这里。
///
/// 用 `try_init` 而不是 `init`：后者在已有全局订阅器时会 **panic**，而宿主进程
/// （GUI / 服务端）通常自己装了一个。装不上就安静跳过 —— 日志少几行，比整个进程崩掉好。
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
                .try_init()
                .ok();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init()
                .ok();
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

pub async fn run_cli() -> Result<std::process::ExitCode> {
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

    // `--cwd`：越早切越好 —— 后面的会话定位、记忆发现、工具相对路径全看进程 cwd。
    if let Some(dir) = &args.cwd {
        std::env::set_current_dir(dir).map_err(|e| {
            crate::error::CarterError::Config(format!("无法切换到工作目录 {dir}: {e}"))
        })?;
    }

    // 首次运行向导：无 config.toml 且处于交互式终端时，引导生成一份并重载。
    if crate::wizard::ensure_config().await? {
        config = Config::load()?;
        apply_env(&config.env);
    }

    // CLI `--system-prompt-file` 覆盖配置里的 agent.system_prompt_file（优先级最高）。
    if let Some(path) = &args.system_prompt_file {
        config.agent.system_prompt_file = Some(path.clone());
    }
    // `--max-turns` 覆盖配置：长任务评测时 50 轮远远不够。
    if let Some(n) = args.max_turns {
        config.agent.max_turns = n;
    }

    // prompt 来源：位置参数 / `-`（stdin）/ `--prompt-file`。先读出来再决定走哪条路。
    let prompt = read_prompt_source(&args)?;

    // 模型元数据：读 ~/.carter/models.json 缓存（缺失给友好提示）。
    let cache_json = fetch::read_cache()?;

    let model_name = args
        .model
        .clone()
        .unwrap_or_else(|| config.agent.model.clone());
    let resolved =
        crate::provider::resolve_provider(&config, &cache_json, &model_name, Vec::new())?;
    let (model, provider) = (resolved.model, resolved.provider);

    // Fast 模型（压缩 / 标题）：配了 agent.fast_model 则解析，否则回落主模型/主 provider。
    let (fast_model, fast_provider): (crate::registry::ModelInfo, Arc<dyn LlmProvider>) =
        match &config.agent.fast_model {
            Some(reference) => {
                let r =
                    crate::provider::resolve_provider(&config, &cache_json, reference, Vec::new())?;
                (r.model, r.provider)
            }
            None => (model.clone(), provider.clone()),
        };

    let show_thinking = config.reasoning.show_thinking && !args.no_thinking;

    // Skills 目录：发现后注入 system；正文由 `skill` 工具按需加载。
    let skills_dir = crate::config::paths::skills_dir();
    let skill_metas = if config.skills.enabled {
        crate::skills::discover(&skills_dir)
    } else {
        Vec::new()
    };

    // 会话解析：resume/fork/continue/新建。在模式分发前完成，oneshot 与 TUI 共用。
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (thread, session_meta) = resolve_session(&cwd, &model_name, &args)?;

    // system 分段数组（按序：人设 + skills + 多层记忆 + 运行环境）。供 oneshot 与 TUI 共用。
    let system = crate::prompt::assemble_system(
        &config,
        &cwd,
        &model.key,
        session_meta.git.as_ref(),
        &skill_metas,
        !args.no_memory,
    );

    // 模式分发：--ndjson → NDJSON 协议；有 prompt 或 --no-tui → 一次性 stdout；否则 TUI。
    if args.ndjson {
        crate::ndjson::run_ndjson(
            thread,
            session_meta,
            provider,
            model,
            fast_provider,
            fast_model,
            config,
            show_thinking,
            model_name,
            system,
            cwd,
        )
        .await
    } else if prompt.is_some() || args.no_tui {
        let opts = OneshotOpts {
            format: args.output_format,
            color: !args.no_color && color_allowed(),
            timeout: args.timeout.map(std::time::Duration::from_secs),
            goal: args.goal,
            max_continues: args
                .max_continues
                .unwrap_or(crate::goal::MAX_AUTO_CONTINUES),
        };
        run_oneshot(
            thread,
            session_meta,
            prompt.unwrap_or_default(),
            provider,
            model,
            fast_provider,
            fast_model,
            config,
            show_thinking,
            system,
            opts,
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

/// 按 CLI 标志解析会话：resume / fork / continue / 新建。
fn resolve_session(
    cwd: &std::path::Path,
    model_name: &str,
    args: &RunArgs,
) -> Result<(Thread, crate::session::SessionMeta)> {
    if let Some(sel) = &args.resume {
        let entry = find_session(cwd, sel)?;
        return crate::session::load(&entry);
    }
    if let Some(sel) = &args.fork {
        let entry = find_session(cwd, sel)?;
        return crate::session::fork(&entry);
    }
    if args.continue_session
        && let Some(entry) = crate::session::latest(cwd)
    {
        return crate::session::load(&entry);
    }
    // 当前目录无历史 → 起新会话。
    let opts = crate::session::SessionOpts {
        session_id: args.session_id.clone(),
    };
    crate::session::start_new(cwd, model_name, &opts)
}

/// 选择器：按 UUID 精确匹配 → `carter sessions` 的 1 基序号 → id 前缀（支持显示的短 id）。
fn find_session(cwd: &std::path::Path, sel: &str) -> Result<crate::session::SessionEntry> {
    let entries = crate::session::list(cwd, false);
    if let Some(e) = entries.iter().find(|e| e.meta.id == *sel) {
        return Ok(e.clone());
    }
    if let Ok(idx) = sel.parse::<usize>()
        && idx >= 1
        && idx <= entries.len()
    {
        return Ok(entries[idx - 1].clone());
    }
    // id 前缀（如状态栏/列表显示的 8 位短 id）。
    let prefixed: Vec<&crate::session::SessionEntry> = entries
        .iter()
        .filter(|e| e.meta.id.starts_with(sel))
        .collect();
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
    let entries = crate::session::list(&cwd, all);
    if entries.is_empty() {
        println!("（无会话）");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let now = crate::session::now_ms();
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
    let entries = crate::session::list(&cwd, all);
    if entries.is_empty() {
        println!("（无会话）");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let (mut total_old, mut total_new) = (0u64, 0u64);
    for e in &entries {
        let label = e.meta.title.clone().unwrap_or_else(|| e.meta.id.clone());
        match crate::session::gc(e) {
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

/// prompt 来源：位置参数（`-` = stdin）或 `--prompt-file`。都没给返回 None（进 TUI）。
fn read_prompt_source(args: &RunArgs) -> Result<Option<String>> {
    if let Some(path) = &args.prompt_file {
        let text = std::fs::read_to_string(path).map_err(|e| {
            crate::error::CarterError::Config(format!("读取 prompt 文件 {path} 失败: {e}"))
        })?;
        return Ok(Some(text.trim_end().to_string()));
    }
    match args.prompt.as_deref() {
        Some("-") => {
            use std::io::Read;
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            Ok(Some(text.trim_end().to_string()))
        }
        Some(p) => Ok(Some(p.to_string())),
        None => Ok(None),
    }
}

/// stdout 是终端且没设 `NO_COLOR` 才上色。管道 / 重定向到文件时裸文本更好用。
fn color_allowed() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// 一次性模式的几个开关，免得 `run_oneshot` 的参数表再长下去。
struct OneshotOpts {
    format: OutputFormat,
    color: bool,
    timeout: Option<std::time::Duration>,
    /// 目标模式（见 `RunArgs::goal`）。
    goal: bool,
    max_continues: u32,
}

/// 一次性模式的最终汇总（`--output-format json` 整个 stdout 就是它；
/// `stream-json` 作为最后一行 `result` 事件）。字段名一旦发出就是对外约定，别随手改。
#[derive(Debug, serde::Serialize)]
struct OneshotResult {
    #[serde(rename = "type")]
    kind: &'static str,
    /// `assistant`（正常收尾）/ `limit`（触顶 max_turns）/ `cancelled`
    /// （Ctrl+C）/ `timeout` / `error`。
    outcome: &'static str,
    session_id: String,
    model: String,
    /// 最后一段 assistant 正文。
    result: String,
    /// 本次 prompt 实际发出的模型请求数。
    turns: u32,
    duration_ms: u64,
    usage: crate::ndjson::WireUsage,
    cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// `--goal` 时才有：目标最终状态与自动继续次数。
    #[serde(skip_serializing_if = "Option::is_none")]
    goal: Option<GoalSummary>,
}

/// 目标模式的收尾信息。`status` 取值同 [`crate::goal::GoalStatus::as_str`]：
/// `completed` / `blocked` / `paused`（需要用户拍板、空转、出错、触顶都落在这里，
/// 看 `reason`）/ `active`（被 Ctrl+C / 超时打断，目标本身没有收束）。
#[derive(Debug, serde::Serialize)]
struct GoalSummary {
    status: &'static str,
    /// 为什么停下（`decide_after_turn` 给的那句话）。
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// 模型在 `goal_update` 里写的说明。
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    auto_continues: u32,
}

/// 一次性 stdout 模式（向后兼容 M1–M3）。
#[allow(clippy::too_many_arguments)]
async fn run_oneshot(
    mut thread: Thread,
    session_meta: crate::session::SessionMeta,
    prompt: String,
    provider: Arc<dyn LlmProvider>,
    model: crate::registry::ModelInfo,
    fast_provider: Arc<dyn LlmProvider>,
    fast_model: crate::registry::ModelInfo,
    config: Config,
    show_thinking: bool,
    system: Vec<String>,
    opts: OneshotOpts,
) -> Result<std::process::ExitCode> {
    let started = std::time::Instant::now();
    // 续接时 thread 已含历史；追加本次 prompt（并落盘）。
    // `@图片.png` 等内联图片附件先转成 `[img:...]` token，便于 provider 边界组多模态。
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let hooks = std::sync::Arc::new(crate::hooks::HookRegistry::from_configs(
        config.hooks.clone(),
    ));
    let prompt = crate::media::inline_user_attachments(&prompt, &cwd);
    // UserPromptSubmit hook：可改写 prompt，可阻断。
    let prompt = crate::hooks::run_user_prompt_submit(&hooks, prompt).await;
    let Some(prompt) = prompt else {
        // 被 hook 阻断。
        return Ok(std::process::ExitCode::SUCCESS);
    };
    thread.append_user(prompt.clone());
    // 子 agent 复用人设段（system[0]）作为其 base system。
    let base_system = system.first().cloned().unwrap_or_default();
    let mut run_opts = RunOptions {
        show_thinking,
        system,
        compact_model: Some(crate::agent::CompactModel {
            provider: fast_provider,
            model: fast_model,
        }),
        hooks: hooks.clone(),
        purpose: crate::provider::RequestPurpose::MainTurn,
        steer: None,
    };
    let cancel = CancelToken::new();
    // 目标模式：目标正文就是这条 prompt；目标段挂到系统提示末尾（见 `goal::system_segment`）。
    let mut goal: Option<crate::goal::Goal> = opts.goal.then(|| crate::goal::Goal::new(&prompt));
    let goal_signal: Option<Arc<crate::goal::GoalSignal>> = opts
        .goal
        .then(|| Arc::new(crate::goal::GoalSignal::default()));
    if let Some(seg) = goal.as_ref().and_then(crate::goal::system_segment) {
        run_opts.system.push(seg);
    }
    if let Some(sig) = &goal_signal {
        sig.set_armed(true);
    }
    let (mcp_mgr, mcp_tools) = crate::mcp::McpManager::start(&config.mcp).await;
    let tools = crate::agent::build_tools(
        config.skills.enabled,
        provider.clone(),
        model.clone(),
        config.agent.clone(),
        base_system,
        cancel.clone(),
        mcp_tools,
        None, // oneshot 模式无 TUI，ask_user_question 工具不可用
        None, // 一次性执行没有下一轮来接着做，后台命令表没有意义
        Some(cwd.clone()),
        // 只有 `--goal` 才挂 `goal_update`：普通一次性执行跑完就退出，
        // 留着这个工具只会让模型以为有人接着推。
        goal_signal.clone(),
        // 一次性执行没有下一轮来接结论，后台子 agent 同样没有意义。
        None,
    );

    // 出口按输出形态选：text 裸打印；json 静默只留汇总；stream-json 走 NDJSON 事件行。
    // 三种都包一层 Capture，拿到最终正文给汇总用。
    let mut sink: Capture<Box<dyn UiSink>> = Capture::new(match opts.format {
        OutputFormat::Text => Box::new(StdoutSink::new().with_color(opts.color)),
        OutputFormat::Json => Box::new(NullSink),
        OutputFormat::StreamJson => Box::new(crate::ndjson::NdjsonSink::detached()),
    });

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

    // Ctrl+C → 协作式取消（让这一轮正常收尾、MCP 子进程回收），不是硬杀。
    // `--timeout` 同理：到点只是 set 同一个 token。
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = {
        let cancel = cancel.clone();
        let interrupted = interrupted.clone();
        let timed_out = timed_out.clone();
        let timeout = opts.timeout;
        tokio::spawn(async move {
            let sleep = async {
                match timeout {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                _ = sleep => {
                    timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            cancel.set();
        })
    };

    // 一轮，加上 `--goal` 推起来的自动继续。退出条件全在 `goal::decide_after_turn`
    // （与 TUI 共用同一个纯函数），这里只负责照做。没开目标模式时循环只跑一次。
    let mut usage = crate::provider::Usage::default();
    let mut idle_streak: u32 = 0;
    let mut goal_reason: Option<String> = None;
    let outcome = loop {
        let before = thread.messages.len();
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
        // 致命错误（provider 连不上等）在 json 形态下也要进汇总，而不是只剩 stderr 一行。
        let (outcome, turn_usage) = match run_res {
            Ok(v) => v,
            Err(e) => (TurnOutcome::Error(e.to_string()), Default::default()),
        };
        usage.add(&turn_usage);

        let (Some(g), Some(sig)) = (goal.as_mut(), goal_signal.as_ref()) else {
            break outcome;
        };
        // 这一轮到底动了没有：历史里新出现的工具调用就是「在干活」。
        let used_tools = thread.messages[before..].iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, crate::provider::Block::ToolUse { .. }))
        });
        idle_streak = if used_tools { 0 } else { idle_streak + 1 };
        g.idle_streak = idle_streak;
        let decision = crate::goal::decide_after_turn(
            g,
            &crate::goal::GoalOutcome {
                user_cancelled: matches!(outcome, TurnOutcome::Cancelled),
                failed: matches!(outcome, TurnOutcome::Limit | TurnOutcome::Error(_)),
                used_tools,
                idle_streak,
                signal: sig.take(),
            },
        );
        match decision {
            crate::goal::Decision::None => break outcome,
            crate::goal::Decision::Set {
                status,
                note,
                notice,
            } => {
                g.status = status;
                g.note = note;
                g.updated_at = crate::session::now_ms();
                sink.emit(crate::agent::UiEvent::Notice(notice.clone()));
                goal_reason = Some(notice);
                break outcome;
            }
            // 上限用 CLI 自己的（`--max-continues`），决策函数里那个常量只是缺省值。
            crate::goal::Decision::Continue if g.auto_continues >= opts.max_continues => {
                g.status = crate::goal::GoalStatus::Paused;
                g.updated_at = crate::session::now_ms();
                let notice = format!("已连续自动继续 {} 轮，停下。", opts.max_continues);
                sink.emit(crate::agent::UiEvent::Notice(notice.clone()));
                goal_reason = Some(notice);
                break outcome;
            }
            crate::goal::Decision::Continue => {
                g.auto_continues += 1;
                if cancel.is_cancelled() {
                    break outcome;
                }
                sink.emit(crate::agent::UiEvent::Divider(format!(
                    "自动继续 {}/{}",
                    g.auto_continues, opts.max_continues
                )));
                thread.append_user(crate::goal::CONTINUE_PROMPT);
            }
        }
    };
    watchdog.abort();
    // 目标状态落盘：`-r` 续接这个会话进 TUI 时能看到它停在哪。
    if let Some(g) = &goal {
        let _ = crate::goal::save(&session_meta.id, g);
    }

    // 回收 MCP 子进程（无论成功失败都先 shutdown，再传播错误）。
    mcp_mgr.shutdown().await;
    let outcome = match outcome {
        TurnOutcome::Cancelled if timed_out.load(std::sync::atomic::Ordering::SeqCst) => {
            TurnOutcome::Error(format!(
                "timeout after {}s",
                opts.timeout.map(|d| d.as_secs()).unwrap_or_default()
            ))
        }
        o => o,
    };
    let is_timeout = timed_out.load(std::sync::atomic::Ordering::SeqCst);

    // SessionEnd hook：返回最终用量。
    hooks
        .emit(
            crate::hooks::HookEvent::SessionEnd,
            serde_json::json!({
                "mode": "oneshot",
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            }),
        )
        .await;

    if !matches!(opts.format, OutputFormat::Text) {
        let (label, error) = match &outcome {
            TurnOutcome::Assistant => ("assistant", None),
            TurnOutcome::Limit => ("limit", None),
            TurnOutcome::Cancelled => ("cancelled", None),
            TurnOutcome::Error(msg) if is_timeout => ("timeout", Some(msg.clone())),
            TurnOutcome::Error(msg) => ("error", Some(msg.clone())),
        };
        let summary = OneshotResult {
            kind: "result",
            outcome: label,
            session_id: session_meta.id.clone(),
            model: model.key.clone(),
            result: sink.final_text().to_string(),
            turns: thread.turns,
            duration_ms: started.elapsed().as_millis() as u64,
            usage: crate::ndjson::WireUsage::from(&usage),
            // 六位小数：再往后全是浮点噪声（0.048438999999）。
            cost_usd: (crate::cost::compute(&usage, &model.pricing) * 1e6).round() / 1e6,
            error,
            goal: goal.as_ref().map(|g| GoalSummary {
                status: g.status.as_str(),
                reason: goal_reason.clone(),
                note: g.note.clone(),
                auto_continues: g.auto_continues,
            }),
        };
        let line = serde_json::to_string(&summary).unwrap_or_else(|e| {
            format!("{{\"type\":\"result\",\"outcome\":\"error\",\"error\":\"serialize: {e}\"}}")
        });
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    if is_timeout {
        // 与 coreutils `timeout` 的约定一致。
        return Ok(std::process::ExitCode::from(124));
    }
    Ok(outcome_to_code(
        outcome,
        matches!(opts.format, OutputFormat::Text),
    ))
}

/// `/model` 弹窗的候选：配置里每个 `provider/model`，描述栏放协议种类 + 元数据摘要。
/// 元数据查不到的只标协议，不让一个坏 meta 把整个列表拖垮。
fn model_completion_items(config: &Config, cache_json: &str) -> Vec<crate::tui::CompletionItem> {
    let mut items = Vec::new();
    let mut provs: Vec<&String> = config.providers.keys().collect();
    provs.sort();
    for p in provs {
        let pcfg = &config.providers[p];
        let mut ms: Vec<&String> = pcfg.models.keys().collect();
        ms.sort();
        for m in ms {
            let summary = pcfg.models[m]
                .meta
                .split_once('/')
                .and_then(|(pid, mid)| {
                    crate::registry::catalog::models(cache_json, pid)
                        .into_iter()
                        .find(|e| e.id == mid)
                })
                .map(|e| e.summary())
                .unwrap_or_default();
            let description = if summary.is_empty() {
                format!("[{}]", pcfg.kind)
            } else {
                format!("[{}] {summary}", pcfg.kind)
            };
            items.push(crate::tui::CompletionItem {
                name: format!("{p}/{m}"),
                description,
                hint: None,
            });
        }
    }
    items
}

/// 交互式 TUI REPL 模式。agent loop 跑在独立任务里，经通道与 TUI 通信。
#[allow(clippy::too_many_arguments)]
async fn run_tui(
    thread: Thread,
    session_meta: crate::session::SessionMeta,
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
    // 插话没被取走时退回队列要用：agent 任务持一份，TUI 持另一份。
    let input_tx_for_retry = input_tx.clone();
    // 插话队列：一轮进行中回车进来的话，交给正在跑的那一轮的下一次请求。
    let steer = crate::agent::SteerInbox::new();
    let agent_steer = steer.clone();
    let cancel = CancelToken::new();
    // 后台命令表：进程级一份，活过整个 REPL。关会话 / 退出时 `kill_all` 收掉。
    let shells = crate::tools::BackgroundShells::new();
    // 后台子 agent 表：与后台命令同一套做法（派完就收工，结束了再被叫回来）。
    let agents_bg = crate::agent::BackgroundAgents::new();

    // MCP：会话级 manager 留在外层 scope（活过整个 agent 任务）；工具（持克隆 peer）移入任务。
    let (mcp_mgr, mcp_tools) = crate::mcp::McpManager::start(&config.mcp).await;
    // /mcp 用：移入 build_tools 前先记下已加载的 MCP 工具数。
    let mcp_tool_count = mcp_tools.len();

    // 生命周期 hook 注册表（按事件分桶）。
    let hooks = std::sync::Arc::new(crate::hooks::HookRegistry::from_configs(
        config.hooks.clone(),
    ));

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

    // `/model` 候选 = 配置里写了块的模型（带元数据摘要）+ 启动后后台向各端点拉到的
    // 模型列表（配置里没写块的那些也能选）。弹窗每次打开现取，所以拉完即可见。
    let model_items = std::sync::Arc::new(std::sync::Mutex::new(model_completion_items(
        &config,
        &cache_json,
    )));
    {
        let items = model_items.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            let remote = crate::provider::models_api::list_for_config(&cfg).await;
            let mut extra: Vec<crate::tui::CompletionItem> = Vec::new();
            for (pname, list) in remote {
                let kind = cfg.providers[&pname].kind.clone();
                for m in list {
                    // 配置里已经有块的跳过：那条已带元数据摘要，别重复一条没摘要的。
                    if cfg.providers[&pname].models.contains_key(&m.id) {
                        continue;
                    }
                    extra.push(crate::tui::CompletionItem {
                        name: format!("{pname}/{}", m.id),
                        description: format!("[{kind}] 端点列表"),
                        hint: None,
                    });
                }
            }
            if let Ok(mut guard) = items.lock() {
                guard.extend(extra);
            }
        });
    }
    let model_candidates: Box<dyn Fn() -> Vec<crate::tui::CompletionItem> + Send> = {
        let items = model_items.clone();
        Box::new(move || items.lock().map(|g| g.clone()).unwrap_or_default())
    };
    let arg_completions: Vec<(String, Vec<crate::tui::CompletionItem>)> = Vec::new();

    // /resume·/fork → 会话候选动态来源：每次打开弹窗重新列会话（新建即时可见）。
    let sess_cwd = cwd.clone();
    let session_candidates: Box<dyn Fn() -> Vec<crate::tui::CompletionItem> + Send> =
        Box::new(move || {
            crate::session::list(&sess_cwd, false)
                .iter()
                .map(|e| crate::tui::CompletionItem {
                    name: e.meta.id.chars().take(8).collect(),
                    description: e.meta.title.clone().unwrap_or_else(|| "（无标题）".into()),
                    hint: None,
                })
                .collect()
        });

    // 跨会话输入历史：启动时载入（上下方向键召回）。
    let input_history = crate::session::history::load();

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

    // 长任务目标（`/goal`）：信号格子给 `goal_update` 工具，目标状态由主循环持有。
    let goal_signal = std::sync::Arc::new(crate::goal::GoalSignal::default());
    // 盘上处于 active 的一律降级成 paused：用户关掉终端时目标还在跑，下次打开
    // 原样恢复就意味着人不在的时候模型自己开工烧 token。
    let (start_goal, goal_downgraded) = match crate::goal::load(&session_meta.id) {
        Some(g) => {
            let (g, downgraded) = crate::goal::on_session_start(g);
            (Some(g), downgraded)
        }
        None => (None, false),
    };

    // agent 任务：连续多轮，每轮一个 prompt → run_turn → emit 事件。
    let agent_cancel = cancel.clone();
    let agent_hooks = hooks.clone();
    let agent_shells = shells.clone();
    let agent_agents = agents_bg.clone();
    let agent_cwd = cwd.clone();
    let agent_task = tokio::spawn(async move {
        let mut thread = thread;
        // 子 agent 复用人设段（system[0]）作为其 base system。
        let base_system = system.first().cloned().unwrap_or_default();
        let mut run_opts = RunOptions {
            show_thinking,
            system,
            compact_model: Some(crate::agent::CompactModel {
                provider: fast_provider.clone(),
                model: fast_model.clone(),
            }),
            hooks: agent_hooks.clone(),
            purpose: crate::provider::RequestPurpose::MainTurn,
            steer: Some(agent_steer.clone()),
        };
        // 目标段是系统提示的**最后一段**，每轮现接（见 `goal::system_segment`）。
        // 基段留着不动，否则目标一改、下一轮再拼就把上一段也拼进去了。
        let system_base = run_opts.system.clone();
        let tools = crate::agent::build_tools(
            config.skills.enabled,
            provider.clone(),
            model.clone(),
            config.agent.clone(),
            base_system,
            agent_cancel.clone(),
            mcp_tools,
            Some(ui_tx.clone()), // TUI 模式：ask_user_question 工具走这个通道发 UiEvent::AskUser
            Some(agent_shells.clone()),
            Some(agent_cwd),
            Some(goal_signal.clone()),
            Some(agent_agents.clone()),
        );
        // 主模型 / provider 设为可变：会话内 `/model` 热切换会重绑它们（run_turn 每轮取最新）。
        // 注：子 agent 的 `task` 工具仍持旧 model（重建 MCP 工具代价高，列为已知限制）。
        let mut model = model;
        let mut provider = provider;
        // 起标题跑在另一条任务里，事件也要能发回 TUI：留一份发送端。
        let title_tx = ui_tx.clone();
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
        // 目标状态：每轮现算系统提示段、每轮结束后决定要不要再推一轮。
        let mut goal: Option<crate::goal::Goal> = start_goal;
        let mut idle_streak = 0u32;
        if goal_downgraded {
            sink.emit(crate::agent::UiEvent::Notice(
                "上次退出时这个目标还在进行中，已改为暂停。用 /goal resume 可以接着推进。".into(),
            ));
        }
        if let Some(g) = &goal {
            sink.emit(crate::agent::UiEvent::Notice(format!(
                "目标（{}）：{}",
                g.status.label(),
                g.objective
            )));
        }
        // 本会话累计用量（供 /cost）。
        let mut session_usage = crate::provider::Usage::default();
        // 后台命令自己结束时的通知：拿到 shell_id 后取正文，作为一条 user 消息
        // 追进历史再跑一轮 —— 否则模型启动完后台命令就结束了一轮，之后没人推进。
        let mut shell_done = agent_shells.subscribe();
        // 后台子 agent 全部结束时的通知：同一条路。
        let mut agents_done = agent_agents.subscribe();
        loop {
            let prompt = tokio::select! {
                p = input_rx.recv() => match p {
                    Some(p) => p,
                    None => break,
                },
                // 后台命令自己结束了：完成通知作为一条 user 消息追进历史，再跑一轮。
                // 与用户输入放在同一个 select 里 —— 命令往往在上一轮结束之后才跑完，
                // 轮末检查一次是接不到的。
                id = shell_done.recv() => {
                    let Some(id) = id else { continue };
                    let Some(text) = agent_shells.completion(&id) else { continue };
                    sink.emit(crate::agent::UiEvent::Notice(format!(
                        "后台命令 {id} 已结束，接着处理"
                    )));
                    thread.append_user(text);
                    agent_steer.set_open(true);
                    let usage = run_turn(
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
                    agent_steer.set_open(false);
                    if let Ok((_, usage)) = usage {
                        session_usage.add(&usage);
                    }
                    sink.emit(crate::agent::UiEvent::Idle);
                    continue;
                }
                // 后台子 agent 全部结束：结论作为一条 user 消息追进历史，再跑一轮。
                id = agents_done.recv() => {
                    let Some(id) = id else { continue };
                    let Some(text) = agent_agents.completion(&id) else { continue };
                    sink.emit(crate::agent::UiEvent::Notice(format!(
                        "后台子 agent {id} 已结束，接着处理"
                    )));
                    thread.append_user(text);
                    agent_steer.set_open(true);
                    let usage = run_turn(
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
                    agent_steer.set_open(false);
                    if let Ok((_, usage)) = usage {
                        session_usage.add(&usage);
                    }
                    sink.emit(crate::agent::UiEvent::Idle);
                    continue;
                }
            };
            // 记录到跨会话输入历史（原始输入，含斜杠命令）。
            crate::session::history::append(&session_meta.id, &prompt);
            // 斜杠命令解析：自定义命令展开为 prompt；内置命令就地分发（无需跑 turn）。
            // `/quit`·`/exit` 已由 TUI 层在提交前拦截。
            // `/btw <问题>`：侧边问一句。带着主对话的转录跑**一轮**，不写进这条对话。
            // 放在这里而不是 `dispatch_builtin`：它要用工具表与 provider，而那些在
            // 闭包里、那个函数看不到。
            let btw = prompt
                .trim_start()
                .strip_prefix("/btw")
                .or_else(|| prompt.trim_start().strip_prefix("/side"))
                .map(|rest| rest.trim().to_string());
            if let Some(question) = btw {
                if question.is_empty() {
                    sink.emit(crate::agent::UiEvent::Notice(
                        "用法：/btw <问题>（带着主对话的上下文问一句，不影响这条对话）".into(),
                    ));
                } else {
                    let mut side_thread = Thread::new_empty();
                    side_thread.append_user(question.clone());
                    let mut side_system = system_base.clone();
                    side_system.push(crate::side::INSTRUCTIONS.to_string());
                    side_system.push(crate::side::transcript(&thread.messages, &[]));
                    let side_opts = RunOptions {
                        show_thinking: run_opts.show_thinking,
                        system: side_system,
                        compact_model: None,
                        hooks: run_opts.hooks.clone(),
                        purpose: crate::provider::RequestPurpose::MainTurn,
                        // 侧边会话只听这一问；主对话的插话队列不归它。
                        steer: None,
                    };
                    sink.emit(crate::agent::UiEvent::Divider("侧边聊天".into()));
                    let _ = run_turn(
                        &mut side_thread,
                        &*provider,
                        &model,
                        &config.agent,
                        &side_opts,
                        &tools,
                        &mut sink,
                        &agent_cancel,
                    )
                    .await;
                    sink.emit(crate::agent::UiEvent::Divider("回到主对话".into()));
                }
                sink.emit(crate::agent::UiEvent::Idle);
                continue;
            }

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
                            &mut goal,
                            &session_meta.id,
                            &mut idle_streak,
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
                let text = match crate::hooks::run_user_prompt_submit(&agent_hooks, text).await {
                    Some(t) => t,
                    None => {
                        sink.emit(crate::agent::UiEvent::Notice(
                            "[hook] prompt blocked by user_prompt_submit hook".into(),
                        ));
                        sink.emit(crate::agent::UiEvent::Idle);
                        continue;
                    }
                };
                // 循环里还要用这段正文（第一轮用它，后续几轮是 CONTINUE_PROMPT）。
                let prompt_text = text.clone();
                thread.append_user(text);
                if needs_title {
                    needs_title = false;
                    // 起标题**不挡这一轮**：它是一次完整的模型调用，串在这里，用户就得
                    // 先等标题回来才看得到第一句回答。另起一条任务，回来了再落盘、通知界面。
                    // fast 模型短输出；失败静默忽略（错误即数据，不阻断会话）。
                    let title_provider = fast_provider.clone();
                    let title_model = fast_model.clone();
                    let session_id = thread.session_id.clone();
                    let recorder = thread.recorder();
                    let title_tx = title_tx.clone();
                    let prompt = prompt.clone();
                    tokio::spawn(async move {
                        let Ok(title) = crate::agent::generate_title(
                            &prompt,
                            session_id.as_deref(),
                            &*title_provider,
                            &title_model,
                        )
                        .await
                        else {
                            return;
                        };
                        if title.is_empty() {
                            return;
                        }
                        // 会话自己那个 recorder（内部有锁），与主循环的写入不会绞在一起。
                        if let Some(rec) = recorder {
                            rec.record(crate::session::RecordKind::Title {
                                title: title.clone(),
                            });
                        }
                        let _ = title_tx.send(crate::agent::UiEvent::Title(title));
                    });
                }
                // 一轮，加上 `/goal` 推起来的自动继续。退出条件全在
                // `goal::decide_after_turn`（纯函数、逐条测过），这里只负责照做。
                let mut next: Option<String> = None;
                loop {
                    let user_text = match next.take() {
                        Some(t) => {
                            thread.append_user(t.clone());
                            t
                        }
                        None => prompt_text.clone(),
                    };
                    let _ = &user_text;

                    // 只有 active 时模型才该调 `goal_update`；目标段每轮现接。
                    goal_signal.set_armed(
                        goal.as_ref()
                            .is_some_and(|g| g.status == crate::goal::GoalStatus::Active),
                    );
                    run_opts.system = system_base.clone();
                    if let Some(seg) = goal.as_ref().and_then(crate::goal::system_segment) {
                        run_opts.system.push(seg);
                    }

                    let before = thread.messages.len();
                    // 这一轮进行中，回车进来的话走插话；跑完立刻关门，空闲时照旧排队。
                    agent_steer.set_open(true);
                    let outcome = run_turn(
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
                    agent_steer.set_open(false);
                    if let Ok((_, usage)) = &outcome {
                        session_usage.add(usage);
                    }
                    // 这一轮后面没再发请求，插话没被取走：退回队列，按下一轮正常跑。
                    for t in agent_steer.drain() {
                        let raw = t
                            .strip_prefix(crate::agent::STEER_HEAD)
                            .map(|s| s.trim_start_matches('\n').to_string())
                            .unwrap_or(t);
                        if input_tx_for_retry.send(raw).is_err() {
                            break;
                        }
                    }

                    // 这一轮到底动了没有：历史里新出现的工具调用就是「在干活」。
                    let used_tools = thread.messages[before..].iter().any(|m| {
                        m.content
                            .iter()
                            .any(|b| matches!(b, crate::provider::Block::ToolUse { .. }))
                    });
                    let user_cancelled = matches!(outcome, Ok((TurnOutcome::Cancelled, _)));
                    let failed = match &outcome {
                        Err(_) => true,
                        Ok((TurnOutcome::Cancelled | TurnOutcome::Assistant, _)) => false,
                        Ok((TurnOutcome::Limit | TurnOutcome::Error(_), _)) => true,
                    };
                    let Some(mut g) = goal.clone() else { break };
                    idle_streak = if used_tools {
                        0
                    } else {
                        idle_streak.saturating_add(1)
                    };
                    g.idle_streak = idle_streak;
                    let decision = crate::goal::decide_after_turn(
                        &g,
                        &crate::goal::GoalOutcome {
                            user_cancelled,
                            failed,
                            used_tools,
                            idle_streak,
                            signal: goal_signal.take(),
                        },
                    );
                    match decision {
                        crate::goal::Decision::None => {
                            let _ = crate::goal::save(&session_meta.id, &g);
                            goal = Some(g);
                            break;
                        }
                        crate::goal::Decision::Set {
                            status,
                            note,
                            notice,
                        } => {
                            g.status = status;
                            g.note = note;
                            g.updated_at = crate::session::now_ms();
                            let _ = crate::goal::save(&session_meta.id, &g);
                            goal = Some(g.clone());
                            sink.emit(crate::agent::UiEvent::Notice(notice));
                            break;
                        }
                        crate::goal::Decision::Continue => {
                            g.auto_continues = g.auto_continues.saturating_add(1);
                            let _ = crate::goal::save(&session_meta.id, &g);
                            goal = Some(g.clone());
                            // 给「停止」留一点手感窗口，也避免退化成 tight loop。
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                            if agent_cancel.is_cancelled() {
                                break;
                            }
                            next = Some(crate::goal::CONTINUE_PROMPT.to_string());
                        }
                    }
                }
                // 错误即数据：run_turn 内部已 emit；这里忽略 Err 让 REPL 继续。
            }

            // 无论本次是跑了一轮、执行了斜杠命令、还是被取消 / 出错，
            // 都发一个 Idle 让 TUI 退出 streaming 态、恢复可交互（修复卡死）。
            sink.emit(crate::agent::UiEvent::Idle);
        }
    });

    // 跑 TUI 主循环（阻塞直到用户退出）。
    let tui_res = crate::tui::run(
        model_label,
        completion_items,
        arg_completions,
        session_candidates,
        model_candidates,
        input_history,
        tui_cwd,
        ui_rx,
        input_tx,
        steer,
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
    goal: &mut Option<crate::goal::Goal>,
    session_id: &str,
    idle_streak: &mut u32,
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
                rec.record(crate::session::RecordKind::Compacted {
                    tier: "clear".into(),
                    messages: Vec::new(),
                });
            }
            sink.emit(notice(
                "已清空上下文（磁盘原文保留，可 resume 审计）".into(),
            ));
            sink.emit(UiEvent::Divider(
                "上下文已清空 · 以下为新对话（之前内容不再发送给模型）".into(),
            ));
        }
        "new" => {
            match crate::session::start_new(
                cwd,
                &model.api_name,
                &crate::session::SessionOpts::default(),
            ) {
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
                match find_session(cwd, args).and_then(|e| crate::session::load(&e)) {
                    Ok((t, m)) => swap_thread(thread, sink, needs_title, t, m, "已恢复会话"),
                    Err(e) => sink.emit(notice(format!("恢复失败：{e}"))),
                }
            }
        }
        "fork" => {
            // 有参数 → fork 指定会话；无参数 → fork 当前会话（按当前文件路径）。
            let entry = if args.is_empty() {
                thread.recorder().map(|r| crate::session::SessionEntry {
                    meta: current_meta_placeholder(),
                    path: r.path().to_path_buf(),
                })
            } else {
                find_session(cwd, args).ok()
            };
            match entry {
                Some(e) => match crate::session::fork(&e) {
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
                    "当前模型：{}\n用法：/model <provider/model>（TUI 里输入 /model 回车可弹列表选择）\n可用：",
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
                    let list = ms.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
                    let kind = &config.providers[p].kind;
                    out.push_str(&format!("\n  {p} [{kind}]: {list}"));
                }
                sink.emit(notice(out));
            } else {
                // 宽松解析：端点列表里选出来、配置里没写块的模型也能切。
                match crate::provider::resolve_provider_lenient(
                    config,
                    cache_json,
                    args,
                    Vec::new(),
                )
                .map(|r| (r.model, r.provider))
                {
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
            match crate::agent::context::compact(thread, &**fast_provider, fast_model, 0, sink)
                .await
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
                session_usage.input_tokens, session_usage.output_tokens
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
        "goal" => {
            // 子命令：status / pause / resume / complete / clear；其余一律当成
            // 「设定目标正文」。与 GUI 的目标条同一套状态机（`goal::apply_action`）。
            use crate::goal::{self, GoalCommand, GoalStatus};
            let action = match args {
                "" | "status" => {
                    match goal.as_ref() {
                        Some(g) => {
                            let mut out = format!(
                                "目标（{}）：{}\n自动继续 {}/{} 轮",
                                g.status.label(),
                                g.objective,
                                g.auto_continues,
                                goal::MAX_AUTO_CONTINUES
                            );
                            if let Some(note) = &g.note {
                                out.push_str(&format!("\n说明：{note}"));
                            }
                            out.push_str(
                                "\n\n/goal <目标> 设定 · /goal pause 暂停 · /goal resume 继续 · /goal complete 完成 · /goal clear 清除",
                            );
                            sink.emit(notice(out));
                        }
                        None => sink.emit(notice(
                            "还没有设定目标。用 `/goal <目标>` 设一个，模型未完成时会自动接着推。"
                                .into(),
                        )),
                    }
                    return;
                }
                "pause" => GoalCommand::Pause,
                "resume" => GoalCommand::Resume,
                "complete" | "done" => GoalCommand::Complete,
                "clear" | "off" => GoalCommand::Clear,
                other => GoalCommand::Set {
                    objective: other.to_string(),
                },
            };
            let was_active = goal
                .as_ref()
                .is_some_and(|g| g.status == GoalStatus::Active);
            match goal::apply_action(goal.clone(), &action) {
                Some(next) => {
                    if let Some(g) = &next {
                        let _ = goal::save(session_id, g);
                    } else {
                        let _ = goal::remove(session_id);
                    }
                    let now_active = next
                        .as_ref()
                        .is_some_and(|g| g.status == GoalStatus::Active);
                    *goal = next;
                    // 从「不推进」变成「推进」：清零空转计数，立刻开干。
                    if now_active && !was_active {
                        *idle_streak = 0;
                        sink.emit(UiEvent::Notice(
                            "目标已设定，开始推进。停下时目标还没完成会自动再推一轮。".into(),
                        ));
                    } else {
                        sink.emit(notice(match action {
                            GoalCommand::Pause => "目标已暂停：不会再自动继续。".to_string(),
                            GoalCommand::Resume => "目标已继续。".to_string(),
                            GoalCommand::Complete => "目标已标记为完成。".to_string(),
                            GoalCommand::Clear => "目标已清除。".to_string(),
                            GoalCommand::Set { .. } => "目标已更新。".to_string(),
                        }));
                    }
                }
                None => sink.emit(notice(
                    "这个动作在当前状态下没有意义（`/goal` 看现状）。".into(),
                )),
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
    meta: crate::session::SessionMeta,
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
fn session_label(meta: &crate::session::SessionMeta) -> String {
    let id8: String = meta.id.chars().take(8).collect();
    match &meta.title {
        Some(t) if !t.is_empty() => format!("{t} ({id8})"),
        _ => id8,
    }
}

/// fork 当前会话时的占位 meta（`crate::session::fork` 只用 entry.path，不读 entry.meta）。
fn current_meta_placeholder() -> crate::session::SessionMeta {
    crate::session::SessionMeta {
        id: String::new(),
        format_version: crate::session::CURRENT_FORMAT_VERSION,
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

/// turn 结果 → 进程退出码。`print_error` 为 false 时错误已经进了 JSON 汇总，
/// 不再往 stderr 重复一遍。
fn outcome_to_code(outcome: TurnOutcome, print_error: bool) -> std::process::ExitCode {
    match outcome {
        TurnOutcome::Assistant | TurnOutcome::Limit => std::process::ExitCode::SUCCESS,
        TurnOutcome::Cancelled => std::process::ExitCode::from(130),
        TurnOutcome::Error(msg) => {
            if print_error {
                eprintln!("turn error: {msg}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}
