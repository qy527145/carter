//! 后台命令：把「跑很久、又不该卡住这一轮」的命令丢到后台，给模型一个句柄。
//!
//! ## 为什么不是「超时了自动转后台」
//!
//! 自动转后台看起来很省事，但模型的请求里**没有**「这个命令会跑很久」这个信息，
//! 它拿到一个超时结果时不会知道去做别的事 —— 于是还是会立刻 `bash_output` 死等，
//! 只是把一次 120 秒的阻塞换成了二十次轮询。真正有用的做法与 Claude Code 一样：
//! 让模型显式地写 `run_in_background: true`（起服务、跑长构建、`tail -f`），
//! 拿到 `shell_id` 之后由它决定什么时候回来看。
//!
//! ## 结束时主动通知，而不是让模型轮询
//!
//! 光有 `shell_id` 还不够：模型不知道命令什么时候结束，只能 `bash_output` 一遍遍去问
//! —— 这一轮照样卡着，和前台跑没有区别。所以命令**自己**结束时，这里往宿主发一条
//! 完成通知（见 [`BackgroundShells::subscribe`]）：宿主在这一轮结束后把它作为一条
//! user 消息追加进历史，再起一轮。模型启动后台命令之后就可以结束这一轮，用户照常聊别的。
//!
//! 不通知的情况：模型自己 `kill_shell` 的、关会话收掉的（前者模型已经知道，后者没人听）；
//! 模型已经用 `bash_output` 亲眼看到它结束了（再报一遍是重复）。
//!
//! **用户**在界面上终止的要通知（[`BackgroundShells::kill_by_user`]），而且话要和「自己
//! 跑完了」分开说：模型还以为它在跑，不告诉它的话，它会一直等一条永远不来的完成通知；
//! 只说「结束了」的话，它会把半截输出当成完整结果。
//!
//! ## 输出怎么拿回来
//!
//! 后台进程的 stdout/stderr 直接重定向到日志文件，`bash_output` 每次从**上次读到的
//! 位置**往后读。不留在管道里是有原因的：管道有内核缓冲上限，没人读就会把子进程
//! 写阻塞住 —— 一个「后台」服务跑一会儿自己卡死，排查起来极其费劲。
//!
//! ## 生命周期
//!
//! 一个宿主一份 [`BackgroundShells`]（CLI 是进程级，GUI 宿主按会话）。宿主退出时
//! [`BackgroundShells::kill_all`] 把它们整组收掉 —— 不留后台孤儿进程。这也意味着
//! 后台命令**不跨宿主进程**：想让它长久活着，那是 systemd / launchd 该管的事。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::{Tool, ToolResult};
use crate::platform::Shell;

/// 单次读回的上限。与 bash 的结果上限一致：够模型看清，又不至于把上下文撑爆。
const MAX_READ: usize = 30_000;

/// 完成通知里最多带多少输出（末尾）。完整的仍在日志里，模型要看可以 `bash_output`。
const NOTICE_TAIL: usize = 4_000;

/// 完成通知送进会话时加在前面的标记。宿主靠它认出「这不是用户说的话」；
/// 模型也靠它认出这是系统通知。
pub const DONE_MARK: &str = "【后台命令】";

/// 起完后台命令之后告诉模型的那句话：重点是「别轮询，结束了会通知你」。
pub fn follow_up(id: &str) -> String {
    format!(
        "命令结束时会自动通知你（以一条 `{DONE_MARK}` 开头的消息），不要用 bash_output 反复轮询。\
         接下来没有别的事要做的话，告诉用户命令在后台跑着、结束一轮即可；\
         中途想看输出用 bash_output(shell_id=\"{id}\")，要提前结束用 kill_shell(shell_id=\"{id}\")。"
    )
}

/// 完成通知的发送端，由宿主订阅。见 [`BackgroundShells::subscribe`]。
type Notifier = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>;

/// 表变化时推给宿主的回调（整表快照，只含还在跑的）。见 [`BackgroundShells::set_observer`]。
type Observer = Arc<dyn Fn(Vec<RunningShell>) + Send + Sync>;

/// 还在跑的一条后台命令（宿主画提醒条用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningShell {
    pub id: String,
    pub command: String,
    /// 开始的时刻（epoch 毫秒）。
    pub started_at: u64,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShellStatus {
    Running,
    /// 自己结束的，带退出码。
    Exited(i32),
    /// 被 `kill_shell` / 宿主关闭收掉的。
    Killed,
    /// 用户在界面上手动终止的。与 `Killed` 分开：这一种要通知模型。
    KilledByUser,
}

impl ShellStatus {
    pub fn as_str(&self) -> String {
        match self {
            ShellStatus::Running => "running".to_string(),
            ShellStatus::Exited(code) => format!("exited(code={code})"),
            ShellStatus::Killed => "killed".to_string(),
            ShellStatus::KilledByUser => "killed(by user)".to_string(),
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, ShellStatus::Running)
    }
}

struct ShellState {
    command: String,
    log: PathBuf,
    /// 下次 `bash_output` 从哪个字节开始读。
    offset: u64,
    status: ShellStatus,
    /// 进程组 id（Unix 上就是子进程 pid，因为 setsid 之后它自成一个组）。
    pid: Option<u32>,
    /// 模型已经知道它结束了（`bash_output` 读到过结束状态，或完成通知已经送过）。
    told: bool,
    /// 起它的时刻（epoch 毫秒）。
    started_at: u64,
    /// 怎么进的后台。完成通知里要说清楚（模型自己起的 / 用户转的 / 超时转的）。
    origin: Origin,
}

/// 一条后台命令怎么来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// 模型写了 `run_in_background: true`。
    Model,
    /// 先在前台跑，用户按了「转后台」。
    User,
    /// 先在前台跑，超时后按设置自动转的。
    Timeout,
}

impl Origin {
    fn phrase(self) -> &'static str {
        match self {
            Origin::Model => "你之前在后台启动的命令",
            Origin::User => "用户之前手动转到后台的命令",
            Origin::Timeout => "之前因前台超时自动转到后台的命令",
        }
    }
}

/// 接管过来的一条命令（前台转后台）。见 [`BackgroundShells::adopt`]。
pub struct AdoptedShell {
    pub id: String,
    pub log: PathBuf,
    /// 继续往日志里追加输出用的句柄（读取任务持有它）。
    pub file: std::fs::File,
    state: Arc<Mutex<ShellState>>,
    owner: Arc<BackgroundShells>,
}

impl AdoptedShell {
    /// 进程退出了，把状态写回后台表。
    pub fn mark_exited(&self, code: i32) {
        self.owner.finish(&self.state, &self.id, code);
    }
}

/// 一条会话的后台命令表。
pub struct BackgroundShells {
    shells: Mutex<HashMap<String, Arc<Mutex<ShellState>>>>,
    seq: AtomicU64,
    notify: Notifier,
    /// 表有变化时推给宿主。见 [`BackgroundShells::set_observer`]。
    observer: Mutex<Option<Observer>>,
    /// 日志根目录。生产是 `~/.carter/shells`；测试给一个自己的临时目录。
    root: PathBuf,
}

impl BackgroundShells {
    pub fn new() -> Arc<Self> {
        Self::at(crate::config::paths::carter_home().join("shells"))
    }

    pub fn at(root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            shells: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            notify: Arc::default(),
            observer: Mutex::new(None),
            root,
        })
    }

    /// 订阅表的变化（启动、转后台、结束、被结束）。一个宿主订阅一次，再订阅会顶掉
    /// 前一个。收到的是**整表快照**，见 [`BackgroundShells::running`]。
    pub fn set_observer(&self, observer: Observer) {
        *self.observer.lock().unwrap() = Some(observer);
    }

    /// 把当前快照推给宿主。**每一处改了「还在跑的那些」的地方都要调** ——
    /// 少一次，提醒条就停在上一份状态上。
    fn publish(&self) {
        let observer = self.observer.lock().unwrap().clone();
        if let Some(f) = observer {
            f(self.running());
        }
    }

    /// 订阅完成通知：命令自己结束时收到它的 `shell_id`。一个宿主订阅一次，
    /// 再订阅会顶掉前一个。收到之后用 [`BackgroundShells::completion`] 取通知正文。
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.notify.lock().unwrap() = Some(tx);
        rx
    }

    /// 还在跑的命令快照，按开始时间排序。
    pub fn running(&self) -> Vec<RunningShell> {
        let mut out: Vec<RunningShell> = self
            .shells
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, state)| {
                let s = state.lock().unwrap();
                s.status.is_running().then(|| RunningShell {
                    id: id.clone(),
                    command: s.command.clone(),
                    started_at: s.started_at,
                    origin: s.origin,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// 命令自己结束：记下退出码、唤醒宿主。被 kill 过的不要被「进程确实退出了」
    /// 覆盖成 Exited —— 「我杀掉了它」和「它自己跑完了」是两件事，也不必通知。
    fn finish(&self, state: &Mutex<ShellState>, id: &str, code: i32) {
        {
            let mut s = state.lock().unwrap();
            if !s.status.is_running() {
                return;
            }
            s.status = ShellStatus::Exited(code);
        }
        // 先推宿主再唤醒：唤醒之后紧接着就是新一轮，提醒条不该在那之后还挂着。
        self.publish();
        if let Some(tx) = self.notify.lock().unwrap().as_ref() {
            let _ = tx.send(id.to_string());
        }
    }

    fn log_path(&self, session_id: &str, id: &str) -> PathBuf {
        self.root.join(session_id).join(format!("{id}.log"))
    }

    /// 起一条后台命令。返回 `(shell_id, 日志路径)`。
    pub fn spawn(
        self: &Arc<Self>,
        command: &str,
        cwd: &std::path::Path,
        shell: &Shell,
        session_id: &str,
    ) -> Result<(String, PathBuf), String> {
        let id = format!("sh{}", self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let log = self.log_path(session_id, &id);
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建后台命令的日志目录失败：{e}"))?;
        }
        let out = std::fs::File::create(&log).map_err(|e| format!("建日志文件失败：{e}"))?;
        let err = out
            .try_clone()
            .map_err(|e| format!("复制日志句柄失败：{e}"))?;

        let mut cmd = tokio::process::Command::new(&shell.program);
        cmd.arg(shell.flag)
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        #[cfg(windows)]
        crate::platform::hide_console_async(&mut cmd);
        // 独立进程组：`kill_shell` 与宿主退出时能一次收掉整棵树。
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动后台命令失败：{e}（shell：{}）", shell.label()))?;
        let pid = child.id();

        let state = Arc::new(Mutex::new(ShellState {
            command: command.to_string(),
            log: log.clone(),
            offset: 0,
            status: ShellStatus::Running,
            pid,
            told: false,
            started_at: crate::session::now_ms(),
            origin: Origin::Model,
        }));
        // 先进表、推快照，再挂等待退出的任务：短命令可能在挂任务之前就结束了，
        // 反过来写的话两份快照的顺序会飘（先「没了」再「有一条」）。
        self.shells
            .lock()
            .unwrap()
            .insert(id.clone(), state.clone());
        self.publish();
        let watcher = state;
        let (watch_id, owner) = (id.clone(), self.clone());
        tokio::spawn(async move {
            let code = match child.wait().await {
                Ok(st) => st.code().unwrap_or(-1),
                Err(_) => -1,
            };
            owner.finish(&watcher, &watch_id, code);
        });
        Ok((id, log))
    }

    /// 接管一条**已经在跑**的命令（前台转后台）。
    ///
    /// 与 [`BackgroundShells::spawn`] 的区别只有一个：进程不是这里起的，输出也不在
    /// 日志文件里 —— 调用方把已经攒下的输出写进返回的 `file`，之后继续往里追加。
    /// 退出状态由调用方的监视任务回填（`AdoptedShell::mark_exited`）。
    pub fn adopt(
        self: &Arc<Self>,
        command: &str,
        session_id: &str,
        pid: Option<u32>,
        origin: Origin,
    ) -> Result<AdoptedShell, String> {
        use std::io::Write as _;

        let id = format!("sh{}", self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let log = self.log_path(session_id, &id);
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建后台命令的日志目录失败：{e}"))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| format!("建日志文件失败：{e}"))?;
        // 日志开头写清楚它是哪条命令 —— 之后 `bash_output` 只给模型看增量，
        // 没有这一行它就分不清这是哪条命令的输出。
        let why = if origin == Origin::Timeout {
            "超时后自动转到后台"
        } else {
            "已转到后台"
        };
        let _ = writeln!(file, "（这条命令先在前台跑，{why}）\n$ {command}");

        let state = Arc::new(Mutex::new(ShellState {
            command: command.to_string(),
            log: log.clone(),
            offset: 0,
            status: ShellStatus::Running,
            pid,
            told: false,
            started_at: crate::session::now_ms(),
            origin,
        }));
        self.shells
            .lock()
            .unwrap()
            .insert(id.clone(), state.clone());
        self.publish();
        Ok(AdoptedShell {
            id,
            log,
            file,
            state,
            owner: self.clone(),
        })
    }

    /// 从上次读到的位置往后读。没有新输出时返回空串。
    pub fn read(&self, id: &str) -> Result<(String, ShellStatus), String> {
        let state = self
            .shells
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("没有这个后台命令：{id}（可能已经结束并被清理）"))?;
        let mut s = state.lock().unwrap();
        let mut file = std::fs::File::open(&s.log)
            .map_err(|e| format!("读不到日志文件 {}：{e}", s.log.display()))?;
        file.seek(SeekFrom::Start(s.offset))
            .map_err(|e| format!("定位日志失败：{e}"))?;
        let mut buf = Vec::new();
        file.take(MAX_READ as u64)
            .read_to_end(&mut buf)
            .map_err(|e| format!("读日志失败：{e}"))?;
        // 别把一个多字节字符切成两半：只推进到最后一个完整字符处。
        let boundary = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(e) => e.valid_up_to(),
        };
        let text = String::from_utf8_lossy(&buf[..boundary]).to_string();
        s.offset += boundary as u64;
        // 模型亲眼看到它结束了 —— 不必再发完成通知。
        if !s.status.is_running() {
            s.told = true;
        }
        Ok((text, s.status.clone()))
    }

    /// 一条命令的完成通知正文。`None` = 不用通知（模型已经知道了、模型自己 kill 的、
    /// 还在跑的、或者不认识这个 id）。
    ///
    /// 两种结束分开说：**自己跑完**（带退出码）与**被用户终止**（输出是半截的）。
    pub fn completion(&self, id: &str) -> Option<String> {
        let state = self.shells.lock().unwrap().get(id).cloned()?;
        let mut s = state.lock().unwrap();
        let (headline, guide) = match s.status {
            ShellStatus::Exited(code) => (
                format!("已结束（退出码 {code}）"),
                format!(
                    "这是系统通知，不是用户说的话：{}自己跑完了。\
                     据此接着做原来的事，或者把结果告诉用户。",
                    s.origin.phrase()
                ),
            ),
            ShellStatus::KilledByUser => (
                "已被用户终止".to_string(),
                format!(
                    "这是系统通知：{}没有跑完，是**用户在界面上手动终止**的（不是它自己失败，\
                     也不是超时）。下面的输出是半截的，不要当成完整结果。\
                     不要自作主张重跑它；先简短确认一下，问用户接下来想怎么做。",
                    s.origin.phrase()
                ),
            ),
            _ => return None,
        };
        if std::mem::replace(&mut s.told, true) {
            return None;
        }
        let mut buf = Vec::new();
        if let Ok(mut f) = std::fs::File::open(&s.log) {
            let _ = f.seek(SeekFrom::Start(s.offset));
            let _ = f.read_to_end(&mut buf);
        }
        s.offset += buf.len() as u64;
        let all = String::from_utf8_lossy(&buf);
        let all = all.trim_end();
        let tail = if all.len() > NOTICE_TAIL {
            let mut cut = all.len() - NOTICE_TAIL;
            while !all.is_char_boundary(cut) {
                cut += 1;
            }
            format!(
                "（前面还有 {} 字节，完整输出见日志）\n…{}",
                cut,
                &all[cut..]
            )
        } else if all.is_empty() {
            "（没有新输出）".to_string()
        } else {
            all.to_string()
        };
        Some(format!(
            "{DONE_MARK}{id} · {headline}\n{guide}\n\
             $ {}\n日志：{}\n--- 未读输出 ---\n{tail}",
            s.command,
            s.log.display()
        ))
    }

    pub fn status(&self, id: &str) -> Option<ShellStatus> {
        self.shells
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.lock().unwrap().status.clone())
    }

    /// 结束一条后台命令（整棵进程树）。模型的 `kill_shell` 与宿主收尾走这里，不通知。
    pub fn kill(&self, id: &str) -> Result<ShellStatus, String> {
        self.kill_as(id, ShellStatus::Killed).map(|(s, _)| s)
    }

    /// 用户在界面上终止一条后台命令。与 [`BackgroundShells::kill`] 的区别：模型并不知道，
    /// 所以和「自己跑完了」一样送一条通知（正文说清是用户终止的，见 [`BackgroundShells::completion`]）。
    pub fn kill_by_user(&self, id: &str) -> Result<ShellStatus, String> {
        let (status, now) = self.kill_as(id, ShellStatus::KilledByUser)?;
        // 只有**这一下**真的结束了它才通知：已经结束的再按一次，不该再报一遍。
        if now && let Some(tx) = self.notify.lock().unwrap().as_ref() {
            let _ = tx.send(id.to_string());
        }
        Ok(status)
    }

    /// 返回 `(状态, 是不是这一下结束的)`。
    fn kill_as(&self, id: &str, as_status: ShellStatus) -> Result<(ShellStatus, bool), String> {
        let state = self
            .shells
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("没有这个后台命令：{id}"))?;
        {
            let mut s = state.lock().unwrap();
            if !s.status.is_running() {
                return Ok((s.status.clone(), false));
            }
            if let Some(pid) = s.pid {
                kill_process_tree(pid);
            }
            s.status = as_status.clone();
        }
        // 被结束也是「不在跑了」：提醒条要跟着少一条。
        self.publish();
        Ok((as_status, true))
    }

    /// 宿主退出时把还活着的全部收掉。
    pub fn kill_all(&self) {
        let ids: Vec<String> = self.shells.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let _ = self.kill(&id);
        }
    }

    /// 还活着的命令数（测试与诊断用）。
    pub fn running_count(&self) -> usize {
        self.running().len()
    }
}

/// 杀掉一整棵进程树。
///
/// Unix 用进程组（`setsid` 之后子进程自成一个组）；Windows 用 `taskkill /T`，
/// 那是系统自带的、能递归杀子树的东西。
pub fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        // 负号 = 整个进程组。
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        // 给进程一次体面退出的机会（关连接、删临时文件），然后强杀。
        std::thread::sleep(Duration::from_millis(150));
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::platform::hide_console(&mut cmd);
        let _ = cmd.status();
    }
}

// ---------------------------------------------------------------------------
// 两个工具
// ---------------------------------------------------------------------------

/// `bash_output`：读后台命令**自上次读之后**的新输出。
pub struct BashOutputTool {
    shells: Arc<BackgroundShells>,
}

impl BashOutputTool {
    pub fn new(shells: Arc<BackgroundShells>) -> Self {
        Self { shells }
    }
}

#[async_trait::async_trait]
impl Tool for BashOutputTool {
    fn name(&self) -> &str {
        "bash_output"
    }

    fn description(&self) -> &str {
        "读取后台命令（`run_in_background: true` 的命令）的输出，\
         每次只返回**上次之后的新内容**，立刻返回、不等待。\
         不要用它反复轮询等命令结束 —— 命令结束时会自动通知你。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "shell_id": {
                    "type": "string",
                    "description": "后台命令的 id（`bash` 返回的 shell_id）"
                }
            },
            "required": ["shell_id"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("shell_id").and_then(Value::as_str) else {
            return ToolResult::err("缺少 shell_id");
        };
        let (text, status) = match self.shells.read(id) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        let body = match (text.is_empty(), status.is_running()) {
            (false, _) => text,
            (true, true) => "（还没有新输出，命令仍在运行；结束时会自动通知你）".to_string(),
            (true, false) => "（没有新输出）".to_string(),
        };
        ToolResult::ok(format!(
            "shell_id={id}\nstatus={}\n--- 新输出 ---\n{body}",
            status.as_str()
        ))
    }
}

/// `kill_shell`：结束一条后台命令。
pub struct KillShellTool {
    shells: Arc<BackgroundShells>,
}

impl KillShellTool {
    pub fn new(shells: Arc<BackgroundShells>) -> Self {
        Self { shells }
    }
}

#[async_trait::async_trait]
impl Tool for KillShellTool {
    fn name(&self) -> &str {
        "kill_shell"
    }

    fn description(&self) -> &str {
        "结束一条用 `run_in_background: true` 启动的后台命令（连同它的子进程）。\
         已经结束的命令会原样返回它的状态。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "shell_id": { "type": "string", "description": "要结束的后台命令 id" }
            },
            "required": ["shell_id"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("shell_id").and_then(Value::as_str) else {
            return ToolResult::err("缺少 shell_id");
        };
        match self.shells.kill(id) {
            Ok(status) => ToolResult::ok(format!("shell_id={id}\nstatus={}", status.as_str())),
            Err(e) => ToolResult::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn sh() -> Shell {
        crate::platform::detect_shell()
    }

    /// 每个测试一个独立目录：日志是真实文件，不能落在共享的临时目录里。
    fn shells() -> Arc<BackgroundShells> {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "carter-bg-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        BackgroundShells::at(dir)
    }

    #[tokio::test]
    async fn a_background_command_reports_output_and_exit() {
        let shells = shells();
        let (id, log) = shells
            .spawn(
                "echo hello-bg",
                &std::env::temp_dir(),
                &sh(),
                "test-session",
            )
            .unwrap();
        assert!(log.exists());
        for _ in 0..50 {
            if shells.status(&id).is_some_and(|s| !s.is_running()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (text, status) = shells.read(&id).unwrap();
        assert!(matches!(status, ShellStatus::Exited(0)), "{status:?}");
        assert!(text.contains("hello-bg"), "{text}");
    }

    /// 读是**增量**的：第二次读不该把第一次的内容再返回一遍。
    #[tokio::test]
    async fn reading_advances_past_what_was_already_delivered() {
        let shells = shells();
        let (id, _) = shells
            .spawn("echo first", &std::env::temp_dir(), &sh(), "test-session")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let (first, _) = shells.read(&id).unwrap();
        assert!(first.contains("first"));
        let (second, _) = shells.read(&id).unwrap();
        assert!(!second.contains("first"), "第二次不该重复：{second:?}");
    }

    #[tokio::test]
    async fn killing_a_running_shell_marks_it_killed() {
        let shells = shells();
        let (id, _) = shells
            .spawn("sleep 30", &std::env::temp_dir(), &sh(), "test-session")
            .unwrap();
        assert!(shells.running_count() >= 1);
        let st = shells.kill(&id).unwrap();
        assert_eq!(st, ShellStatus::Killed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(shells.running_count(), 0);
        assert!(shells.read(&id).is_ok(), "日志仍然读得到");
    }

    /// 命令自己结束 → 宿主收到通知，正文带命令与未读输出；只报一次。
    #[tokio::test]
    async fn a_finished_command_notifies_once_with_its_output() {
        let shells = shells();
        let mut rx = shells.subscribe();
        let (id, _) = shells
            .spawn(
                "echo done-out; exit 3",
                &std::env::temp_dir(),
                &sh(),
                "test-session",
            )
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("结束时要通知")
            .unwrap();
        assert_eq!(got, id);
        let text = shells.completion(&id).expect("第一次要有正文");
        assert!(text.starts_with(DONE_MARK), "{text}");
        assert!(text.contains("退出码 3"), "{text}");
        assert!(text.contains("done-out"), "{text}");
        assert!(shells.completion(&id).is_none(), "不该报第二遍");
        // 通知里给过的输出，bash_output 不再重复。
        let (rest, _) = shells.read(&id).unwrap();
        assert!(!rest.contains("done-out"), "{rest:?}");
    }

    /// 模型已经用 bash_output 看到它结束了 → 不再通知；被 kill 的也不通知。
    #[tokio::test]
    async fn no_notice_when_already_seen_or_killed() {
        let shells = shells();
        let mut rx = shells.subscribe();
        let (seen, _) = shells
            .spawn("echo x", &std::env::temp_dir(), &sh(), "test-session")
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        let _ = shells.read(&seen).unwrap();
        assert!(shells.completion(&seen).is_none());

        let (killed, _) = shells
            .spawn("sleep 30", &std::env::temp_dir(), &sh(), "test-session")
            .unwrap();
        shells.kill(&killed).unwrap();
        let none = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(none.is_err(), "被 kill 的不该通知：{none:?}");
        assert!(shells.completion(&killed).is_none());
    }

    /// 用户在界面上终止：要通知，而且话和「自己跑完了」不一样。
    #[tokio::test]
    async fn a_user_kill_is_announced_differently_from_a_normal_exit() {
        let shells = shells();
        let mut rx = shells.subscribe();
        let (id, _) = shells
            .spawn(
                "echo half; sleep 30",
                &std::env::temp_dir(),
                &sh(),
                "test-session",
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(shells.kill_by_user(&id).unwrap(), ShellStatus::KilledByUser);
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert_eq!(got.ok().flatten().as_deref(), Some(id.as_str()));
        let text = shells.completion(&id).expect("用户终止要通知");
        assert!(text.contains("已被用户终止"), "{text}");
        assert!(text.contains("不要当成完整结果"), "{text}");
        assert!(!text.contains("退出码"), "{text}");
        assert!(shells.completion(&id).is_none(), "只通知一次");
        // 已经结束的再按一次：原样返回，不再通知。
        assert_eq!(shells.kill_by_user(&id).unwrap(), ShellStatus::KilledByUser);
        let none = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(none.is_err());
    }

    /// 宿主的提醒条认的是这份快照：起来了就有一条，被结束之后就没有。
    #[tokio::test]
    async fn running_snapshot_follows_start_and_kill() {
        let shells = shells();
        assert!(shells.running().is_empty(), "刚建的表是空的");
        let (id, _) = shells
            .spawn("sleep 30", &std::env::temp_dir(), &sh(), "test-session")
            .unwrap();
        let running = shells.running();
        assert_eq!(running.len(), 1, "{running:?}");
        assert_eq!(running[0].id, id);
        assert_eq!(running[0].command, "sleep 30");
        assert!(running[0].started_at > 0);
        shells.kill(&id).unwrap();
        assert!(shells.running().is_empty(), "杀掉之后不该还挂在提醒条上");
    }

    /// 起来、结束各推一次快照 —— 宿主靠这两下画出来和收回去。
    #[tokio::test]
    async fn observer_gets_a_snapshot_on_start_and_exit() {
        let shells = shells();
        let seen = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let sink = seen.clone();
        shells.set_observer(Arc::new(move |view| {
            sink.lock()
                .unwrap()
                .push(view.into_iter().map(|s| s.id).collect());
        }));
        let mut rx = shells.subscribe();
        let (id, _) = shells
            .spawn("echo hi", &std::env::temp_dir(), &sh(), "test-session")
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.first(), Some(&vec![id.clone()]), "{seen:?}");
        assert_eq!(seen.last(), Some(&Vec::new()), "{seen:?}");
    }

    #[tokio::test]
    async fn an_unknown_shell_id_is_a_clear_error() {
        let shells = shells();
        let err = shells.read("sh404").unwrap_err();
        assert!(err.contains("sh404"), "{err}");
    }
}
