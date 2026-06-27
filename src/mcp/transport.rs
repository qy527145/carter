//! MCP 传输构建 —— stdio 子进程 / streamable-http(reqwest)。
//! 所有 rmcp transport 类型限于此文件。对 manager 只暴露「连接好的 RunningService」。

use std::collections::HashMap;
use std::sync::Arc;

use http::{HeaderName, HeaderValue};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::ServiceExt;

use crate::config::McpServerConfig;

/// 连接结果：rmcp 客户端运行态。drop 即拆传输 / 杀 stdio 子进程。
pub type Service = RunningService<RoleClient, ()>;

/// 按配置建传输并完成 MCP 握手，返回运行态 service。
/// `transport` 非法 / 缺字段 / 握手失败 → Err（字符串，不外泄 rmcp 错误类型）。
pub async fn connect(cfg: &McpServerConfig) -> Result<Service, String> {
    match cfg.transport.as_str() {
        "stdio" => connect_stdio(cfg).await,
        "http" => connect_http(cfg).await,
        other => Err(format!("unknown transport: {other} (expected \"stdio\" or \"http\")")),
    }
}

async fn connect_stdio(cfg: &McpServerConfig) -> Result<Service, String> {
    let command = cfg
        .command
        .as_deref()
        .ok_or_else(|| "stdio transport requires `command`".to_string())?;

    let args = cfg.args.clone();
    let env = cfg.env.clone();
    let transport = TokioChildProcess::new(tokio::process::Command::new(command).configure(
        |c| {
            c.args(&args);
            for (k, v) in &env {
                c.env(k, v);
            }
        },
    ))
    .map_err(|e| format!("spawn failed: {e}"))?;

    ().serve(transport)
        .await
        .map_err(|e| format!("handshake failed: {e}"))
}

async fn connect_http(cfg: &McpServerConfig) -> Result<Service, String> {
    let url = cfg
        .url
        .as_deref()
        .ok_or_else(|| "http transport requires `url`".to_string())?;

    let headers = build_headers(&cfg.headers)?;
    let http_cfg = StreamableHttpClientTransportConfig::with_uri(Arc::<str>::from(url))
        .custom_headers(headers);
    let transport = StreamableHttpClientTransport::from_config(http_cfg);

    ().serve(transport)
        .await
        .map_err(|e| format!("handshake failed: {e}"))
}

/// 把字符串 header map 转成 http 的 `HeaderName`/`HeaderValue`（限本文件，不外泄）。
fn build_headers(
    raw: &HashMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, String> {
    let mut out = HashMap::new();
    for (k, v) in raw {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| format!("invalid header name {k:?}: {e}"))?;
        let val =
            HeaderValue::from_str(v).map_err(|e| format!("invalid header value for {k:?}: {e}"))?;
        out.insert(name, val);
    }
    Ok(out)
}
