//! bash：执行 shell 命令。捕获 stdout/stderr/exit_code，带超时与大输出截断。

use std::process::Stdio;
use std::time::Duration;

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
        "执行 shell 命令（sh -c）。返回 stdout/stderr 与退出码。带超时与大输出截断。"
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

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("failed to spawn command: {e}")),
        };

        let output = match tokio::time::timeout(
            Duration::from_secs(timeout),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolResult::err(format!("command execution error: {e}")),
            Err(_) => return ToolResult::err(format!("command timed out after {timeout}s")),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);

        let mut body = String::new();
        if !stdout.is_empty() {
            body.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !body.is_empty() {
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
