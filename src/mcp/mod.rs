//! MCP 接入 —— 复用官方 `rmcp` SDK 把外部 MCP server 的工具透明并入工具集。
//!
//! 隔离纪律（神圣）：`rmcp::*` 只允许出现在本目录（`src/mcp/`）。跨模块边界的公开签名
//! 只能是 `dyn crate::tools::Tool` / std / serde_json —— rmcp 类型绝不外泄（R6）。
//!
//! 生命周期（R5）：`serve(transport)` 返回的 `RunningService` 必须存活，drop 即拆传输、
//! 杀 stdio 子进程。`McpManager` 会话级持有全部 service，活过每个 turn；每个 `McpTool` 只持
//! 克隆的 peer 句柄（Clone+Send+Sync）。退出时显式 `shutdown()` 回收子进程。

mod manager;
mod tool;
mod transport;

pub use manager::McpManager;
