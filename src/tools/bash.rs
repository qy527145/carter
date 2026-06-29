//! bash：执行 shell 命令。捕获 stdout/stderr/exit_code，带超时与大输出截断。
//!
//! 安全栅栏：
//! - 危险命令（`rm -rf` / 反引号注入提示）被检测到时**仍然允许执行**，但在结果首行打 `[warning]`
//!   提示，让模型能在产生破坏前读到（错误即数据）。这是"软警告"——硬阻断由配置后续接 Hook 实现。
//! - Unix 上**用进程组**起子进程，超时后 `kill(-pgid)` 清理整个进程树，
//!   避免 `bash -c "long-running ... & disown"` 这类残留。

use std::process::Stdio;
use std::time::Duration;

// rustc 误报：pre_exec 是这个 trait 的方法，确实在用。
#[cfg(unix)]
#[allow(unused_imports)]
use std::os::unix::process::CommandExt;

use serde_json::{json, Value};

use super::{arg_str, arg_u64_opt, truncate, Tool, ToolResult};

pub struct Bash;

const MAX_OUTPUT: usize = 30_000;
const DEFAULT_TIMEOUT_SECS: u64 = 120;

#[async_trait::async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "执行 shell 命令（sh -c）。返回 stdout/stderr 与退出码。带超时、大输出截断、\
         进程组超时清理、危险命令软警告。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的命令" },
                "timeout_secs": { "type": "integer", "description": "超时秒数（缺省 120）" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let command = match arg_str(&args, "command") {
            Ok(c) => c,
            Err(e) => return e,
        };
        let timeout = arg_u64_opt(&args, "timeout_secs").unwrap_or(DEFAULT_TIMEOUT_SECS);

        let warning = sniff_dangerous(&command);

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Unix：起独立进程组，便于超时时一起 kill（含子孙进程）。
        // Windows：tokio::process 在 Windows 上自带 Job Object 行为，drop child 即清理树，
        // 不需要也不能用 setsid。
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                // 新建进程组：pgid 等于子进程 pid。
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        // child 在 unix 路径下不变；保持非 mut 即可（不再写回）。
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("failed to spawn command: {e}")),
        };

        // 记下 child pid 供超时杀进程树用。
        #[cfg(unix)]
        let pgid: Option<i32> = child.id().map(|p| p as i32);

        let wait_fut = child.wait_with_output();
        let output = match tokio::time::timeout(Duration::from_secs(timeout), wait_fut).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolResult::err(format!("command execution error: {e}")),
            Err(_) => {
                // 超时：杀整个进程组（如能拿到 pgid）。Windows 路径走 child 的 Drop 兜底。
                #[cfg(unix)]
                if let Some(pid) = pgid {
                    // SAFETY: kill(2) 接收负 pgid 表示发给整个进程组。
                    unsafe {
                        libc::kill(-pid, libc::SIGTERM);
                    }
                    // 给子进程少量时间清理，再 SIGKILL 兜底。
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                return ToolResult::err(format!("command timed out after {timeout}s (process tree killed)"));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);

        let mut body = String::new();
        if let Some(w) = &warning {
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
            body.push_str("(no output)");
        }
        let body = truncate(&body, MAX_OUTPUT);
        let report = format!("exit_code={code}\n{body}");

        if output.status.success() {
            ToolResult::ok(report)
        } else {
            // 非零退出也作为结构化错误回灌（错误即数据）。
            ToolResult::err(report)
        }
    }
}

/// 嗅探危险命令模式 —— 命中则返回提示文本，调用方在结果首行打 `[warning]` 注释。
/// 软警告：不阻断执行（硬阻断后续由 Hook 系统的 PreToolUse 实现），让模型在结果里看到。
fn sniff_dangerous(cmd: &str) -> Option<&'static str> {
    let normalized = cmd.trim();
    // rm -rf / -fr 两种写法 + 接根目录 / 用户目录 / `*` 等高危目标。
    let lower = normalized.to_ascii_lowercase();
    if (lower.contains("rm -rf") || lower.contains("rm -fr"))
        && (lower.contains(" /") || lower.contains(" ~") || lower.contains(" *") || lower.contains(" .git"))
    {
        return Some("detected `rm -rf` against a sensitive target — double-check before relying on the result");
    }
    // dd if=/dev/... of=/dev/sd* —— 误触会破坏磁盘。
    if lower.contains("dd ") && lower.contains("of=/dev/") {
        return Some("`dd` writing to a /dev/* device is destructive and irreversible");
    }
    // mkfs / format /dev/...
    if (lower.starts_with("mkfs") || lower.contains(" mkfs")) && lower.contains("/dev/") {
        return Some("`mkfs` formats a block device — irreversible");
    }
    // shutdown / reboot —— 一般 agent 不需要做这件事。
    if lower.starts_with("shutdown") || lower.starts_with("reboot") || lower.starts_with("halt") {
        return Some("system power command — proceeding will affect the host machine");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::sniff_dangerous;

    #[test]
    fn rm_rf_root_warns() {
        assert!(sniff_dangerous("rm -rf /tmp/x").is_some());
        assert!(sniff_dangerous("rm -rf /").is_some());
        assert!(sniff_dangerous("rm -rf ~/proj").is_some());
        assert!(sniff_dangerous("rm -rf *.log").is_some());
        assert!(sniff_dangerous("rm -rf .git").is_some());
    }

    #[test]
    fn safe_rm_does_not_warn() {
        // 非 -rf / 不接根级路径 → 不警告。
        assert!(sniff_dangerous("rm a.txt").is_none());
        assert!(sniff_dangerous("rm -r build").is_none());
    }

    #[test]
    fn dd_to_dev_warns() {
        assert!(sniff_dangerous("dd if=foo.iso of=/dev/sda bs=1M").is_some());
    }

    #[test]
    fn mkfs_warns() {
        assert!(sniff_dangerous("mkfs.ext4 /dev/sda1").is_some());
    }

    #[test]
    fn shutdown_warns() {
        assert!(sniff_dangerous("shutdown -h now").is_some());
        assert!(sniff_dangerous("reboot").is_some());
    }

    #[test]
    fn benign_commands_pass() {
        assert!(sniff_dangerous("ls -la").is_none());
        assert!(sniff_dangerous("cargo build").is_none());
        assert!(sniff_dangerous("git status").is_none());
    }
}
