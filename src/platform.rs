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
    pub fn label(&self) -> &'static str {
        match self.kind {
            ShellKind::Posix => {
                let name = self
                    .program
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "sh".into());
                // `sh` 与 `bash` 对模型来说是一回事，但 Git Bash 值得一提 ——
                // 它意味着 Windows 上也有完整的 POSIX 工具集。
                if name.contains("bash") {
                    "Git Bash（POSIX shell）"
                } else {
                    "POSIX shell（sh）"
                }
            }
            ShellKind::Cmd => "Windows cmd.exe",
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
        // Unix 上 sh 一定在，不用探测。
        return Shell {
            program: PathBuf::from("sh"),
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

    #[test]
    fn invocation_matches_the_shell_kind() {
        let s = detect_shell();
        match s.kind {
            ShellKind::Posix => assert!(s.invocation().contains("-c")),
            ShellKind::Cmd => assert!(s.invocation().contains("/C")),
        }
    }
}
