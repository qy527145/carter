//! edit_file：精确串替换。read-before-edit 校验 old_string 存在且唯一（除非 replace_all），防脏写。

use serde_json::{json, Value};

use super::{arg_bool, arg_str, Tool, ToolResult};

pub struct EditFile;

#[async_trait::async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "精确字符串替换。默认要求 old_string 在文件中唯一出现；replace_all=true 则替换所有出现。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径" },
                "old_string": { "type": "string", "description": "要替换的原文（需唯一，除非 replace_all）" },
                "new_string": { "type": "string", "description": "替换后的新文本" },
                "replace_all": { "type": "boolean", "description": "替换所有出现（缺省 false）" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let path = match arg_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let old = match arg_str(&args, "old_string") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let new = match arg_str(&args, "new_string") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let replace_all = arg_bool(&args, "replace_all");

        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("cannot read {path}: {e}")),
        };

        let (updated, n) = match apply_edit(&text, &old, &new, replace_all) {
            Ok(r) => r,
            Err(e) => return ToolResult::err(e),
        };

        match tokio::fs::write(&path, &updated).await {
            Ok(()) => ToolResult::ok(format!("replaced {n} occurrence(s) in {path}")),
            Err(e) => ToolResult::err(format!("cannot write {path}: {e}")),
        }
    }
}

/// 纯函数核心，便于单测。返回 (新文本, 替换次数)。
/// 错误信息力求帮助模型自纠：报告匹配次数、最近候选行号（"看似但不是"）。
fn apply_edit(text: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("old_string must not be empty".to_string());
    }
    let count = text.matches(old).count();
    if count == 0 {
        // 提供 fuzzy 提示：找最长公共前缀让模型知道拼错在哪。
        let hint = nearest_match_hint(text, old);
        return Err(format!("old_string not found: {old:?}{hint}"));
    }
    if count > 1 && !replace_all {
        // 列出前 3 个匹配的行号，便于模型挑更具体的子串。
        let lines = first_match_lines(text, old, 3);
        let lines_str = lines
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "old_string appears {count} times (lines {lines_str}); pass replace_all=true or provide a more specific string"
        ));
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    Ok((updated, count))
}

/// 找出最长前缀重合的候选行；空返回串表示无线索。
fn nearest_match_hint(text: &str, old: &str) -> String {
    // 取 old 的首行（避免多行 old_string 噪声），找首行能匹配到多少前缀的源行。
    let first = old.lines().next().unwrap_or("").trim();
    if first.len() < 4 {
        return String::new();
    }
    // 前 12 字符前缀作为锚点。
    let n = first.chars().take(12).collect::<String>();
    if n.is_empty() {
        return String::new();
    }
    for (i, line) in text.lines().enumerate() {
        if line.contains(&n) {
            return format!(" — closest line {} contains the prefix {:?}", i + 1, n);
        }
    }
    String::new()
}

/// 列出前 N 个 old 出现的行号（1 基）。
fn first_match_lines(text: &str, old: &str, max: usize) -> Vec<usize> {
    let mut out = Vec::new();
    // 把整段拆行；记录每行起始 byte offset，再用 text.find_iter 找 byte offset → line。
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let mut start = 0usize;
    while let Some(off) = text[start..].find(old) {
        let abs = start + off;
        let lineno = match line_starts.binary_search(&abs) {
            Ok(i) => i + 1,
            Err(i) => i, // 插入位置 = 该行号（1 基）。
        };
        out.push(lineno);
        if out.len() >= max {
            break;
        }
        start = abs + old.len().max(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{apply_edit, first_match_lines, nearest_match_hint};

    #[test]
    fn unique_replace() {
        let (out, n) = apply_edit("hello world", "world", "rust", false).unwrap();
        assert_eq!(out, "hello rust");
        assert_eq!(n, 1);
    }

    #[test]
    fn not_found_errors() {
        let err = apply_edit("abc", "xyz", "q", false).unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn ambiguous_without_replace_all_errors() {
        let err = apply_edit("a a a", "a", "b", false).unwrap_err();
        // 错误里要带行号提示。
        assert!(err.contains("lines"), "expected line numbers, got: {err}");
    }

    #[test]
    fn replace_all_works() {
        let (out, n) = apply_edit("a a a", "a", "b", true).unwrap();
        assert_eq!(out, "b b b");
        assert_eq!(n, 3);
    }

    #[test]
    fn first_match_lines_finds_correct_row() {
        let text = "line1\nfoo bar\nline3\nfoo baz\nline5";
        let lines = first_match_lines(text, "foo", 5);
        assert_eq!(lines, vec![2, 4]);
    }

    #[test]
    fn nearest_match_hint_finds_typos() {
        let text = "fn render_history(ctx: &Ctx) {}\n";
        // 拼成 render_histroy（typo），首行前缀 render_histr 无效；改前缀 render_histo 命中。
        let hint = nearest_match_hint(text, "render_history_typo_long_enough");
        assert!(hint.contains("closest line"), "got: {hint}");
    }

    #[test]
    fn nearest_match_hint_short_query_yields_empty() {
        assert_eq!(nearest_match_hint("abc", "ab"), "");
    }
}
