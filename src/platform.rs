//! 平台相关的进程启动细节。
//!
//! ## Windows 上为什么要显式隐藏控制台
//!
//! carter 既作为 CLI 跑（自带控制台），也被 GUI 宿主（carter-app）当库用。GUI 宿主
//! 在 release 下是 `windows_subsystem = "windows"`，**没有控制台可以继承** —— 于是
//! 每起一个控制台程序（`git.exe`、`cmd.exe`、`sh.exe`、`rg.exe`、MCP 的 `npx` …），
//! 系统都会给它新开一个控制台窗口，用户看到的就是「启动 / 新建对话时闪一个黑框」。
//!
//! `CREATE_NO_WINDOW` 让子进程**不新建、也不继承**控制台：进程照常跑，stdin/stdout
//! 句柄照常可用（我们本来就把它们接到管道上），只是不出现窗口。
//!
//! CLI 场景下带上它也无害：这些子进程的输入输出全走管道，不依赖终端。反过来说，
//! **漏掉一处就会在宿主里闪一次黑框**，所以本 crate 里每一处 `Command::new` 都要过
//! 一遍这里。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// `CreateProcessW` 的 `CREATE_NO_WINDOW`。
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 让一条 `std::process::Command` 不在 Windows 上弹控制台窗口。
#[cfg(windows)]
pub fn hide_console(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// `tokio::process::Command` 的同名版本。
#[cfg(windows)]
pub fn hide_console_async(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}

// ---------------------------------------------------------------------------
// shell 探测与 bash 工具的描述
// ---------------------------------------------------------------------------
//
// `bash` 工具早先硬编码 `Command::new("sh")`。在 Windows 上，`sh` 只有装了
// Git for Windows / MSYS / WSL 且在 PATH 里才存在；从资源管理器或 IDE 启动时
// PATH 又常常是精简过的，于是模型每一次 `bash` 调用都得到
// `failed to spawn command: program not found`，而且看不出原因。
//
// 这里探测一次、缓存下来，并做第二件事：**把探测结果写进工具描述**。模型看不到
// 我们的进程环境，只能从描述里知道「我被调起来时是 cmd 还是 sh」—— 不告诉它，
// 它就会生成 `ls` 然后拿到「不是内部或外部命令」。
//
// 探测顺序：Windows 上优先找 POSIX shell（模型对 `ls`/`grep`/`sed` 的熟练度远高于
// `dir`/`findstr`），找不到才退回 `cmd /C`（系统一定有）。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Macos,
    Linux,
}

pub fn current_os() -> Os {
    if cfg!(target_os = "windows") {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

pub fn os_label() -> &'static str {
    match current_os() {
        Os::Windows => "Windows",
        Os::Macos => "macOS",
        Os::Linux => "Linux",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// POSIX shell（sh / bash / Git Bash）。命令用 `-c` 传入。
    Posix,
    /// Windows cmd.exe。命令用 `/C` 传入。
    Cmd,
}

#[derive(Debug, Clone)]
pub struct Shell {
    pub program: PathBuf,
    pub flag: &'static str,
    pub kind: ShellKind,
}

impl Shell {
    /// 描述里用来告诉模型「命令是怎么被执行的」。
    ///
    /// 名字按**真正用的那个** shell 写（zsh / bash / sh），不写死成 `sh`：描述里说
    /// `sh` 而实际跑的是 zsh，模型就会以为 `[[ ]]`、`**/*` 这些不能用，白白绕路。
    pub fn label(&self) -> String {
        match self.kind {
            ShellKind::Posix => {
                let name = self
                    .program
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "sh".into());
                // Git Bash 值得单独点出来 —— 它意味着 Windows 上也有完整的 POSIX 工具集。
                if cfg!(windows) && name.contains("bash") {
                    "Git Bash（POSIX shell）".to_string()
                } else {
                    format!("POSIX shell（{name}）")
                }
            }
            ShellKind::Cmd => "Windows cmd.exe".to_string(),
        }
    }

    /// 调用形态，写进描述里让模型知道怎么组织命令。
    pub fn invocation(&self) -> String {
        match self.kind {
            ShellKind::Posix => format!("`{} -c \"命令\"`", self.program.display()),
            ShellKind::Cmd => "`cmd /C \"命令\"`".to_string(),
        }
    }
}

/// 探测本机可用的 shell。
///
/// 探测**只在进程启动后第一次调用时跑一次**（结果缓存）—— 它要碰文件系统，
/// 而工具描述在每一轮构造工具池时都会被读。
pub fn detect_shell() -> Shell {
    static CACHE: OnceLock<Shell> = OnceLock::new();
    CACHE.get_or_init(detect_shell_uncached).clone()
}

fn detect_shell_uncached() -> Shell {
    if !cfg!(windows) {
        // 用用户的默认 shell（终端里那一个），而不是硬编码的 `sh`：两边行为一致，
        // 模型也不会因为「描述说 sh、实际是 zsh」而避开 zsh 能用的写法。
        // 拿不到 `$SHELL` 时才退回 `sh` —— 它在任何 Unix 上都在。
        return Shell {
            program: login_shell().unwrap_or_else(|| PathBuf::from("sh")),
            flag: "-c",
            kind: ShellKind::Posix,
        };
    }

    if let Some(p) = find_posix_shell() {
        return Shell {
            program: p,
            flag: "-c",
            kind: ShellKind::Posix,
        };
    }

    // 兜底：`cmd.exe` 在任何 Windows 上都有。用绝对路径而不是 "cmd" ——
    // 免得多一层 PATHEXT 解析，也能绕开被篡改的 PATH。
    let cmd = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    Shell {
        program: cmd,
        flag: "/C",
        kind: ShellKind::Cmd,
    }
}

// ---------------------------------------------------------------------------
// 默认 shell 与登录环境
// ---------------------------------------------------------------------------
//
// GUI 进程（carter-app）从 Finder / 开始菜单启动，不读 shell 启动文件，拿到的 PATH
// 只有系统默认的几条；而 bun / cargo / brew 的目录都是用户在 `.zshrc` / `.zshenv`
// 里补进去的。于是「终端里明明能跑的 `bun run dev`，工具里就是 command not found」。
// 用户感受到的是「工具没用我的 shell」，实际差别在 PATH。
//
// 这里做两件事：选用户终端里那一个 shell（[`login_shell`]），以及把它的 PATH 读回来
// 交给宿主合并（[`login_shell_path`]）。

/// 能用 `-c` 传命令、且语法与模型写出来的命令足够兼容的 shell。
/// fish / nu 之类不在列 —— 语法差得远，写进工具描述只会把模型带偏。
const POSIX_SHELLS: &[&str] = &["zsh", "bash", "sh", "dash", "ksh", "ksh93", "mksh", "ash"];

/// 用户的默认 shell：`$SHELL` 优先，其次 `/bin/zsh`、`/bin/bash`。
pub fn login_shell() -> Option<PathBuf> {
    pick_posix_shell(std::env::var("SHELL").ok().as_deref(), &|p| p.is_file())
}

/// `$SHELL` 是外部输入，不能无条件信：可能是 fish，也可能指向已经被删掉的路径。
/// 拆成纯函数是为了能在任何平台上测。
fn pick_posix_shell(env_shell: Option<&str>, exists: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let usable = |p: &Path| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string());
        p.is_absolute() && name.is_some_and(|n| POSIX_SHELLS.iter().any(|s| *s == n)) && exists(p)
    };
    if let Some(s) = env_shell
        && usable(Path::new(s))
    {
        return Some(PathBuf::from(s));
    }
    ["/bin/zsh", "/bin/bash"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| usable(p))
}

/// 用户登录 + 交互 shell 里的 `PATH`，顺序与那个 shell 里一致。
///
/// 探测只做一次（「探不到」也缓存）：它要起一个 shell 读 rc 文件，慢且不可预测，
/// 不能挂在每一次工具调用或每一轮描述构造上。
pub fn login_shell_path() -> Option<Vec<PathBuf>> {
    static CACHE: OnceLock<Option<Vec<PathBuf>>> = OnceLock::new();
    CACHE.get_or_init(probe_login_shell_path).clone()
}

fn probe_login_shell_path() -> Option<Vec<PathBuf>> {
    if cfg!(windows) {
        // Windows 上没有「登录 shell」这回事，PATH 由注册表给，宿主自己补目录。
        return None;
    }
    let shell = login_shell()?;
    let mut cmd = std::process::Command::new(&shell);
    cmd.args(["-l", "-i", "-c", PROBE_SCRIPT])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        // rc 文件里的告警、`-i` 的 job control 提示都走 stderr，别让它们掺进解析。
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buf = String::new();
        let _ = out.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(raw) => {
            let _ = child.wait();
            parse_login_path(&raw)
        }
        // rc 里可能挂着要交互的东西（询问、下载）。启动路径上不能等，直接杀掉。
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// 带标记行输出 PATH：rc 文件自己也可能往 stdout 写东西，靠标记才认得出哪一行是它。
const PROBE_SCRIPT: &str = "printf 'CARTER_LOGIN_PATH=%s\\n' \"$PATH\"";

fn parse_login_path(raw: &str) -> Option<Vec<PathBuf>> {
    // 取最后一行：rc 里若自己也 echo 了同名标记，最靠后的才是我们这一句的输出。
    let value = raw
        .lines()
        .filter_map(|l| l.trim_end_matches('\r').strip_prefix("CARTER_LOGIN_PATH="))
        .next_back()?;
    let dirs: Vec<PathBuf> = std::env::split_paths(value)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    (!dirs.is_empty()).then_some(dirs)
}

/// 合并两份 PATH：`first` 整体在前，随后追加 `second` 里没出现过的条目。
///
/// 幂等 —— 把结果再和自己合并得到同一份列表，宿主可以放心地反复调用。
pub fn merge_path(first: &[PathBuf], second: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(first.len() + second.len());
    for p in first.iter().chain(second) {
        if p.as_os_str().is_empty() || out.contains(p) {
            continue;
        }
        out.push(p.clone());
    }
    out
}

/// 找一个能用的 POSIX shell：先看 PATH，再看几个常见安装位置。
fn find_posix_shell() -> Option<PathBuf> {
    for name in ["sh", "bash"] {
        if let Some(p) = which(name) {
            return Some(p);
        }
    }
    // PATH 里没有不代表机器上没有 —— 从 Explorer / IDE 启动时 PATH 常常是精简过的。
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            candidates.push(base.join("Git/bin/sh.exe"));
            candidates.push(base.join("Git/usr/bin/sh.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        candidates.push(local.join("Programs/Git/bin/sh.exe"));
    }
    // WSL 的 sh 能跑，但工作目录映射会很别扭（`/mnt/c/...`），不做首选。
    candidates.into_iter().find(|p| p.is_file())
}

/// 按 PATH 找一个可执行文件。Windows 上还要试 PATHEXT 里的后缀。
///
/// 供「这个程序在本机装了没有」这类**只探一次**的判断使用。它不是通用的进程
/// 启动器 —— 真要执行时仍然直接 `Command::new`，让系统按自己的规则解析。
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };

    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in &exts {
            let cand = dir.join(format!("{program}{ext}"));
            if is_executable_file(&cand) {
                return Some(cand);
            }
        }
        // Windows 上 `sh` 也可能没有后缀。
        let bare = dir.join(program);
        if is_executable_file(&bare) {
            return Some(bare);
        }
    }
    None
}

fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

// ---------------------------------------------------------------------------
// Python 探测（只在 Windows 上做）
// ---------------------------------------------------------------------------
//
// Windows 上模型写 cmd / PowerShell 的出错率远高于写 bash：引号、转义、变量展开、
// `&&` 与 `;` 的差异，几乎每一条超过一行的命令都要返工一次。而 Python 在 Windows
// 开发机上十分常见（python.org 安装器、`py` 启动器、`uv`），模型写 Python 又稳得多。
// 所以在 Windows 上探测一次有没有 Python，有就在 bash 工具描述里明确说：
// **多行逻辑写成 Python 脚本跑，不要硬写 cmd / PowerShell**。
//
// 为什么要真的跑一次 `--version` 而不是只看文件在不在：
// 微软商店在 `%LOCALAPPDATA%\Microsoft\WindowsApps\python.exe` 放了一个「应用执行
// 别名」—— Python 没装时它也在，运行只会弹商店。只看 `is_file()` 会被它骗到。
// 带参数运行它只打印一句「Python was not found」并返回 9009，不弹窗，所以用
// `--version` 的退出码判定是安全的。
//
// 为什么只在 Windows 上探：macOS 上没装 Command Line Tools 时 `/usr/bin/python3`
// 是 xcode-select 的占位，一运行就弹「要不要安装开发者工具」的对话框 —— 而 Unix
// 上 bash 本来就好用，没必要为一段提示去冒这个险。

/// 找到的是哪一种 Python 入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonKind {
    /// 直接的解释器：`python` / `python3`。
    Interpreter,
    /// Windows 的 `py` 启动器（随 python.org 安装器一起装）。
    Launcher,
    /// `uv`：能 `uv run python`，还能 `--with` 临时带第三方库；没装 Python 时会自己下一个。
    Uv,
}

/// 一个能用的 Python 入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonRuntime {
    pub kind: PythonKind,
    /// 可执行文件。`on_path` 为真时这是裸名字（`python`），否则是绝对路径。
    pub program: PathBuf,
    /// 是否从 PATH 里找到的 —— 决定描述里让模型写 `python` 还是写完整路径。
    pub on_path: bool,
}

impl PythonRuntime {
    /// 写进命令行的形态：PATH 里的用裸名字，别处的给绝对路径并加引号（路径多半带空格）。
    fn command(&self) -> String {
        if self.on_path {
            self.program.to_string_lossy().into_owned()
        } else {
            format!("\"{}\"", self.program.display())
        }
    }

    /// 「运行一个脚本」的前缀：`python`、`py -3`、`uv run python`。
    pub fn run_prefix(&self) -> String {
        match self.kind {
            PythonKind::Interpreter => self.command(),
            PythonKind::Launcher => format!("{} -3", self.command()),
            PythonKind::Uv => format!("{} run python", self.command()),
        }
    }
}

/// 探测本机的 Python 入口（Windows 专属；别的平台恒为空）。
///
/// 返回的顺序就是推荐顺序：解释器 > `py` 启动器 > `uv`。只跑一次，结果缓存 ——
/// 它要 spawn 子进程做验证，而工具描述每一轮构造工具池时都会读。
pub fn detect_python() -> &'static [PythonRuntime] {
    static CACHE: OnceLock<Vec<PythonRuntime>> = OnceLock::new();
    CACHE.get_or_init(detect_python_uncached).as_slice()
}

fn detect_python_uncached() -> Vec<PythonRuntime> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let mut found = Vec::new();

    // 解释器：PATH 里的 python / python3，再看 python.org 安装器的默认目录。
    let interp = ["python", "python3"]
        .iter()
        .find_map(|name| which(name).map(|_| (PathBuf::from(name), true)))
        .or_else(|| {
            python_install_dirs()
                .into_iter()
                .map(|d| d.join("python.exe"))
                .find(|p| p.is_file())
                .map(|p| (p, false))
        });
    if let Some((program, on_path)) = interp
        && runs_ok(&program, &["--version"])
    {
        found.push(PythonRuntime {
            kind: PythonKind::Interpreter,
            program,
            on_path,
        });
    }

    // `py` 启动器：通常在 %WINDIR%（PATH 里一定有），也可能装在用户目录。
    let launcher = which("py")
        .map(|_| (PathBuf::from("py"), true))
        .or_else(|| {
            let mut cands = Vec::new();
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                cands.push(PathBuf::from(local).join("Programs/Python/Launcher/py.exe"));
            }
            if let Some(win) = std::env::var_os("WINDIR") {
                cands.push(PathBuf::from(win).join("py.exe"));
            }
            cands.into_iter().find(|p| p.is_file()).map(|p| (p, false))
        });
    if let Some((program, on_path)) = launcher
        && runs_ok(&program, &["-3", "--version"])
    {
        found.push(PythonRuntime {
            kind: PythonKind::Launcher,
            program,
            on_path,
        });
    }

    // uv：官方安装脚本放在 ~/.local/bin，`cargo install` 放在 ~/.cargo/bin。
    let uv = which("uv")
        .map(|_| (PathBuf::from("uv"), true))
        .or_else(|| {
            let home = std::env::var_os("USERPROFILE").map(PathBuf::from)?;
            [
                home.join(".local/bin/uv.exe"),
                home.join(".cargo/bin/uv.exe"),
            ]
            .into_iter()
            .find(|p| p.is_file())
            .map(|p| (p, false))
        });
    if let Some((program, on_path)) = uv
        && runs_ok(&program, &["--version"])
    {
        found.push(PythonRuntime {
            kind: PythonKind::Uv,
            program,
            on_path,
        });
    }

    found
}

/// python.org 安装器的默认位置：`%LOCALAPPDATA%\Programs\Python\Python3xx`、
/// `%ProgramFiles%\Python3xx`、`C:\Python3xx`。版本号在目录名里，所以要列目录。
fn python_install_dirs() -> Vec<PathBuf> {
    let mut parents = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        parents.push(PathBuf::from(local).join("Programs/Python"));
    }
    for var in ["ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            parents.push(PathBuf::from(pf));
        }
    }
    parents.push(PathBuf::from(r"C:\"));

    let mut dirs: Vec<PathBuf> = parents
        .iter()
        .filter_map(|p| std::fs::read_dir(p).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("Python3"))
                    .unwrap_or(false)
        })
        .collect();
    // 版本高的排前面：`Python313` > `Python39`（按长度再按字典序，避免 39 > 313）。
    dirs.sort_by(|a, b| {
        let (a, b) = (
            a.to_string_lossy().to_string(),
            b.to_string_lossy().to_string(),
        );
        b.len().cmp(&a.len()).then_with(|| b.cmp(&a))
    });
    dirs
}

/// 跑一下 `program args`，看它能不能正常退出。
///
/// 只用于探测：输出不读（`--version` 这种一行的输出撑不满管道），超时就杀掉当失败。
/// 超时的意义是兜住「程序卡住等输入」的情况 —— 探测在启动路径上，不能挂住整个进程。
fn runs_ok(program: &Path, args: &[&str]) -> bool {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    hide_console(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(30));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Windows 上关于「优先用 Python」的那一段描述。拆成纯函数是为了能在任何平台上测文案。
///
/// `pythons` 为空时也要说一句「没探测到」—— 否则模型会凭经验试 `python`，
/// 撞上商店占位程序或「不是内部或外部命令」再绕回来。
fn python_guidance(pythons: &[PythonRuntime], shell: ShellKind, temp_dir: &Path) -> String {
    let Some(primary) = pythons.first() else {
        return "\n**本机没探测到 Python**（`python` / `py` / `uv` 都不在），别去试它们；\
                多行逻辑只能靠 shell 自己或 `powershell -NoProfile -Command \"...\"`。\n"
            .to_string();
    };
    let listing = pythons
        .iter()
        .map(|p| match p.kind {
            PythonKind::Interpreter => format!("解释器 `{}`", p.command()),
            PythonKind::Launcher => format!("`py` 启动器（`{} -3`）", p.command()),
            PythonKind::Uv => format!("`uv`（`{} run`）", p.command()),
        })
        .collect::<Vec<_>>()
        .join("、");
    let run = primary.run_prefix();
    let script = temp_dir.join("carter-任务名.py");
    let script = script.display();

    let mut d = format!(
        "\n**优先用 Python 写逻辑，不要硬写 cmd / PowerShell**：本机有 {listing}。\
         cmd 与 PowerShell 的引号、转义、变量展开规则很容易写错；凡是超过一行的逻辑\
         （遍历 / 批量改名文件、文本替换、JSON / CSV 处理、条件与循环、HTTP 请求、\
         拼装多步操作）一律写成 Python 脚本：先用 `write_file` 把脚本写到 `{script}`，\
         再 `{run} \"{script}\"` 运行。单行小表达式可以 `{run} -c \"...\"`，\
         但里面一出现引号就改成写文件。\
         Python 标准库的 pathlib / shutil / subprocess / json / re / urllib \
         覆盖了绝大多数 shell 任务，默认不装第三方包"
    );
    if shell == ShellKind::Posix {
        d.push_str("；在 POSIX shell 下也可以用 heredoc 内联：`");
        d.push_str(&run);
        d.push_str(" - <<'EOF' ... EOF`");
    }
    d.push('。');
    match pythons.iter().find(|p| p.kind == PythonKind::Uv) {
        Some(uv) => {
            d.push_str(&format!(
                "真要用第三方库时用 `{} run --with 包名 脚本.py` 临时带上，\
                 不要 pip install 进全局环境。\n",
                uv.command()
            ));
        }
        None => {
            d.push_str("真要用第三方库时 `python -m pip install --user 包名`，装前先问用户。\n")
        }
    }
    d
}

/// 探测结果 + 平台上手要点，一起写进 bash 工具的描述。
///
/// 这一段是给**模型**看的，不是给人看的。所以要写它真正会踩的坑：
/// 哪些命令不存在、变量怎么写、路径怎么给。
pub fn bash_tool_description() -> String {
    let shell = detect_shell();
    let os = os_label();
    let inv = shell.invocation();

    let mut d = format!(
        "执行 shell 命令并返回 stdout / stderr / 退出码。带超时（时长与超时后的处理见 \
         `timeout_secs` 参数说明）、大输出截断、危险命令软警告。\n\n\
         **长时间运行的命令**（起服务、跑长构建、`tail -f`）加 `run_in_background: true`：\
         它立刻返回一个 shell_id，不卡住这一轮；之后用 `bash_output` 读它的新输出、\
         用 `kill_shell` 结束它。前台命令在用户点「停止」时会被立即杀掉。\n\n\
         当前环境：**{os}**，命令通过 {inv} 执行，shell 是 {}。\n\n\
         **当前目录**：第一条命令在会话的工作目录（系统提示里的「工作目录」）下执行，\
         之后像终端一样**保留 `cd` 的结果**（目录变了结果末尾会有一行 `[cwd]`）。\
         所以不要每条命令都写 `cd /完整/绝对/路径 && ...`：要进子目录就单独 `cd sub` 一次，\
         之后直接写命令；文件用相对路径。`cd` 到工作目录之外不会被保留，下一条命令回到工作目录。\n\n\
         **看图片不要用 bash**：想知道一张图（png/jpg/gif/webp/bmp）里是什么，直接 `read_file` \
         那个路径 —— 它把真实图像交给你看。不要写 python / PIL 脚本去「读」图：脚本看不到画面，\
         而且环境里多半没装 PIL。只有要**裁剪 / 放大局部**时才用命令处理，处理完再 `read_file` 产物。\n",
        shell.label()
    );
    if current_os() == Os::Macos {
        d.push_str(
            "macOS 裁图 / 缩放用系统自带的 `sips`：\
             `sips -c 高 宽 --cropOffset y x in.png --out out.png`、`sips -Z 最长边 in.png --out out.png`。\n",
        );
    }

    match shell.kind {
        ShellKind::Posix => {
            d.push_str(
                "\n写命令时的注意事项：\n\
                 - 用的是 POSIX 工具集：ls / cat / grep / sed / awk / find / xargs 都能用。\n\
                 - 路径分隔符用 `/`（`src/main.rs`）。绝对路径 `C:/Users/...` 也认。\n\
                 - 调用 Windows 自带程序时带上后缀更稳（`whoami.exe`、`ipconfig.exe`、`tasklist.exe`）；\
                 不带后缀通常也行，因为它们就在 PATH 里。\n\
                 - **cmd 的内建命令不可用**：`dir` `copy` `type` `del` `set` 都要换成 \
                 `ls` `cp` `cat` `rm` `env`。\n\
                 - `echo %VAR%` 是 cmd 写法，这里要写 `echo $VAR`。\n\
                 - 中文路径要加引号。\n",
            );
        }
        ShellKind::Cmd => {
            d.push_str(
                "\n写命令时的注意事项：\n\
                 - 这是 **cmd.exe，不是 bash**。`ls` `grep` `cat` `sed` 都**不存在**。\n\
                 - 列目录用 `dir`，看文件用 `type`，搜内容用 `findstr /s /n \"关键词\" *.rs`，\
                 复制用 `copy`，删除用 `del`。\n\
                 - 环境变量是 `%VAR%`，不是 `$VAR`。\n\
                 - 多条命令用 `&&` 连接；`;` 分隔符不可用。\n\
                 - 路径带空格时加引号。\n\
                 - 需要 grep/sed 这类工具时，用 `powershell -NoProfile -Command \"...\"` 代替，\
                 不要指望 bash 命令能跑。\n",
            );
        }
    }
    if current_os() == Os::Windows {
        d.push_str(&python_guidance(
            detect_python(),
            shell.kind,
            &std::env::temp_dir(),
        ));
    }
    d
}

/// 描述算一次就够 —— shell 探测与字符串拼接没必要每轮都做。
///
/// `Tool::description()` 的签名是 `&self -> &str`，所以这里返回 `&'static str`：
/// 它比 `&self` 活得久，能直接满足签名，不需要 `Box::leak`。
pub fn bash_tool_description_leaked() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(bash_tool_description).as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 描述里写的必须是**真正在用**的那个 shell。macOS 上默认是 zsh，
    /// 写死成 `sh` 会让模型避开 zsh 能用的写法。
    #[test]
    fn the_label_names_the_actual_shell() {
        let s = Shell {
            program: PathBuf::from("/bin/zsh"),
            flag: "-c",
            kind: ShellKind::Posix,
        };
        assert!(s.label().contains("zsh"), "{}", s.label());
        let b = Shell {
            program: PathBuf::from("/bin/bash"),
            flag: "-c",
            kind: ShellKind::Posix,
        };
        assert!(b.label().contains("bash"), "{}", b.label());
        assert!(b.invocation().contains("/bin/bash -c"));
    }

    /// `$SHELL` 是外部输入：fish 的语法不一样，指向已删除的路径也不能用。
    #[test]
    fn only_posix_shells_are_accepted() {
        let yes = |_: &Path| true;
        assert_eq!(
            pick_posix_shell(Some("/bin/zsh"), &yes),
            Some(PathBuf::from("/bin/zsh"))
        );
        let picked = pick_posix_shell(Some("/opt/homebrew/bin/fish"), &yes);
        assert!(
            picked
                .as_ref()
                .is_none_or(|p| p.file_stem().is_some_and(|s| s != "fish")),
            "fish 不能被当成 POSIX shell：{picked:?}"
        );
        // 不存在的路径同样跳过，退到下一个候选。
        let only_bash = |p: &Path| p == Path::new("/bin/bash");
        assert_eq!(
            pick_posix_shell(Some("/bin/zsh"), &only_bash),
            Some(PathBuf::from("/bin/bash"))
        );
    }

    /// 探测脚本的输出里可能混着 rc 文件自己 echo 的东西：只认标记行，且取最后一行。
    #[test]
    fn the_probe_output_is_parsed_by_marker() {
        let raw = "Welcome\nCARTER_LOGIN_PATH=/usr/bin:/bin\nbye\n";
        assert_eq!(
            parse_login_path(raw).unwrap(),
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
        );
        let raw = "CARTER_LOGIN_PATH=/old\nCARTER_LOGIN_PATH=/a:/b\n";
        assert_eq!(
            parse_login_path(raw).unwrap(),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert!(parse_login_path("no marker here").is_none());
        assert!(parse_login_path("CARTER_LOGIN_PATH=\n").is_none());
    }

    /// 本机探测要真能拿到 PATH —— 挡的是「探了但解析错 / 超时，宿主悄悄退回兜底目录」。
    #[cfg(unix)]
    #[test]
    fn the_login_shell_path_is_probed_on_unix() {
        if login_shell().is_none() {
            return;
        }
        let dirs = login_shell_path().expect("登录 shell 的 PATH 应该能读回来");
        assert!(
            dirs.iter()
                .any(|d| d == Path::new("/usr/bin") || d == Path::new("/bin")),
            "读回来的 PATH 不像 PATH：{dirs:?}"
        );
    }

    /// 合并保留顺序、去重，并且幂等 —— 宿主每次启动都会调。
    #[test]
    fn merge_path_keeps_order_and_dedupes() {
        let login = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let current = vec![PathBuf::from("/b"), PathBuf::from("/c"), PathBuf::from("")];
        let merged = merge_path(&login, &current);
        assert_eq!(
            merged,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c")
            ]
        );
        assert_eq!(merge_path(&merged, &merged), merged);
    }

    #[test]
    fn a_shell_is_always_found() {
        let s = detect_shell();
        // 探测的兜底是 cmd.exe，在任何 Windows 上都存在；Unix 上是 sh。
        // 这里只断言「拿到了一个非空的 program」，不断言具体是哪个 ——
        // 断言「一定是 sh」会让装了 Git 的机器和没装的机器测试结果不同。
        assert!(!s.program.as_os_str().is_empty());
        assert!(matches!(s.kind, ShellKind::Posix | ShellKind::Cmd));
    }

    /// 探测出来的 shell **必须真的存在** —— 否则就等于把「sh 找不到」换成了
    /// 另一个「找不到」，用户看到的还是同一句 program not found。
    ///
    /// 这条守的是用户报的那个 bug：从资源管理器 / IDE 启动时 PATH 常被精简，
    /// 里面有 `git.exe`（Git 只把 `Git\cmd` 写进 PATH）却没有 `sh.exe`。
    #[test]
    fn the_detected_shell_is_actually_spawnable() {
        let s = detect_shell();
        let ok = if s.program.is_absolute() {
            // 绝对路径必须真的在。
            s.program.is_file()
        } else {
            // 裸名字必须能从 PATH 解析出来。
            which(&s.program.to_string_lossy()).is_some()
        };
        assert!(
            ok,
            "探测到的 shell `{}` 不存在，spawn 时会报 program not found",
            s.program.display()
        );
    }

    /// `which` 得真的在找东西 —— 否则上面那条测试会因为「永远返回 Some」而恒过。
    #[test]
    fn which_does_not_find_things_that_are_not_there() {
        assert!(which("definitely-not-a-real-program-xyz").is_none());
    }

    /// 描述必须点出当前是哪一个 shell。这是整个模块存在的理由：
    /// 模型看不到我们的进程环境，只能从描述里知道该写 `ls` 还是 `dir`。
    #[test]
    fn the_description_names_the_platform_and_the_shell() {
        let d = bash_tool_description();
        assert!(d.contains(os_label()), "描述里要写明是哪个系统：{d}");
        assert!(
            d.contains("echo $VAR") || d.contains("%VAR%"),
            "描述里要写清环境变量的写法：{d}"
        );
        assert!(
            d.contains("cmd 的内建命令不可用") || d.contains("不是 bash"),
            "描述里要写清哪些命令不存在：{d}"
        );
    }

    /// 模型看图时爱先写 PIL 脚本（多半没装、也看不到画面），描述里要把它引回 read_file。
    #[test]
    fn the_description_points_images_to_read_file() {
        let d = bash_tool_description();
        assert!(
            d.contains("看图片不要用 bash") && d.contains("read_file"),
            "{d}"
        );
        if current_os() == Os::Macos {
            assert!(d.contains("sips"), "macOS 上要给出自带的裁图工具：{d}");
        }
    }

    fn rt(kind: PythonKind, program: &str, on_path: bool) -> PythonRuntime {
        PythonRuntime {
            kind,
            program: PathBuf::from(program),
            on_path,
        }
    }

    /// 有 Python 就要把「别写 cmd / PowerShell、写 Python 脚本」说清楚，
    /// 并给出能直接照抄的运行命令与脚本落点。
    #[test]
    fn python_guidance_tells_the_model_to_write_scripts() {
        let py = [
            rt(PythonKind::Interpreter, "python", true),
            rt(PythonKind::Uv, "uv", true),
        ];
        let d = python_guidance(&py, ShellKind::Cmd, Path::new(r"C:\Temp"));
        assert!(d.contains("优先用 Python"), "{d}");
        assert!(d.contains("write_file"), "要告诉模型脚本怎么落盘：{d}");
        // 分隔符跟平台走（macOS 上 join 出来是 `/`），期望值用同一个 join 算。
        let script = Path::new(r"C:\Temp").join("carter-任务名.py");
        assert!(
            d.contains(&format!("python \"{}\"", script.display())),
            "{d}"
        );
        assert!(
            d.contains("uv run --with"),
            "有 uv 就该推荐它带第三方库：{d}"
        );
        assert!(!d.contains("heredoc"), "cmd 下没有 heredoc：{d}");
    }

    /// 只有 `py` 启动器时运行前缀是 `py -3`；只有 uv 时是 `uv run python`。
    #[test]
    fn run_prefix_follows_the_kind() {
        assert_eq!(rt(PythonKind::Launcher, "py", true).run_prefix(), "py -3");
        assert_eq!(rt(PythonKind::Uv, "uv", true).run_prefix(), "uv run python");
        // PATH 之外的解释器要给绝对路径并加引号 —— 路径多半带空格。
        assert_eq!(
            rt(
                PythonKind::Interpreter,
                r"C:\Program Files\Python313\python.exe",
                false
            )
            .run_prefix(),
            r#""C:\Program Files\Python313\python.exe""#
        );
    }

    /// Git Bash 下多给一条 heredoc 的写法；没有 uv 时推荐 pip 并要求先问用户。
    #[test]
    fn python_guidance_adapts_to_posix_shell_and_missing_uv() {
        let py = [rt(PythonKind::Launcher, "py", true)];
        let d = python_guidance(&py, ShellKind::Posix, Path::new(r"C:\Temp"));
        assert!(d.contains("py -3 - <<'EOF'"), "{d}");
        assert!(d.contains("pip install --user"), "{d}");
    }

    /// 没探测到 Python 时要明说，免得模型凭经验去试 `python` 再撞商店占位程序。
    #[test]
    fn python_guidance_says_so_when_nothing_is_found() {
        let d = python_guidance(&[], ShellKind::Cmd, Path::new(r"C:\Temp"));
        assert!(d.contains("没探测到 Python"), "{d}");
        assert!(!d.contains("write_file"), "{d}");
    }

    /// 非 Windows 上既不探测、也不往描述里塞 Python 段 —— 那里 bash 本来就好用，
    /// 而且 macOS 的 python3 占位一运行会弹安装对话框。
    #[test]
    fn python_detection_is_windows_only() {
        if current_os() != Os::Windows {
            assert!(detect_python().is_empty());
            assert!(!bash_tool_description().contains("优先用 Python"));
        } else {
            // Windows 上两种结果都合法，但描述里必须二选一地提到 Python。
            let d = bash_tool_description();
            assert!(
                d.contains("优先用 Python") || d.contains("没探测到 Python"),
                "{d}"
            );
        }
    }

    /// 探测到的每一个入口都得真的能跑 —— 这条在 Windows 机器上挡「找到了商店占位程序」。
    #[test]
    fn every_detected_python_actually_runs() {
        for p in detect_python() {
            let args: &[&str] = match p.kind {
                PythonKind::Launcher => &["-3", "--version"],
                _ => &["--version"],
            };
            assert!(runs_ok(&p.program, args), "{:?} 跑不起来", p);
        }
    }

    #[test]
    fn invocation_matches_the_shell_kind() {
        let s = detect_shell();
        match s.kind {
            ShellKind::Posix => assert!(s.invocation().contains("-c")),
            ShellKind::Cmd => assert!(s.invocation().contains("/C")),
        }
    }
}
