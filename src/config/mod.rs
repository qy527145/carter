//! 配置加载。根目录 `~/.carter`（见 paths.rs）。
//! 加载顺序：内置默认 → `~/.carter/config.toml`（存在才读）→ env 插值。

mod interpolate;
pub mod paths;

pub use interpolate::interpolate;

use std::collections::HashMap;

use serde::Deserialize;

use llm_adapt_core::{ClientConfig, ProtocolId};

use crate::error::{CarterError, Result};
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
    /// 生命周期 hook 列表（来自 `[[hooks]]` 段）。
    #[serde(default)]
    pub hooks: Vec<crate::hooks::HookConfig>,
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
    /// 子代理链最大深度（主 agent = 0，每层 +1）。缺省 3。
    pub subagent_max_depth: Option<u32>,
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
    /// 是否启用。false = 配置留着但不连接。
    ///
    /// 比「删掉再写回来」方便得多 —— 排障时常常要临时关掉某个 server，
    /// 而它的 command/args/env 又长又难复原。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
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

    /// 要声明的**服务端工具**（厂商自己执行的，如 `web_search`）。
    ///
    /// 它由厂商执行，本地没有可跑的东西。认不出的名字、或该协议没有对应物的
    /// （Chat Completions 全都没有），会在启动时被报出来而不是静默丢弃 ——
    /// 否则用户以为开了联网搜索，模型其实一个结果都拿不到。
    #[serde(default)]
    pub server_tools: Vec<String>,
    /// 原样下发的服务端工具声明。
    ///
    /// 逃生舱：厂商给这些工具带版本号（`web_search_20250305`），内置表必然滞后，
    /// 或者你要用的参数超出内置表覆盖范围时，在这里直接给一个对象。
    #[serde(default)]
    pub server_tool_specs: Vec<serde_json::Value>,
    /// 服务端工具在单次响应里的调用次数上限（Anthropic 的 `max_uses`）。
    #[serde(default)]
    pub server_tool_max_uses: Option<u32>,
}

/// 渠道种类 → (线协议, 默认端点, 覆盖路径)。
///
/// `kind` 是用户面写的东西，允许沿用旧名（`openai` / `openai_compat` / `custom` / `gemini` …），
/// 由 [`ProtocolId::new`] 做别名归一。带覆盖路径的那两个是**路径与官方不同**的兼容端点：
/// Gemini 的 OpenAI 兼容端点在 `/v1beta/openai/chat/completions`，不是 `/v1/chat/completions`。
fn kind_preset(kind: &str) -> (ProtocolId, Option<&'static str>, Option<&'static str>) {
    match kind.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "anthropic_v2" | "anthropic_messages" | "messages" => (
            ProtocolId::new(ProtocolId::ANTHROPIC),
            Some("https://api.anthropic.com"),
            None,
        ),
        "openai_responses" | "responses" => (
            ProtocolId::new(ProtocolId::OPENAI_RESPONSES),
            Some("https://api.openai.com"),
            None,
        ),
        "gemini" | "google" => (
            ProtocolId::new(ProtocolId::OPENAI_CHAT),
            Some("https://generativelanguage.googleapis.com/v1beta/openai"),
            Some("/chat/completions"),
        ),
        "ollama" => (
            ProtocolId::new(ProtocolId::OPENAI_CHAT),
            Some("http://localhost:11434/v1"),
            None,
        ),
        "groq" => (
            ProtocolId::new(ProtocolId::OPENAI_CHAT),
            Some("https://api.groq.com/openai/v1"),
            None,
        ),
        "deepseek" => (
            ProtocolId::new(ProtocolId::OPENAI_CHAT),
            Some("https://api.deepseek.com"),
            None,
        ),
        "xai" | "grok" => (
            ProtocolId::new(ProtocolId::OPENAI_CHAT),
            Some("https://api.x.ai/v1"),
            None,
        ),
        "openrouter" => (
            ProtocolId::new(ProtocolId::OPENAI_CHAT),
            Some("https://openrouter.ai/api/v1"),
            None,
        ),
        // openai / openai_compat / custom 以及一切未知值：OpenAI Chat 协议。
        // 未知值必须自带 `base_url`，否则下面会报错而不是偷偷打到 api.openai.com。
        _ => (ProtocolId::new(ProtocolId::OPENAI_CHAT), None, None),
    }
}

impl ProviderConfig {
    /// 这个渠道走哪种线协议。
    pub fn protocol(&self) -> ProtocolId {
        kind_preset(&self.kind).0
    }

    /// 解析成底座要的客户端配置。
    ///
    /// 缺少端点且种类无法辨认时**报错**，不猜 —— 猜错的后果是把请求（连同 API key）
    /// 发到一个你没打算用的域名上。
    pub fn to_client_config(&self) -> Result<ClientConfig> {
        let (_, preset_base, preset_path) = kind_preset(&self.kind);
        let base_url = self
            .base_url
            .clone()
            .or_else(|| preset_base.map(str::to_string))
            .ok_or_else(|| {
                CarterError::Config(format!(
                    "provider kind `{}` 无法辨认，且没有配 base_url ——                      请补上 base_url，或把 kind 改成内置支持的名字",
                    self.kind
                ))
            })?;
        Ok(ClientConfig {
            base_url,
            path: preset_path.map(str::to_string),
            api_key: self.api_key.clone().unwrap_or_default(),
            ..Default::default()
        })
    }
}

impl Config {
    /// 取某个 provider 的配置。`[providers.*]` 里没有的话按名字合成一份，
    /// API key 从 `<NAME>_API_KEY` 环境变量取。
    ///
    /// 保留这种「不写配置也能跑」的开箱体验是刻意的：换底座之前
    /// （genai）就是这么工作的，突然要求人人都写 `[providers.*]` 是没必要的破坏。
    pub fn provider_for(&self, name: &str) -> ProviderConfig {
        if let Some(p) = self.providers.get(name) {
            return p.clone();
        }
        let env_key = format!(
            "{}_API_KEY",
            name.to_ascii_uppercase().replace(['-', '.', '/'], "_")
        );
        ProviderConfig {
            kind: name.to_string(),
            api_key: std::env::var(&env_key).ok(),
            base_url: None,
            models: HashMap::new(),
            server_tools: Vec::new(),
            server_tool_specs: Vec::new(),
            server_tool_max_uses: None,
        }
    }
}

fn default_enabled() -> bool {
    true
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
            hooks: Vec::new(),
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
            subagent_max_depth: None,
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
