//! `skill` 工具：模型按名加载 SKILL.md 正文到下一轮 context。

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::tools::{Tool, ToolResult};

pub struct SkillTool {
    skills_dir: PathBuf,
}

impl SkillTool {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self { skills_dir }
    }
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "按名加载一个 Skill 的完整说明（SKILL.md 正文）。返回的正文即该能力的执行指引，加载后据此完成任务。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill 名（见系统提示里的 Skills 目录）" }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let name = match args.get("name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n,
            _ => return ToolResult::err("missing or non-string argument: name"),
        };
        match crate::skills::load(&self.skills_dir, name) {
            Some(skill) => ToolResult::ok(skill.body),
            None => ToolResult::err(format!("unknown skill: {name}")),
        }
    }
}
