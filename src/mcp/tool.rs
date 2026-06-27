//! `McpTool` —— 把一个 MCP server 的单个工具适配成 `crate::tools::Tool`。
//! 持克隆的 peer 句柄（非 owning service）；`execute` 经 peer 调远端工具。
//! 对外工具名 `mcp__{server}__{tool}` 防与内置 / 其它 server 撞名。
//! 绝不 panic / 绝不外抛 Err —— 所有失败包成 `ToolResult{ok:false}`（R6）。

use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::service::{Peer, RoleClient};
use serde_json::Value;

use crate::tools::{Tool, ToolResult};

/// 远端工具输出截断上限（防爆 token）。
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

pub struct McpTool {
    peer: Peer<RoleClient>,
    /// 对外暴露名 `mcp__{server}__{tool}`。
    full_name: String,
    /// 远端真实工具名（调用时下发）。
    remote_name: String,
    description: String,
    schema: Value,
}

impl McpTool {
    pub fn new(
        peer: Peer<RoleClient>,
        server: &str,
        remote_name: String,
        description: String,
        schema: Value,
    ) -> Self {
        let full_name = format!("mcp__{server}__{remote_name}");
        Self {
            peer,
            full_name,
            remote_name,
            description,
            schema,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, args: Value) -> ToolResult {
        // arguments 必须是 JSON object（或缺省）；其它类型 → 结构化 err 让模型自纠。
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return ToolResult::err(format!(
                    "tool arguments must be a JSON object, got: {other}"
                ))
            }
        };

        let mut params = CallToolRequestParams::new(self.remote_name.clone());
        if let Some(map) = arguments {
            params = params.with_arguments(map);
        }

        let result = match self.peer.call_tool(params).await {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("mcp call failed: {e}")),
        };

        let text = flatten_content(&result);
        if result.is_error.unwrap_or(false) {
            ToolResult::err(text)
        } else {
            ToolResult::ok(crate::tools::truncate(&text, MAX_OUTPUT_BYTES))
        }
    }
}

/// 拍平 `CallToolResult` 的内容块为纯文本：取所有 text 块拼接；非文本块标注类型。
fn flatten_content(result: &rmcp::model::CallToolResult) -> String {
    let mut out = String::new();
    for c in &result.content {
        match &c.raw {
            RawContent::Text(t) => out.push_str(&t.text),
            RawContent::Image(_) => out.push_str("[image content omitted]"),
            RawContent::Audio(_) => out.push_str("[audio content omitted]"),
            RawContent::Resource(_) => out.push_str("[resource content omitted]"),
            RawContent::ResourceLink(_) => out.push_str("[resource link omitted]"),
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}
