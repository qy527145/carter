//! read_file：读文件，cat -n 风格行号输出，支持 offset/limit。
//! 图片文件（png/jpg/jpeg/gif/webp/bmp）会被存入多模态 image store 并返回 `[img:...]` 引用 token。

use serde_json::{json, Value};

use super::{arg_str, arg_u64_opt, truncate, Tool, ToolResult};
use crate::media;

pub struct ReadFile;

const MAX_OUTPUT: usize = 50_000;
const DEFAULT_LIMIT: u64 = 2000;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "读取文件内容。文本文件以带行号（cat -n 风格）输出，支持 offset/limit 按行范围读取；\
         图片文件（png/jpg/gif/webp/bmp）被存入多模态资源并返回 `[img:<hash>.<ext>]` 引用 token，\
         可直接嵌入回答或后续消息中——模型在后续轮会看到真实图像内容。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径（相对或绝对）" },
                "offset": { "type": "integer", "description": "起始行号（1-based，缺省 1，仅文本文件）" },
                "limit": { "type": "integer", "description": "读取行数（缺省 2000，仅文本文件）" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let path = match arg_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };

        // 图片快路径：按扩展名 + 字节嗅探（防伪装），存入 image store 后回引用 token。
        let p = std::path::Path::new(&path);
        if media::is_image_path(p) {
            return match media::put_path(p) {
                Ok(rf) => ToolResult::ok(format!(
                    "{}\n[image stored: {} bytes, {} mime]",
                    rf.token(),
                    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
                    rf.mime(),
                )),
                Err(e) => ToolResult::err(format!("cannot read image {path}: {e}")),
            };
        }

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
