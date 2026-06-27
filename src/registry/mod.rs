//! ModelRegistry —— 模型元数据解析。
//! 元数据来源 = models.dev 缓存（`~/.carter/models.json`）；用户面绑定在 config 的 `[models.X]`。

mod model;
pub mod fetch;
pub mod models_dev;

#[allow(unused_imports)]
pub use model::{Capability, ModelInfo, Pricing, ReasoningEffort};

use crate::config::Config;
use crate::error::CarterError;

/// 解析模型引用 `provider/model` → 完整 `ModelInfo`（含端点 api_name + provider 覆盖）。
/// 链：拆引用为 (provider_name, model_name) → config.providers[provider].models[model]
///     → 拆 entry.meta 为 (meta_provider, model_id) → models.dev 缓存查元数据
///     → 用 entry.api_name / provider_name / entry.default_reasoning 覆盖。
pub fn resolve_model(
    config: &Config,
    cache_json: &str,
    reference: &str,
) -> crate::Result<ModelInfo> {
    let (provider_name, model_name) = reference.split_once('/').ok_or_else(|| {
        CarterError::Config(format!(
            "模型引用 `{reference}` 格式应为 `provider/model`（如 `ws/sonnet`）。"
        ))
    })?;

    let provider = config.providers.get(provider_name).ok_or_else(|| {
        CarterError::Config(format!(
            "config 中未定义 provider `{provider_name}`；请添加 [providers.{provider_name}]。"
        ))
    })?;

    let entry = provider.models.get(model_name).ok_or_else(|| {
        CarterError::Config(format!(
            "provider `{provider_name}` 下未定义模型 `{model_name}`；请添加 [providers.{provider_name}.models.{model_name}]。"
        ))
    })?;

    let (meta_provider, model_id) = entry.meta.split_once('/').ok_or_else(|| {
        CarterError::Config(format!(
            "模型 `{reference}` 的 meta `{}` 格式应为 `provider_id/model_id`。",
            entry.meta
        ))
    })?;

    let mut info = models_dev::lookup(cache_json, meta_provider, model_id).ok_or_else(|| {
        CarterError::Config(format!(
            "models.dev 缓存中找不到 `{}`；请 `carter update` 或检查 meta。",
            entry.meta
        ))
    })?;

    // 用户面绑定覆盖：provider 指向 config 的 [providers.X]；api_name 下发给端点。
    info.provider = provider_name.to_string();
    info.api_name = entry.api_name.clone().unwrap_or_else(|| model_id.to_string());
    if entry.default_reasoning.is_some() {
        info.default_reasoning = entry.default_reasoning.clone();
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CACHE: &str = r#"{
      "anthropic": { "models": { "claude-sonnet-4-5": {
        "id": "claude-sonnet-4-5", "tool_call": true, "reasoning": true,
        "reasoning_options": [{"type":"budget_tokens"}],
        "modalities": {"input":["text","image"]},
        "limit": {"context": 200000, "output": 64000},
        "cost": {"input": 3, "output": 15, "cache_read": 0.3}
      }}}
    }"#;

    fn config_with_model() -> Config {
        let toml = r#"
[providers.ws]
kind = "anthropic"

  [providers.ws.models.sonnet]
  meta = "anthropic/claude-sonnet-4-5"
  api_name = "claude-sonnet-4-6"
"#;
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn resolves_with_overrides() {
        let cfg = config_with_model();
        let info = resolve_model(&cfg, CACHE, "ws/sonnet").unwrap();
        assert_eq!(info.provider, "ws");
        assert_eq!(info.api_name, "claude-sonnet-4-6");
        assert_eq!(info.context_window, 200000);
        assert!(info.supports(&Capability::Thinking));
    }

    #[test]
    fn api_name_defaults_to_model_id() {
        let toml = r#"
[providers.p]
kind = "anthropic"

  [providers.p.models.s]
  meta = "anthropic/claude-sonnet-4-5"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let info = resolve_model(&cfg, CACHE, "p/s").unwrap();
        assert_eq!(info.api_name, "claude-sonnet-4-5");
    }

    #[test]
    fn bad_reference_format_errors() {
        let cfg = config_with_model();
        assert!(resolve_model(&cfg, CACHE, "sonnet").is_err());
    }

    #[test]
    fn unknown_provider_errors() {
        let cfg = config_with_model();
        assert!(resolve_model(&cfg, CACHE, "nope/sonnet").is_err());
    }

    #[test]
    fn unknown_model_errors() {
        let cfg = config_with_model();
        assert!(resolve_model(&cfg, CACHE, "ws/nope").is_err());
    }

    #[test]
    fn meta_not_in_cache_errors() {
        let toml = r#"
[providers.p]
kind = "anthropic"

  [providers.p.models.x]
  meta = "anthropic/does-not-exist"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(resolve_model(&cfg, CACHE, "p/x").is_err());
    }
}
