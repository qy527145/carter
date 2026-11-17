//! bash：执行 shell 命令，捕获 stdout/stderr/exit_code。
//!
//! ## 跨平台
//!
//! 用 [`crate::platform::detect_shell`] 探测本机的 shell：Windows 上优先 POSIX sh，
//! 找不到才退回 `cmd /C`；Unix 上就是 `sh`。探测结果也写进工具描述 —— 模型只能从
//! 描述里知道该写 `ls` 还是 `dir`。
//!
//! 保留的行为（与旧实现一致）：超时、大输出截断、危险命令软警告、Unix 下用进程组
//! 起子进程以便超时时清理整棵进程树。
//!
//! ## 取消
//!
//! 工具自己盯着取消信号 —— `run_turn` 只在流式循环里检查取消，工具执行阶段它什么
//! 都做不了。一个 `sleep 600` 会把「停止」变成摆设。所以这里：
//!
//! * 进入时已经取消 → 一条命令都不执行；
//! * 执行中取消 → 立刻杀掉整个进程组，返回「被中断」的结果。
//!
//! ## 后台执行
//!
//! 模型写 `run_in_background: true`（起服务、跑长构建、`tail -f` 这类命令不该
//! 卡住整轮对话），落到 [`super::background::BackgroundShells`]：立刻返回 `shell_id`，
//! 之后用 `bash_output` 读增量、`kill_shell` 结束。
//!
//! ## `cd` 跨命令保留
//!
//! 每次调用都是新起的 shell，`cd` 在命令结束时就丢了。模型于是每条命令都写一遍
//! `cd /很长/的/绝对路径 && ...`。所以让 `cd` 真的留下来：命令末尾（EXIT trap）把
//! `pwd` 写进临时文件，执行完读回来，下一条命令就在那里起。只在工作目录之内跟随 ——
//! 文件工具的相对路径始终按会话工作目录解析，两边对「当前目录」的理解不能越差越远。

//!
//! ## 单条命令的控制（宿主可选接入）
//!
//! 整轮「停止」之外，宿主可能还想对**一条**正在跑的命令做两件事：中断它（结果回灌
//! 给模型，这一轮继续）、转到后台（工具立刻返回 `shell_id`）；以及在它跑的时候实时
//! 看到输出。这些都是宿主侧的交互，这里只定义一个 [`CommandControl`] 接口与一个
//! 取句柄的 [`ControlLookup`]，由宿主在执行前把句柄放进自己的上下文（通常是
//! task-local）。CLI 不接；GUI 宿主接。

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::CancelToken;
use crate::platform::{ShellKind, detect_shell};

use super::background::BackgroundShells;
use super::{Tool, ToolResult};

const MAX_OUTPUT: usize = 30_000;
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// 单条命令的控制句柄：宿主在界面上那条「运行中…」卡片的两颗按钮与实时输出。
#[async_trait::async_trait]
pub trait CommandControl: Send + Sync {
    /// 等「中断这条命令」被按下。
    async fn interrupted(&self);
    /// 等「转后台」被按下。
    async fn backgrounded(&self);
    /// 有没有人接实时输出。没人接时工具就不攒增量。
    fn streams_output(&self) -> bool;
    /// 推一段实时输出（增量）。
    fn emit_output(&self, text: String);
}

/// 从「当前执行上下文」取控制句柄。宿主实现后交给 [`PlatformBash::with_control`]。
pub type ControlLookup = Arc<dyn Fn() -> Option<Arc<dyn CommandControl>> + Send + Sync>;

/// bash 的当前目录：跨调用保留，像终端一样。
///
/// ## 为什么
///
/// 每次调用都是新起的 shell，`cd` 在命令结束时就丢了。模型于是每条命令都写一遍
/// `cd /很长/的/绝对路径 && ...` —— 卡片上一屏都是同一个前缀。描述里劝它别这么写
/// 治标不治本：它要在子目录里干活，就只能每次都 cd。
///
/// 所以让 `cd` 真的留下来：命令末尾（EXIT trap）把 `pwd` 写进一个临时文件，执行完
/// 读回来，下一条命令就在那里起。进了子目录之后，后面的命令就不用再带 cd。
///
/// ## 只在工作目录之内跟随
///
/// 文件工具（read_file 等）的相对路径始终按会话工作目录解析。bash 要是能漂到任意
/// 地方，两边对「当前目录」的理解会越差越远。所以目录离开工作目录时，下一条命令
/// 回到工作目录，并在结果里明说。
///
/// 只对 POSIX shell 生效：cmd.exe 没有可靠的 EXIT 钩子，那边维持每次从工作目录起。
pub struct ShellCwd {
    root: std::path::PathBuf,
    current: Mutex<std::path::PathBuf>,
}

impl ShellCwd {
    pub fn new(root: std::path::PathBuf) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(root.clone()),
            root,
        })
    }

    /// 下一条命令该在哪起。记下的目录被删了就回到工作目录。
    pub fn current(&self) -> std::path::PathBuf {
        let cur = self.current.lock().unwrap().clone();
        if cur.is_dir() { cur } else { self.root.clone() }
    }

    /// 命令结束时 shell 所在的目录。目录变了就返回一句给模型看的说明。
    fn update(&self, reported: &std::path::Path) -> Option<String> {
        if !reported.is_dir() {
            return None;
        }
        // 比较前都规范化：macOS 的 /tmp 是 /private/tmp 的链接，不规范化的话
        // 同一个目录会被当成「工作目录之外」。
        let canon =
            |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let root = canon(&self.root);
        let new = canon(reported);
        let mut cur = self.current.lock().unwrap();
        if new.starts_with(&root) {
            if canon(&cur) == new {
                return None;
            }
            *cur = reported.to_path_buf();
            Some(format!(
                "[cwd] 当前目录：{}（下一条命令在这里执行）",
                reported.display()
            ))
        } else {
            *cur = self.root.clone();
            Some(format!(
                "[cwd] 命令结束时在工作目录之外（{}），下一条命令回到工作目录 {}",
                reported.display(),
                self.root.display()
            ))
        }
    }
}

/// EXIT trap 的落点文件。每次调用一个，用完删掉。
fn cwd_marker() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "carter-cwd-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

struct RemoveOnDrop(std::path::PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 给命令套上「结束时把 pwd 写进 marker」的外壳。路径里有引号就不套 —— 宁可不跟随，
/// 也不拼出一条引号错乱的命令。
fn with_cwd_trap(command: &str, marker: &std::path::Path) -> Option<String> {
    let path = marker.to_string_lossy().replace('\\', "/");
    if path.contains(['\'', '"', '$', '`']) {
        return None;
    }
    // Git Bash 的 `pwd` 是 `/c/Users/...`，`-W` 才是 Windows 认得的 `C:/Users/...`。
    let pwd = if cfg!(windows) {
        "pwd -W 2>/dev/null || pwd"
    } else {
        "pwd"
    };
    // 换行而不是 `;` 分隔：命令以注释或 heredoc 结尾时也不会把 trap 吞进去。
    Some(format!("trap '{{ {pwd}; }} > \"{path}\"' EXIT\n{command}"))
}

pub struct PlatformBash {
    /// 会话的工作目录，以及 bash 此刻所在的目录。
    cwd: Arc<ShellCwd>,
    /// 用户按「停止」/关会话时置位。见模块头。
    cancel: CancelToken,
    /// 后台命令表。`None` = 这个实例只用来展示（设置页之类的只读视图），不真跑后台。
    shells: Option<Arc<BackgroundShells>>,
    /// 后台日志按会话分目录。
    session_id: String,
    /// 单条命令的控制句柄怎么取。`None` = 不接（CLI）。
    control: Option<ControlLookup>,
}

impl PlatformBash {
    /// `cwd` 由宿主持有、每轮重建工具时传进来 —— 当前目录要跨轮保留。
    pub fn new(
        cwd: Arc<ShellCwd>,
        cancel: CancelToken,
        shells: Option<Arc<BackgroundShells>>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            cwd,
            cancel,
            shells,
            session_id: session_id.into(),
            control: None,
        }
    }

    /// 接上宿主的单条命令控制（见模块头）。
    pub fn with_control(mut self, lookup: ControlLookup) -> Self {
        self.control = Some(lookup);
        self
    }
}

#[async_trait::async_trait]
impl Tool for PlatformBash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        crate::platform::bash_tool_description_leaked()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的命令" },
                "timeout_secs": { "type": "integer", "description": "超时秒数（缺省 120，仅前台执行有效）" },
                "run_in_background": {
                    "type": "boolean",
                    "description": "后台执行，立刻返回 shell_id。适合起服务、跑长构建、tail -f 这类\
                                    不想卡住整轮对话的命令。命令结束时会自动通知你，不用轮询；\
                                    中途想看输出用 bash_output，要提前结束用 kill_shell。"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return ToolResult::err("missing or non-string argument: command");
        };
        let timeout = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let background = args
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !self.cwd.root.is_dir() {
            return ToolResult::err(format!("工作目录不存在：{}", self.cwd.root.display()));
        }
        let here = self.cwd.current();
        let shell = detect_shell();

        // 进入工具时这一轮已经被取消了：一条命令都别跑。
        if self.cancel.is_cancelled() {
            return ToolResult::err("这一轮已被用户中断，命令没有执行。");
        }

        if background {
            let Some(shells) = &self.shells else {
                return ToolResult::err("这个会话不支持后台命令。");
            };
            return match shells.spawn(command, &here, &shell, &self.session_id) {
                Ok((id, log)) => ToolResult::ok(format!(
                    "命令已在后台启动。\n\
                     shell_id={id}\n\
                     日志：{}\n\
                     {}",
                    log.display(),
                    super::background::follow_up(&id)
                )),
                Err(e) => ToolResult::err(e),
            };
        }

        let warning = sniff_dangerous(command);

        // 跟随 `cd`：命令结束时把所在目录写进 marker。
        let marker = matches!(shell.kind, ShellKind::Posix).then(cwd_marker);
        let wrapped = marker.as_deref().and_then(|m| with_cwd_trap(command, m));
        let marker = wrapped.is_some().then_some(marker).flatten();
        let _cleanup = marker.clone().map(RemoveOnDrop);

        let mut cmd = tokio::process::Command::new(&shell.program);
        cmd.arg(shell.flag)
            .arg(wrapped.as_deref().unwrap_or(command))
            .current_dir(&here)
            // 宿主的 stdin 不是终端。不显式给 null 的话，`cat` / `read` 这类命令
            // 会一直等着一个永远不会来的输入。
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        crate::platform::hide_console_async(&mut cmd);
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::err(format!(
                    "启动 shell 失败：{e}\n（探测到的 shell：{} = {}）",
                    shell.label(),
                    shell.program.display()
                ));
            }
        };
        let kill_pid = child.id();

        // 两条管道各交给一个读取任务。**不能等命令结束再收输出**：
        // 一是管道写满会把子进程卡死；二是「转后台」之后还得继续往日志里写。
        let live = Arc::new(LiveOutput::default());
        // 单条命令的控制（宿主卡片上的「中断命令 / 转后台」与实时输出）。
        let control = self.control.as_ref().and_then(|f| f());
        // 实时输出：有人看才攒增量，按节拍推给宿主。这次调用一返回推送任务就随
        // guard 一起停掉 —— 之后由结果正文接管。
        let _streamer = control
            .as_ref()
            .filter(|c| c.streams_output())
            .map(|c| spawn_streamer(live.clone(), c.clone()));
        let mut readers = Some([
            spawn_reader(child.stdout.take(), live.clone(), Which::Stdout),
            spawn_reader(child.stderr.take(), live.clone(), Which::Stderr),
        ]);

        // 子进程退出由一个监视任务统一处理：转了后台之后这次调用已经返回，
        // 但后台表里的状态还得继续更新。
        let exit = Arc::new(Mutex::new(ExitSlot::default()));
        let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();
        {
            let slot = exit.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                let code = status.as_ref().ok().and_then(|s| s.code());
                slot.lock().unwrap().finish(code);
                let _ = exit_tx.send(status);
            });
        }

        let wait_interrupt = async {
            match control.as_ref() {
                Some(c) => c.interrupted().await,
                None => std::future::pending().await,
            }
        };
        let wait_background = async {
            match control.as_ref() {
                Some(c) => c.backgrounded().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(wait_interrupt);
        tokio::pin!(wait_background);
        let deadline = tokio::time::sleep(Duration::from_secs(timeout));
        tokio::pin!(deadline);

        enum How {
            Done(std::io::Result<std::process::ExitStatus>),
            Stop,
            Interrupt,
            Background,
            Timeout,
        }

        let how = tokio::select! {
            biased;
            // 整轮停止 / 关会话优先于单条命令上的操作。
            _ = self.cancel.cancelled() => How::Stop,
            _ = &mut wait_interrupt => How::Interrupt,
            _ = &mut wait_background => How::Background,
            r = &mut exit_rx => How::Done(match r {
                Ok(st) => st,
                Err(_) => return ToolResult::err("执行出错：等待子进程退出时通道断了"),
            }),
            _ = &mut deadline => How::Timeout,
        };

        let status = match how {
            How::Done(st) => match st {
                Ok(s) => s,
                Err(e) => return ToolResult::err(format!("执行出错：{e}")),
            },
            How::Stop => {
                kill_if_running(kill_pid);
                drain(readers.take()).await;
                return ToolResult::err(
                    "命令被用户中断了（进程树已清理）。需要长时间运行的命令请用 \
                     run_in_background: true。",
                );
            }
            How::Interrupt => {
                // 只结束**这一条**命令，不结束这一轮：结果回灌给模型，
                // 让它自己决定换种做法还是先问用户。
                kill_if_running(kill_pid);
                drain(readers.take()).await;
                let (out, err) = live.snapshot();
                let tail = merge_streams(&out, &err);
                return ToolResult::err(if tail.trim().is_empty() {
                    "命令被用户中断了（进程树已清理）。".to_string()
                } else {
                    format!(
                        "命令被用户中断了（进程树已清理）。中断前已产生的输出：\n{}",
                        truncate(&tail, MAX_OUTPUT)
                    )
                });
            }
            How::Timeout => {
                kill_if_running(kill_pid);
                drain(readers.take()).await;
                return ToolResult::err(format!("命令超过 {timeout}s 未结束（进程树已清理）"));
            }
            How::Background => {
                // 读取任务**不 join**：它们继续把输出写进日志。调用在这里返回，
                // `readers` 随之被 drop —— tokio 的 JoinHandle 是 detach 语义，任务照跑。
                return self.hand_over_to_background(command, &live, &exit, kill_pid);
            }
        };
        let code = status.code().unwrap_or(-1);

        // 子进程已退出：等两条管道读到 EOF，输出才完整。
        drain(readers.take()).await;
        let (stdout_b, stderr_b) = live.snapshot();
        let stdout = decode(&stdout_b);
        let stderr = decode(&stderr_b);

        let mut body = String::new();
        if let Some(w) = warning {
            body.push_str(&format!("[warning] {w}\n"));
        }
        if !stdout.is_empty() {
            body.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str("[stderr]\n");
            body.push_str(&stderr);
        }
        if body.is_empty() {
            body.push_str("(命令成功，没有输出)");
        }
        if let Some(note) = marker.as_deref().and_then(|m| self.read_marker(m)) {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&note);
        }

        let report = format!("exit_code={code}\n{}", truncate(&body, MAX_OUTPUT));
        if status.success() {
            ToolResult::ok(report)
        } else {
            ToolResult::err(report)
        }
    }
}

impl PlatformBash {
    /// 把一条前台命令交给后台表，并立刻返回 `shell_id`。
    fn hand_over_to_background(
        &self,
        command: &str,
        live: &LiveOutput,
        exit: &Mutex<ExitSlot>,
        pid: Option<u32>,
    ) -> ToolResult {
        let Some(shells) = &self.shells else {
            return ToolResult::err("这个会话不支持后台命令。");
        };
        match shells.adopt(command, &self.session_id, pid) {
            Ok(mut adopted) => {
                if let Err(e) = live.start_logging(&mut adopted.file) {
                    return ToolResult::err(format!("转到后台失败：{e}"));
                }
                let (id, log) = (adopted.id.clone(), adopted.log.clone());
                // 进程可能已经在这几步之间退出了 —— `attach` 会把这个状态补上。
                exit.lock().unwrap().attach(adopted);
                ToolResult::ok(format!(
                    "命令已转到后台继续运行。\n\
                     shell_id={id}\n\
                     日志：{}\n\
                     {}",
                    log.display(),
                    super::background::follow_up(&id)
                ))
            }
            Err(e) => ToolResult::err(e),
        }
    }

    /// 读回命令结束时所在的目录并更新 [`ShellCwd`]。目录变了才返回说明。
    fn read_marker(&self, marker: &std::path::Path) -> Option<String> {
        let raw = std::fs::read_to_string(marker).ok();
        let _ = std::fs::remove_file(marker);
        let dir = raw?.trim_end_matches(['\n', '\r']).to_string();
        if dir.is_empty() {
            return None;
        }
        self.cwd.update(std::path::Path::new(&dir))
    }
}

/// 前台命令的哪一路输出。
#[derive(Clone, Copy)]
enum Which {
    Stdout,
    Stderr,
}

/// 前台命令已经收到的输出。转到后台之后，同一份数据会继续写进日志文件。
#[derive(Default)]
struct LiveOutput {
    inner: Mutex<Live>,
}

#[derive(Default)]
struct Live {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// 转到后台之后才有值。有它之后就不再攒内存了 —— 一条跑几个小时的后台
    /// 服务不该把全部输出留在进程里。
    log: Option<std::fs::File>,
    /// 还没推给宿主的输出，两路按到达顺序混在一起（终端里看到的就是这个顺序）。
    /// 只在有人接实时输出时才攒，见 [`spawn_streamer`]。
    pending: Option<Vec<u8>>,
}

impl LiveOutput {
    fn push(&self, which: Which, bytes: &[u8]) {
        use std::io::Write as _;
        let mut g = self.inner.lock().unwrap();
        if let Some(f) = g.log.as_mut() {
            let _ = f.write_all(bytes);
            return;
        }
        if let Some(p) = g.pending.as_mut() {
            p.extend_from_slice(bytes);
        }
        match which {
            Which::Stdout => g.stdout.extend_from_slice(bytes),
            Which::Stderr => g.stderr.extend_from_slice(bytes),
        }
    }

    /// 打开实时输出的攒批。
    fn enable_streaming(&self) {
        self.inner
            .lock()
            .unwrap()
            .pending
            .get_or_insert_with(Vec::new);
    }

    /// 取走一批还没推的输出，解成文本。
    ///
    /// 结尾半个 UTF-8 字符留到下一批 —— 管道按字节切块，一个汉字可能被劈成两半，
    /// 各自 lossy 解出来就是两个 `�`。
    fn take_pending(&self) -> String {
        let mut g = self.inner.lock().unwrap();
        let Some(p) = g.pending.as_mut() else {
            return String::new();
        };
        if p.is_empty() {
            return String::new();
        }
        let keep = match std::str::from_utf8(p) {
            Ok(_) => 0,
            // 只是结尾不完整（`error_len() == None`）：留下那几个字节。
            Err(e) if e.error_len().is_none() => p.len() - e.valid_up_to(),
            // 真的不是 UTF-8（Windows 的 GBK 输出等）：整批交给 `decode`。
            Err(_) => 0,
        };
        let rest = p.split_off(p.len() - keep);
        let chunk = std::mem::replace(p, rest);
        decode(&chunk)
    }

    fn snapshot(&self) -> (Vec<u8>, Vec<u8>) {
        let g = self.inner.lock().unwrap();
        (g.stdout.clone(), g.stderr.clone())
    }

    /// 把已经攒下的输出写进日志，并从现在起把新输出也写进去。
    fn start_logging(&self, file: &mut std::fs::File) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut g = self.inner.lock().unwrap();
        if !g.stdout.is_empty() {
            file.write_all(&g.stdout)?;
            if !g.stdout.ends_with(b"\n") {
                file.write_all(b"\n")?;
            }
        }
        if !g.stderr.is_empty() {
            file.write_all(&g.stderr)?;
        }
        g.log = Some(file.try_clone()?);
        Ok(())
    }
}

/// 子进程退出状态。转后台之后，这条记录会被 `ExitSlot.target` 拿去更新后台表。
#[derive(Default)]
struct ExitSlot {
    code: Option<i32>,
    target: Option<super::background::AdoptedShell>,
}

impl ExitSlot {
    fn finish(&mut self, code: Option<i32>) {
        self.code = code;
        if let Some(t) = &self.target {
            t.mark_exited(code.unwrap_or(-1));
        }
    }

    fn attach(&mut self, shell: super::background::AdoptedShell) {
        if let Some(code) = self.code {
            shell.mark_exited(code);
        }
        self.target = Some(shell);
    }
}

/// 实时输出的推送节拍。太密了宿主每帧都在重排，太疏了看着一顿一顿的。
const STREAM_TICK: Duration = Duration::from_millis(150);

/// 推送任务的句柄：drop 即停。
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 起一个推送任务：每隔 [`STREAM_TICK`] 把新到的输出作为增量推给宿主。
fn spawn_streamer(live: Arc<LiveOutput>, control: Arc<dyn CommandControl>) -> AbortOnDrop {
    live.enable_streaming();
    AbortOnDrop(tokio::spawn(async move {
        let mut tick = tokio::time::interval(STREAM_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            control.emit_output(live.take_pending());
        }
    }))
}

/// 中断时给模型看的一段现场：stdout 与 stderr 按区段拼起来。
fn merge_streams(stdout: &[u8], stderr: &[u8]) -> String {
    let mut s = decode(stdout);
    let e = decode(stderr);
    if !e.is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&e);
    }
    s
}

/// 起一个读取任务，把管道里的字节喂给 [`LiveOutput`]。
fn spawn_reader<R>(
    stream: Option<R>,
    live: Arc<LiveOutput>,
    which: Which,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt as _;
    tokio::spawn(async move {
        let Some(mut stream) = stream else { return };
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => live.push(which, &buf[..n]),
            }
        }
    })
}

/// 等读取任务把已产出的输出收完。带时限 —— 被杀的进程可能留下一个不关管道的
/// 孙进程，不能让它把整轮对话吊住。
async fn drain(readers: Option<[tokio::task::JoinHandle<()>; 2]>) {
    let Some(readers) = readers else { return };
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        for h in readers {
            let _ = h.await;
        }
    })
    .await;
}

fn kill_if_running(pid: Option<u32>) {
    if let Some(pid) = pid {
        super::background::kill_process_tree(pid);
    }
}

/// 解 stdout / stderr 的字节。
///
/// 先按 UTF-8 试。不是 UTF-8 的话，Windows 上按 **OEM 代码页**解 —— 中文环境下
/// `dir`、`ipconfig` 这类原生命令输出的是 GBK(936)，直接 `from_utf8_lossy` 会得到
/// 一片 `�`，模型和用户都读不出任何东西。
fn decode(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    {
        if let Some(s) = decode_oem(bytes) {
            return s;
        }
    }
    String::from_utf8_lossy(bytes).to_string()
}

/// 按系统 OEM 代码页解码。解不出来返回 None，让调用方回落 lossy。
#[cfg(windows)]
fn decode_oem(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};

    if bytes.is_empty() {
        return Some(String::new());
    }
    let cp = unsafe { GetOEMCP() };
    let src_len = i32::try_from(bytes.len()).ok()?;
    let need =
        unsafe { MultiByteToWideChar(cp, 0, bytes.as_ptr(), src_len, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return None;
    }
    let mut buf = vec![0u16; need as usize];
    let got =
        unsafe { MultiByteToWideChar(cp, 0, bytes.as_ptr(), src_len, buf.as_mut_ptr(), need) };
    if got <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..got as usize]))
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... [截断 {} 字节]", &s[..end], s.len() - end)
}

/// 危险命令软警告。不阻断 —— 让模型在结果里看到（错误即数据）。
fn sniff_dangerous(cmd: &str) -> Option<&'static str> {
    let lower = cmd.trim().to_ascii_lowercase();
    if (lower.contains("rm -rf") || lower.contains("rm -fr"))
        && (lower.contains(" /")
            || lower.contains(" ~")
            || lower.contains(" *")
            || lower.contains(" .git"))
    {
        return Some("检测到 `rm -rf` 指向敏感目标，请确认结果符合预期");
    }
    if lower.contains("dd ") && lower.contains("of=/dev/") {
        return Some("`dd` 写入 /dev/* 设备不可逆");
    }
    if (lower.starts_with("mkfs") || lower.contains(" mkfs")) && lower.contains("/dev/") {
        return Some("`mkfs` 会格式化块设备，不可逆");
    }
    if lower.starts_with("shutdown") || lower.starts_with("reboot") || lower.starts_with("halt") {
        return Some("系统电源命令，会影响宿主机");
    }
    // Windows 侧的高危操作。
    if lower.contains("format ") && (lower.contains(" /q") || lower.contains("c:")) {
        return Some("`format` 会格式化磁盘，不可逆");
    }
    if lower.contains("rd /s /q") && (lower.contains(" c:\\") || lower.contains(" %systemdrive%")) {
        return Some("`rd /s /q` 指向系统盘，请确认");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_rf_root_warns() {
        assert!(sniff_dangerous("rm -rf /").is_some());
        assert!(sniff_dangerous("rm -rf ~/proj").is_some());
        assert!(sniff_dangerous("rm -rf *.log").is_some());
    }

    #[test]
    fn safe_rm_does_not_warn() {
        assert!(sniff_dangerous("rm a.txt").is_none());
        assert!(sniff_dangerous("rm -r build").is_none());
    }

    #[test]
    fn benign_commands_pass() {
        assert!(sniff_dangerous("ls -la").is_none());
        assert!(sniff_dangerous("cargo build").is_none());
    }

    /// 描述必须写明当前平台与 shell —— 模型只能从描述里知道该写 `ls` 还是 `dir`。
    #[test]
    fn the_description_names_the_platform() {
        let d = crate::platform::bash_tool_description();
        assert!(d.contains(crate::platform::os_label()), "{d}");
    }
}
