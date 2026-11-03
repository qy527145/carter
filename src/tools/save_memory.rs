//! save_memory：把一条事实 / 偏好追加进分层记忆文件，跨会话持久。
//!
//! 落盘逻辑在 `memory::save`（路径解析、去重、原子写 + 修订快照），这里只做参数解析。
//!
//! 项目级记忆写 `<project_dir>/CARTER.md`。`project_dir` 由构造方给定 —— 命令行下就是
//! 进程 cwd，但嵌入方（桌面应用）的进程 cwd 与会话目录不是一回事，必须显式传进来。

use std::path::PathBuf;

use serde_json::{Value, json};

use super::{Tool, ToolResult};
use crate::memory::{self, Kind, Saved, Scope};

pub struct SaveMemory {
    /// 项目级记忆所在目录。`None` = 进程 cwd。
    project_dir: Option<PathBuf>,
}

impl SaveMemory {
    /// 项目级记忆写到 `project_dir/CARTER.md`。
    pub fn new(project_dir: PathBuf) -> Self {
        Self {
            project_dir: Some(project_dir),
        }
    }

    /// 项目级记忆跟随进程 cwd（命令行用）。
    pub fn in_process_cwd() -> Self {
        Self { project_dir: None }
    }

    fn project_dir(&self) -> PathBuf {
        self.project_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// 工具描述。重点写「什么时候该记」：只写克制条件，模型的默认倾向就是不调用。
const DESCRIPTION: &str = "\
把值得跨会话记住的信息写进持久记忆，下次会话开始时会自动出现在系统提示里。

出现下面任一情况就应该调用（不必等用户说「记住」）：
- 用户纠正了你的做法，或者说了「以后」「别再」「每次都」「记住」「我习惯」之类的话
- 用户透露了自己的身份、角色、偏好、沟通习惯 → kind=profile
- 你试错后才搞清楚的构建 / 测试 / 运行命令、环境坑、项目约定 → kind=facts, scope=project
- 本机通用的环境事实（代理、包管理器偏好、工具路径）→ kind=facts, scope=user

不要记：一次性的任务细节、从代码或 git 历史里直接看得到的东西、密钥凭据。
内容写成一句能独立看懂的话，带上「为什么」更好。一次一条；记完在回复里用一句话告诉用户记了什么。";

#[async_trait::async_trait]
impl Tool for SaveMemory {
    fn name(&self) -> &str {
        "save_memory"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "要记住的内容：一句能脱离上下文看懂的话"
                },
                "kind": {
                    "type": "string",
                    "enum": ["facts", "profile"],
                    "description": "facts（默认）= 项目或环境事实；profile = 用户本人的身份与偏好（总是用户级）"
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "user"],
                    "description": "仅 kind=facts 时生效：project（默认）= 当前项目的 CARTER.md；user = 跨项目的 ~/.carter/facts.md"
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        // `fact` 是旧版参数名。
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .or_else(|| args.get("fact").and_then(Value::as_str))
            .unwrap_or("");
        let kind = match Kind::parse(args.get("kind").and_then(Value::as_str).unwrap_or("facts")) {
            Ok(k) => k,
            Err(e) => return ToolResult::err(e),
        };
        let scope = match Scope::parse(
            args.get("scope")
                .and_then(Value::as_str)
                .unwrap_or("project"),
        ) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(e),
        };

        match memory::save(content, kind, scope, &self.project_dir()) {
            Ok(Saved::Written(path)) => {
                ToolResult::ok(format!("已记住（{} → {}）", kind.as_str(), path.display()))
            }
            Ok(Saved::Duplicate(path)) => {
                ToolResult::ok(format!("{} 里已有这条，未重复写入", path.display()))
            }
            Err(e) => ToolResult::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_project_memory_under_given_dir() {
        let dir =
            std::env::temp_dir().join(format!("carter-save-mem-{}", crate::session::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = SaveMemory::new(dir.clone());
        let res = tool
            .execute(json!({"content": "测试命令是 cargo nextest run"}))
            .await;
        assert!(res.ok, "{}", res.content);
        let body = std::fs::read_to_string(dir.join("CARTER.md")).unwrap();
        assert!(body.contains("- 测试命令是 cargo nextest run"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_unknown_kind_and_empty_content() {
        let tool = SaveMemory::new(std::env::temp_dir());
        assert!(
            !tool
                .execute(json!({"content": "x", "kind": "skill"}))
                .await
                .ok
        );
        assert!(!tool.execute(json!({"content": "  "})).await.ok);
    }
}
