//! read_file：读文件，cat -n 风格行号输出，支持 offset/limit。

use serde_json::{json, Value};

use super::{arg_str, arg_u64_opt, truncate, Tool, ToolResult};

pub struct ReadFile;

const MAX_OUTPUT: usize = 50_000;
const DEFAULT_LIMIT: u64 = 2000;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "读取文本文件内容，输出带行号（cat -n 风格）。支持 offset/limit 按行范围读取。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径（相对或绝对）" },
                "offset": { "type": "integer", "description": "起始行号（1-based，缺省 1）" },
                "limit": { "type": "integer", "description": "读取行数（缺省 2000）" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let path = match arg_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let offset = arg_u64_opt(&args, "offset").unwrap_or(1).max(1);
        let limit = arg_u64_opt(&args, "limit").unwrap_or(DEFAULT_LIMIT);

        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("cannot read {path}: {e}")),
        };

        let start = (offset - 1) as usize;
        let lines: Vec<&str> = text.lines().collect();
        if start >= lines.len() && !lines.is_empty() {
            return ToolResult::err(format!(
                "offset {offset} beyond end of file ({} lines)",
                lines.len()
            ));
        }
        let end = (start + limit as usize).min(lines.len());
        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            let n = start + i + 1;
            out.push_str(&format!("{n:>6}\t{line}\n"));
        }
        if out.is_empty() {
            out.push_str("(empty file)");
        }
        ToolResult::ok(truncate(&out, MAX_OUTPUT))
    }
}
