//! 内置工具集 + 工具抽象。
//! 纪律：本模块**不得** import `genai::*`，只依赖 std/tokio/serde_json。
//! 工具规格经 `provider::ToolSpec` 下发给模型，结果以 `ToolResult` 结构化回灌。

mod bash;
mod edit_file;
mod glob_tool;
mod grep;
mod read_file;
mod save_memory;
mod todo_write;
mod write_file;

use std::sync::Arc;

use serde_json::Value;

use crate::provider::ToolSpec;

pub use todo_write::{parse_todos, TodoItem, TodoStatus};

/// 结构化工具输出（docs §4.4 简化版：去掉权限元数据）。
/// 错误即数据：失败不抛异常，包成 `ok=false` 回灌让模型自纠。
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub ok: bool,
    /// 给模型看的主体内容。
    pub content: String,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { ok: true, content: content.into() }
    }

    /// 失败结果。content 即结构化错误文本（回灌给模型）。
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, content: msg.into() }
    }

    /// 回灌给模型的最终字符串。失败时带明显前缀，便于模型识别并自纠。
    pub fn to_model_string(&self) -> String {
        if self.ok {
            self.content.clone()
        } else {
            format!("ERROR: {}", self.content)
        }
    }
}

/// 工具抽象。`execute` 永不返回 Err、永不 panic——所有失败进 `ToolResult`。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// 参数的 JSON Schema（下发给模型）。
    fn parameters(&self) -> Value;
    async fn execute(&self, args: Value) -> ToolResult;
}

/// 工具注册表：持有 `Arc<dyn Tool>` 列表，按名分发。
/// 用 Arc 是为了让子 agent 派生时复用同一份工具实例（无内部可变状态，多个 agent 并发执行安全）。
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// 空注册表（子 agent 用：通过工厂闭包按需重建工具池）。
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    /// 注册第一梯队 6 个内置工具。
    pub fn builtin() -> Self {
        Self {
            tools: vec![
                Arc::new(read_file::ReadFile),
                Arc::new(write_file::WriteFile),
                Arc::new(edit_file::EditFile),
                Arc::new(bash::Bash),
                Arc::new(glob_tool::Glob),
                Arc::new(grep::Grep),
                Arc::new(todo_write::TodoWrite),
                Arc::new(save_memory::SaveMemory),
            ],
        }
    }

    /// 运行期追加工具（Skills 的 `skill`、子 agent 的 `task`、MCP 工具）。
    /// 接收 `Arc<dyn Tool>` 让调用方可以同时把工具放进多个 registry（如父 + 子 agent 工厂）。
    pub fn push(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    /// 暴露已注册的工具 Arc 切片，供子 agent 工厂复用。
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// 转成下发给模型的工具规格。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// 按名分发执行。找不到工具→结构化 err（回灌让模型改用别的工具）。
    pub async fn dispatch(&self, name: &str, args: Value) -> ToolResult {
        match self.tools.iter().find(|t| t.name() == name) {
            Some(t) => t.execute(args).await,
            None => ToolResult::err(format!("unknown tool: {name}")),
        }
    }
}

/// 工具是否**并发安全**（多个调用可同时执行）。
///
/// 安全：只读 / 幂等结构化输入（read_file/grep/glob/todo_write/skill/save_memory 追加写）。
/// 不安全：会副作用到外部世界且与顺序相关的（bash 子进程、write_file/edit_file 写盘、task 子 agent、MCP）。
///
/// `task` 与 MCP 工具默认按 unsafe 处理：前者可能间接做任何事；后者契约未知。
/// 真要为某 MCP 工具开并发，可在该工具实现里返回更精细的标记（后续扩展点）。
pub fn is_concurrent_safe(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file" | "grep" | "glob" | "todo_write" | "skill"
    )
}

/// 工具内部辅助：从 args 取必填字符串字段。
fn arg_str(args: &Value, key: &str) -> Result<String, ToolResult> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolResult::err(format!("missing or non-string argument: {key}")))
}

/// 工具内部辅助：取可选 u64 字段。
fn arg_u64_opt(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

/// 工具内部辅助：取可选 bool 字段，缺省 false。
fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// 大输出截断（防爆 token）。
pub(crate) fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // 按 char 边界安全截断。
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n... [truncated {} bytes]",
        &s[..end],
        s.len() - end
    )
}
