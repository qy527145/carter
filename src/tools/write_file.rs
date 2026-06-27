//! write_file：新建或整体覆盖写文件。

use serde_json::{json, Value};

use super::{arg_str, Tool, ToolResult};

pub struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "新建文件或整体覆盖写入。父目录不存在会自动创建。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径" },
                "content": { "type": "string", "description": "完整文件内容（覆盖写）" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let path = match arg_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let content = match arg_str(&args, "content") {
            Ok(c) => c,
            Err(e) => return e,
        };

        if let Some(parent) = std::path::Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return ToolResult::err(format!("cannot create parent dir for {path}: {e}"));
                }
            }
        }

        match tokio::fs::write(&path, &content).await {
            Ok(()) => ToolResult::ok(format!("wrote {} bytes to {path}", content.len())),
            Err(e) => ToolResult::err(format!("cannot write {path}: {e}")),
        }
    }
}
