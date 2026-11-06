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
