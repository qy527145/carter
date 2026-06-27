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
fn apply_edit(text: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("old_string must not be empty".to_string());
    }
    let count = text.matches(old).count();
    if count == 0 {
        return Err(format!("old_string not found: {old:?}"));
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string appears {count} times; pass replace_all=true or provide a more specific string"
        ));
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    Ok((updated, count))
}

#[cfg(test)]
mod tests {
    use super::apply_edit;

    #[test]
    fn unique_replace() {
        let (out, n) = apply_edit("hello world", "world", "rust", false).unwrap();
        assert_eq!(out, "hello rust");
        assert_eq!(n, 1);
    }

    #[test]
    fn not_found_errors() {
        assert!(apply_edit("abc", "xyz", "q", false).is_err());
    }

    #[test]
    fn ambiguous_without_replace_all_errors() {
        assert!(apply_edit("a a a", "a", "b", false).is_err());
    }

    #[test]
    fn replace_all_works() {
        let (out, n) = apply_edit("a a a", "a", "b", true).unwrap();
        assert_eq!(out, "b b b");
        assert_eq!(n, 3);
    }
}
