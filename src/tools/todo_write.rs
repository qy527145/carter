//! todo_write：待办读写工具（注意力复诵）。模型每次传完整列表，整表覆盖。
//! 工具的 execute 只校验+回显；真正写入 `Thread.todos` 由 turn.rs 在 dispatch 后特判。
//! 类型 + 解析定义在此（tools 是 agent 的下层，agent 可反向 import）。

use serde_json::{json, Value};

use super::{Tool, ToolResult};

/// 单条待办的状态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

/// 一条待办项（注意力复诵用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    /// 祈使形描述（待办内容）。
    pub content: String,
    /// 状态。
    pub status: TodoStatus,
    /// 进行中态展示文案（present-continuous）。
    pub active_form: String,
}

/// 从工具参数解析 todo 列表。turn.rs 与本工具共用。
/// 失败返回结构化错误文本（回灌给模型）。
pub fn parse_todos(args: &Value) -> Result<Vec<TodoItem>, String> {
    let arr = args
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing or non-array argument: todos".to_string())?;

    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let content = item
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("todos[{i}]: missing or non-string `content`"))?;
        let status_str = item
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("todos[{i}]: missing or non-string `status`"))?;
        let status = TodoStatus::from_str(status_str).ok_or_else(|| {
            format!("todos[{i}]: invalid status `{status_str}` (expected pending/in_progress/completed)")
        })?;
        // active_form 可选，缺省回退到 content。
        let active_form = item
            .get("active_form")
            .and_then(Value::as_str)
            .unwrap_or(content)
            .to_string();
        out.push(TodoItem {
            content: content.to_string(),
            status,
            active_form,
        });
    }
    Ok(out)
}

pub struct TodoWrite;

#[async_trait::async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "维护任务待办列表（整表覆盖）。用于多步任务规划与进度追踪。每次传完整列表，\
         status 取 pending/in_progress/completed；建议同一时刻仅一项 in_progress。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "完整待办列表（覆盖旧表）",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "待办内容（祈使形）" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "状态"
                            },
                            "active_form": { "type": "string", "description": "进行中展示文案（present-continuous）" }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        // 仅校验 + 回显；写入 Thread.todos 由 turn.rs 特判完成。
        match parse_todos(&args) {
            Ok(todos) => {
                let done = todos.iter().filter(|t| t.status == TodoStatus::Completed).count();
                let in_prog = todos.iter().filter(|t| t.status == TodoStatus::InProgress).count();
                ToolResult::ok(format!(
                    "todos updated: {} total ({} completed, {} in progress)",
                    todos.len(),
                    done,
                    in_prog
                ))
            }
            Err(e) => ToolResult::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_valid_table() {
        let args = json!({
            "todos": [
                { "content": "build", "status": "completed", "active_form": "Building" },
                { "content": "test", "status": "in_progress", "active_form": "Testing" }
            ]
        });
        let todos = parse_todos(&args).unwrap();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].status, TodoStatus::Completed);
        assert_eq!(todos[1].active_form, "Testing");
    }

    #[test]
    fn invalid_status_errors() {
        let args = json!({ "todos": [{ "content": "x", "status": "bogus" }] });
        assert!(parse_todos(&args).is_err());
    }

    #[test]
    fn missing_todos_errors() {
        assert!(parse_todos(&json!({})).is_err());
    }

    #[test]
    fn empty_table_ok() {
        let todos = parse_todos(&json!({ "todos": [] })).unwrap();
        assert!(todos.is_empty());
    }

    #[test]
    fn active_form_defaults_to_content() {
        let args = json!({ "todos": [{ "content": "do thing", "status": "pending" }] });
        let todos = parse_todos(&args).unwrap();
        assert_eq!(todos[0].active_form, "do thing");
    }
}
