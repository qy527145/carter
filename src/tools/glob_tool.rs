//! glob：文件名模式匹配。排除 .git，结果上限防爆 token。

use serde_json::{json, Value};

use super::{arg_str, Tool, ToolResult};

pub struct Glob;

const MAX_RESULTS: usize = 500;

#[async_trait::async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "按 glob 模式匹配文件路径（如 src/**/*.rs）。自动排除 .git 目录，结果有上限。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob 模式，如 **/*.rs" },
                "path": { "type": "string", "description": "搜索根目录（缺省当前目录）" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let pattern = match arg_str(&args, "pattern") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let base = args.get("path").and_then(Value::as_str).unwrap_or(".");

        // 拼接 base + pattern。
        let full = if base == "." || base.is_empty() {
            pattern.clone()
        } else {
            format!("{}/{}", base.trim_end_matches('/'), pattern)
        };

        let paths = match glob::glob(&full) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("invalid glob pattern: {e}")),
        };

        let mut hits: Vec<String> = Vec::new();
        for entry in paths {
            match entry {
                Ok(p) => {
                    let s = p.to_string_lossy();
                    if s.contains("/.git/") || s.starts_with(".git/") {
                        continue;
                    }
                    hits.push(s.into_owned());
                }
                Err(_) => continue,
            }
            if hits.len() >= MAX_RESULTS {
                break;
            }
        }

        if hits.is_empty() {
            return ToolResult::ok(format!("no files match: {full}"));
        }
        let truncated = hits.len() >= MAX_RESULTS;
        let mut out = hits.join("\n");
        if truncated {
            out.push_str(&format!("\n... [capped at {MAX_RESULTS} results]"));
        }
        ToolResult::ok(out)
    }
}
