//! glob：文件名模式匹配。默认排除常见噪声目录（.git/node_modules/target/dist/build/.venv）；
//! 结果上限防爆 token。

use serde_json::{json, Value};

use super::{arg_bool, arg_str, Tool, ToolResult};

pub struct Glob;

const MAX_RESULTS: usize = 500;

/// 默认排除的目录段（出现在路径任一层级即过滤）。
/// `include_hidden=true` 时不过滤 `.git` 之外的隐藏目录；`include_noise=true` 时全留。
const NOISE_SEGMENTS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    ".next",
    ".cache",
];

#[async_trait::async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "按 glob 模式匹配文件路径（如 src/**/*.rs）。默认排除常见构建/缓存目录\
         （.git/node_modules/target/dist/build/.venv/__pycache__/.next/.cache）。结果有上限。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob 模式，如 **/*.rs" },
                "path": { "type": "string", "description": "搜索根目录（缺省当前目录）" },
                "include_noise": { "type": "boolean", "description": "包含构建/缓存目录（缺省 false）" }
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
        let include_noise = arg_bool(&args, "include_noise");

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
        let mut filtered = 0usize;
        for entry in paths {
            match entry {
                Ok(p) => {
                    let s = p.to_string_lossy();
                    if !include_noise && is_noisy(&s) {
                        filtered += 1;
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
            let suffix = if filtered > 0 {
                format!(" (filtered {filtered} noise paths; pass include_noise=true to keep them)")
            } else {
                String::new()
            };
            return ToolResult::ok(format!("no files match: {full}{suffix}"));
        }
        let truncated = hits.len() >= MAX_RESULTS;
        let mut out = hits.join("\n");
        if truncated {
            out.push_str(&format!("\n... [capped at {MAX_RESULTS} results]"));
        }
        if filtered > 0 {
            out.push_str(&format!(
                "\n[note] filtered {filtered} noise paths (.git/node_modules/target/...)"
            ));
        }
        ToolResult::ok(out)
    }
}

/// 路径是否被任一噪声段命中（按 `/` 分段精确匹配，避免误伤子串）。
fn is_noisy(path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').collect();
    NOISE_SEGMENTS.iter().any(|n| segs.iter().any(|s| s == n))
}

#[cfg(test)]
mod tests {
    use super::is_noisy;

    #[test]
    fn filters_node_modules_and_target() {
        assert!(is_noisy("node_modules/foo/index.js"));
        assert!(is_noisy("a/b/target/debug/x"));
        assert!(is_noisy(".git/HEAD"));
        assert!(is_noisy("project/.venv/pyvenv.cfg"));
    }

    #[test]
    fn keeps_normal_paths() {
        assert!(!is_noisy("src/main.rs"));
        assert!(!is_noisy("docs/intro.md"));
        // 子串相似但非完整段：如文件名包含 target 字样不应误伤。
        assert!(!is_noisy("src/targetting/foo.rs"));
        assert!(!is_noisy("notes/builder.md"));
    }
}
