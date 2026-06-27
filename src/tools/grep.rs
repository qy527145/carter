//! grep：基于系统 ripgrep 的内容搜索。rg 缺失时给出明确提示。

use std::process::Stdio;

use serde_json::{json, Value};

use super::{arg_str, truncate, Tool, ToolResult};

pub struct Grep;

const MAX_OUTPUT: usize = 30_000;

#[async_trait::async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "用 ripgrep 搜索文件内容。支持正则、按 glob 过滤、限定目录。需系统已安装 rg。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "正则表达式" },
                "path": { "type": "string", "description": "搜索目录或文件（缺省当前目录）" },
                "glob": { "type": "string", "description": "文件过滤 glob，如 *.rs" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let pattern = match arg_str(&args, "pattern") {
            Ok(p) => p,
            Err(e) => return e,
        };

        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--line-number").arg("--no-heading").arg("--color=never");
        if let Some(g) = args.get("glob").and_then(Value::as_str) {
            cmd.arg("--glob").arg(g);
        }
        cmd.arg(&pattern);
        if let Some(p) = args.get("path").and_then(Value::as_str) {
            cmd.arg(p);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => {
                return ToolResult::err(format!(
                    "cannot run ripgrep (is `rg` installed?): {e}"
                ))
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        // rg 退出码 1 = 无匹配（非错误）。
        match output.status.code() {
            Some(0) => ToolResult::ok(truncate(&stdout, MAX_OUTPUT)),
            Some(1) => ToolResult::ok(format!("no matches for: {pattern}")),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                ToolResult::err(format!("ripgrep error: {stderr}"))
            }
        }
    }
}
