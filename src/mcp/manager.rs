//! `McpManager` —— 会话级持有全部 MCP `RunningService`，活过每个 turn。
//! `start` 逐个 server 建连 + 列工具 → 映射成 `McpTool`（克隆 peer）；失败/超时跳过不致命（R7）。
//! `shutdown` 逐个 cancel，回收 stdio 子进程（R5）。

use std::time::Duration;

use serde_json::Value;

use crate::config::McpConfig;
use crate::tools::Tool;

use super::tool::McpTool;
use super::transport::{self, Service};

/// 单 server 建连 + 握手 + 列工具的超时（防慢 server 卡死整体启动 — R7）。
const PER_SERVER_TIMEOUT: Duration = Duration::from_secs(15);

pub struct McpManager {
    /// 必须存活：drop 即拆传输、杀 stdio 子进程。
    services: Vec<Service>,
}

impl McpManager {
    /// 按配置启动所有 server，返回 (manager, 工具列表)。
    /// 任一 server 失败/超时 → emit 警告并跳过（呼应 provider-fallback 风格，不致命）。
    pub async fn start(cfg: &McpConfig) -> (Self, Vec<std::sync::Arc<dyn Tool>>) {
        let mut services = Vec::new();
        let mut tools: Vec<std::sync::Arc<dyn Tool>> = Vec::new();

        // 名字排序：启动顺序可复现。
        let mut names: Vec<&String> = cfg.servers.keys().collect();
        names.sort();

        for name in names {
            let server_cfg = &cfg.servers[name];
            match tokio::time::timeout(PER_SERVER_TIMEOUT, transport::connect(server_cfg)).await {
                Ok(Ok(service)) => match Self::collect_tools(&service, name).await {
                    Ok(server_tools) => {
                        tracing::info!(server = %name, count = server_tools.len(), "mcp server connected");
                        tools.extend(server_tools);
                        services.push(service);
                    }
                    Err(e) => {
                        tracing::warn!(server = %name, error = %e, "mcp list_tools failed; skipping server");
                    }
                },
                Ok(Err(e)) => {
                    tracing::warn!(server = %name, error = %e, "mcp connect failed; skipping server");
                }
                Err(_) => {
                    tracing::warn!(server = %name, timeout = ?PER_SERVER_TIMEOUT, "mcp connect timed out; skipping server");
                }
            }
        }

        (Self { services }, tools)
    }

    /// 列远端工具并映射成 `McpTool`（每个持克隆 peer）。
    async fn collect_tools(service: &Service, server: &str) -> Result<Vec<std::sync::Arc<dyn Tool>>, String> {
        let listed = service
            .list_all_tools()
            .await
            .map_err(|e| format!("{e}"))?;

        let mut out: Vec<std::sync::Arc<dyn Tool>> = Vec::new();
        for t in listed {
            let schema = Value::Object((*t.input_schema).clone());
            let description = t.description.map(|d| d.into_owned()).unwrap_or_default();
            out.push(std::sync::Arc::new(McpTool::new(
                service.peer().clone(),
                server,
                t.name.into_owned(),
                description,
                schema,
            )));
        }
        Ok(out)
    }

    /// 优雅关停：逐个 cancel，回收 stdio 子进程。
    pub async fn shutdown(self) {
        for service in self.services {
            if let Err(e) = service.cancel().await {
                tracing::warn!(error = %e, "mcp service shutdown error");
            }
        }
    }
}
