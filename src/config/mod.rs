//! 配置加载。根目录 `~/.carter`（见 paths.rs）。
//! 加载顺序：内置默认 → `~/.carter/config.toml`（存在才读）→ env 插值。

mod interpolate;
pub mod paths;

pub use interpolate::interpolate;

use std::collections::HashMap;

use serde::Deserialize;

use crate::registry::ReasoningEffort;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub agent: AgentConfig,
    pub reasoning: ReasoningConfig,
    pub skills: SkillsConfig,
    pub mcp: McpConfig,
    pub providers: HashMap<String, ProviderConfig>,
    /// 启动时设置的环境变量（如 http_proxy / https_proxy）。在建任何 HTTP 客户端前生效。
    pub env: HashMap<String, String>,
    /// 调试开关。
    pub debug: DebugConfig,
}

/// 调试开关。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DebugConfig {
    /// 记录每次 LLM 请求的详情到独立的按天 jsonl 日志（默认 ~/.carter/debug/llm_log/）。
    pub log_requests: bool,
    /// 自定义 LLM 请求日志目录（省略用默认 ~/.carter/debug/llm_log）。
    pub llm_log_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub model: String,
    /// 上下文压缩 / 标题生成专用模型（`provider/model` 引用）。省略则回落主模型。
    pub fast_model: Option<String>,
    /// 自定义系统提示词文件路径（覆盖内置人设）。省略时回落约定位置
    /// `~/.carter/system.md`，再无则用内置默认。
    pub system_prompt_file: Option<String>,
    pub max_turns: u32,
    pub max_output_tokens: Option<u32>,
    /// 是否启用上下文自动压缩。
    pub compact_enabled: bool,
    /// 压缩触发阈值：真实 input token 超过 context_window * ratio 时压缩。
    pub compact_threshold_ratio: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReasoningConfig {
    pub effort: Option<ReasoningEffort>,
    pub show_thinking: bool,
}

/// Skills 开关。能力包目录 `~/.carter/skills/`。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    pub enabled: bool,
}

/// MCP 服务器集合。TOML 形如 `[mcp.servers.X]`。默认空（无 server）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub servers: HashMap<String, McpServerConfig>,
}

/// 单个 MCP server 定义。`transport = "stdio" | "http"`。
/// stdio：用 `command` + `args` + `env` 起子进程。
/// http：用 `url` + `headers` 连 streamable-http 端点。
/// `${ENV}` 占位经现有 `interpolate()` 自动展开。
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub transport: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// 用户面模型定义：嵌在 `[providers.X.models.Y]`，引用格式 `provider/model`。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// models.dev 的 `provider_id/model_id`（查元数据）。
    pub meta: String,
    /// 端点实际模型名（下发给 provider）。省略 = `meta` 的 model_id。
    #[serde(default)]
    pub api_name: Option<String>,
    /// 覆盖元数据的默认推理强度（可省）。
    #[serde(default)]
    pub default_reasoning: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub kind: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// provider 内部的模型定义：块名即模型名（`provider/model` 引用）。
    #[serde(default)]
    pub models: HashMap<String, ModelEntry>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agent: AgentConfig::default(),
            reasoning: ReasoningConfig::default(),
            skills: SkillsConfig::default(),
            mcp: McpConfig::default(),
            providers: HashMap::new(),
            env: HashMap::new(),
            debug: DebugConfig::default(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "sonnet".to_string(),
            fast_model: None,
            system_prompt_file: None,
            max_turns: 50,
            max_output_tokens: Some(16000),
            compact_enabled: true,
            compact_threshold_ratio: 0.75,
        }
    }
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            effort: Some(ReasoningEffort::Medium),
            show_thinking: true,
        }
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Config {
    /// 加载配置：内置默认；若 `~/.carter/config.toml` 存在则解析覆盖；最后 env 插值。
    pub fn load() -> crate::Result<Self> {
        let path = paths::config_path();
        let config = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let interpolated = interpolate(&raw);
            toml::from_str(&interpolated).map_err(crate::error::CarterError::TomlParse)?
        } else {
            Config::default()
        };
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_schema() {
        let toml = r#"
[agent]
model = "ws/sonnet"
max_turns = 30

[providers.ws]
kind = "anthropic"
base_url = "https://example/anthropic/v1/"
api_key = "secret"

  [providers.ws.models.sonnet]
  meta = "anthropic/claude-sonnet-4-5"
  api_name = "claude-sonnet-4-6"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.agent.model, "ws/sonnet");
        let p = cfg.providers.get("ws").unwrap();
        assert_eq!(p.kind, "anthropic");
        let entry = p.models.get("sonnet").unwrap();
        assert_eq!(entry.meta, "anthropic/claude-sonnet-4-5");
        assert_eq!(entry.api_name.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn api_name_optional() {
        let toml = r#"
[providers.p]
kind = "anthropic"

  [providers.p.models.haiku]
  meta = "anthropic/claude-haiku-4-5"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let entry = cfg.providers.get("p").unwrap().models.get("haiku").unwrap();
        assert!(entry.api_name.is_none());
    }

    #[test]
    fn skills_default_enabled_and_overridable() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.skills.enabled);

        let cfg: Config = toml::from_str("[skills]\nenabled = false\n").unwrap();
        assert!(!cfg.skills.enabled);
    }

    #[test]
    fn fast_model_default_none_and_parses() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.agent.fast_model.is_none());

        let cfg: Config = toml::from_str("[agent]\nfast_model = \"ws/haiku\"\n").unwrap();
        assert_eq!(cfg.agent.fast_model.as_deref(), Some("ws/haiku"));
    }

    #[test]
    fn env_and_debug_default_empty_and_parse() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.env.is_empty());
        assert!(!cfg.debug.log_requests);

        let toml = r#"
[env]
http_proxy = "http://127.0.0.1:7890"

[debug]
log_requests = true
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.env.get("http_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert!(cfg.debug.log_requests);
    }

    #[test]
    fn mcp_servers_default_empty_and_parse() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.mcp.servers.is_empty());

        let toml = r#"
[mcp.servers.fs]
transport = "stdio"
command = "npx"
args = ["-y", "server-filesystem", "/tmp"]
env = { TOK = "x" }

[mcp.servers.remote]
transport = "http"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer t" }
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let fs = cfg.mcp.servers.get("fs").unwrap();
        assert_eq!(fs.transport, "stdio");
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, vec!["-y", "server-filesystem", "/tmp"]);
        assert_eq!(fs.env.get("TOK").map(String::as_str), Some("x"));
        let remote = cfg.mcp.servers.get("remote").unwrap();
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(
            remote.headers.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );
    }
}
