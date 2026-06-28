//! save_memory：把一条事实 / 偏好追加进分层记忆文件（CARTER.md），跨会话持久。
//! 项目级写 `<cwd>/CARTER.md`，全局级写 `~/.carter/CARTER.md`；下次启动经 `memory` 模块
//! 自动注入 system。仅追加、不重写既有内容；相同事实去重。

use serde_json::{json, Value};

use super::{arg_str, Tool, ToolResult};

/// save_memory 追加的归集小节标题（便于人和工具识别这批由模型写入的记忆）。
const SECTION: &str = "## Carter 记忆（save_memory）";

pub struct SaveMemory;

/// 解析 scope → 目标记忆文件路径。默认 project。未知值 → Err 文本。
fn target_path(scope: &str) -> Result<std::path::PathBuf, String> {
    match scope {
        "project" => Ok(std::path::PathBuf::from("CARTER.md")),
        "global" => Ok(crate::config::paths::global_memory_path()),
        other => Err(format!("未知 scope: {other}（仅支持 project | global）")),
    }
}

#[async_trait::async_trait]
impl Tool for SaveMemory {
    fn name(&self) -> &str {
        "save_memory"
    }

    fn description(&self) -> &str {
        "把一条需要跨会话记住的事实 / 用户偏好 / 项目约定追加进记忆文件（CARTER.md），\
         下次启动会自动注入。scope=project 写当前项目（默认），global 写全局。\
         只在确属持久、可复用的信息时使用；一次只记一条、精炼成一句话。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fact": { "type": "string", "description": "要记住的一条事实 / 偏好，精炼成一句话" },
                "scope": {
                    "type": "string",
                    "enum": ["project", "global"],
                    "description": "project=当前项目 CARTER.md（默认）；global=~/.carter/CARTER.md"
                }
            },
            "required": ["fact"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let fact = match arg_str(&args, "fact") {
            Ok(f) => f.trim().to_string(),
            Err(e) => return e,
        };
        if fact.is_empty() {
            return ToolResult::err("fact 不能为空");
        }
        // 单行事实：把内部换行压成空格，保证一条 = 一个 markdown bullet。
        let fact = fact.split_whitespace().collect::<Vec<_>>().join(" ");

        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("project");
        let path = match target_path(scope) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let bullet = format!("- {fact}");
        // 去重：逐行精确比对，已存在则不重复写。
        if existing.lines().any(|l| l.trim() == bullet) {
            return ToolResult::ok(format!("已存在相同记忆（{scope}），未重复写入：{fact}"));
        }

        let updated = append_bullet(&existing, &bullet);

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return ToolResult::err(format!("无法创建目录 {}: {e}", parent.display()));
                }
            }
        }
        match tokio::fs::write(&path, &updated).await {
            Ok(()) => ToolResult::ok(format!("已记住（{scope} → {}）：{fact}", path.display())),
            Err(e) => ToolResult::err(format!("写入 {} 失败：{e}", path.display())),
        }
    }
}

/// 把 bullet 追加进记忆正文：无内容 → 建标题骨架；无归集小节 → 追加小节；否则尾部追加。
fn append_bullet(existing: &str, bullet: &str) -> String {
    if existing.trim().is_empty() {
        return format!("# Carter 记忆\n\n{SECTION}\n\n{bullet}\n");
    }
    let mut out = existing.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.contains(SECTION) {
        out.push_str(&format!("\n{SECTION}\n"));
    }
    out.push_str(&format!("{bullet}\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_to_empty_creates_skeleton() {
        let out = append_bullet("", "- 用户偏好中文回答");
        assert!(out.contains("# Carter 记忆"));
        assert!(out.contains(SECTION));
        assert!(out.contains("- 用户偏好中文回答"));
    }

    #[test]
    fn append_adds_section_when_missing() {
        let out = append_bullet("# 既有记忆\n\n- 旧事实\n", "- 新事实");
        assert!(out.contains("- 旧事实"));
        assert!(out.contains(SECTION));
        assert!(out.trim_end().ends_with("- 新事实"));
    }

    #[test]
    fn append_reuses_existing_section() {
        let base = format!("# t\n\n{SECTION}\n\n- a\n");
        let out = append_bullet(&base, "- b");
        // 不应重复出现小节标题。
        assert_eq!(out.matches(SECTION).count(), 1);
        assert!(out.contains("- a") && out.contains("- b"));
    }

    #[test]
    fn target_path_resolves_scopes() {
        assert_eq!(target_path("project").unwrap(), std::path::PathBuf::from("CARTER.md"));
        assert!(target_path("global").is_ok());
        assert!(target_path("xxx").is_err());
    }
}
