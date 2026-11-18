//! ModelRegistry —— 模型元数据解析。
//! 元数据来源 = models.dev 缓存（`~/.carter/models.json`）；用户面绑定在 config 的 `[models.X]`。

pub mod bundled;
pub mod catalog;
pub mod fetch;
mod model;
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
            "模型元数据里找不到 `{}`；新模型请先 `carter update` 刷新，或检查 meta 拼写。",
            entry.meta
        ))
    })?;

    // 用户面绑定覆盖：key = 用户引用（provider/model）；provider 指向 config 的 [providers.X]；
    // api_name 下发给端点 —— 省略时默认取**配置块名**（model_name），便于直接用真实模型名做块名。
    info.key = reference.to_string();
    info.provider = provider_name.to_string();
    info.api_name = entry
        .api_name
        .clone()
        .unwrap_or_else(|| model_name.to_string());
    if entry.default_reasoning.is_some() {
        info.default_reasoning = entry.default_reasoning.clone();
    }
    Ok(info)
}

/// 宽松解析：引用里的模型没在配置里写块也行 —— 端点列表里选出来的模型就是这种。
///
/// 配置里有块走 [`resolve_model`]；没有就现合成一个：`api_name` 取引用里的模型名，
/// 元数据按协议种类对应的 models.dev provider 猜（猜不到用保守的通用值：128k 上下文、
/// 价格 0、工具 + 流式）。启动时的默认模型仍用严格版本，配置里的拼写错误要在
/// 启动时报出来，而不是变成一个 404。
pub fn resolve_model_lenient(
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
    if provider.models.contains_key(model_name) {
        return resolve_model(config, cache_json, reference);
    }

    let preferred = meta_provider_for_kind(&provider.kind);
    let mut info = catalog::guess_meta(cache_json, preferred, model_name)
        .and_then(|(pid, mid)| models_dev::lookup(cache_json, &pid, &mid))
        .unwrap_or_else(|| generic_model_info(provider_name, model_name, &provider.kind));
    info.key = reference.to_string();
    info.provider = provider_name.to_string();
    info.api_name = model_name.to_string();
    Ok(info)
}

/// 协议种类 → models.dev 里对应的 provider id（用来缩小猜元数据的范围）。
pub fn meta_provider_for_kind(kind: &str) -> Option<&'static str> {
    Some(match kind.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "anthropic_v2" | "anthropic_messages" | "messages" => "anthropic",
        "openai" | "openai_compat" | "openai_responses" | "responses" | "custom" => "openai",
        "gemini" | "google" => "google",
        "deepseek" => "deepseek",
        "groq" => "groq",
        "xai" | "grok" => "xai",
        "openrouter" => "openrouter",
        "ollama" => "ollama",
        _ => return None,
    })
}

/// 没有任何元数据时的保守默认。
fn generic_model_info(provider_name: &str, model_name: &str, kind: &str) -> ModelInfo {
    ModelInfo {
        key: format!("{provider_name}/{model_name}"),
        provider: provider_name.to_string(),
        api_name: model_name.to_string(),
        context_window: 128_000,
        max_output_tokens: 16_000,
        tokenizer: meta_provider_for_kind(kind).unwrap_or("openai").to_string(),
        capabilities: vec![Capability::Tools, Capability::Streaming],
        pricing: Pricing {
            input: 0.0,
            output: 0.0,
            cache_read: None,
            cache_write: None,
        },
        default_reasoning: None,
    }
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
        assert_eq!(info.key, "ws/sonnet");
        assert_eq!(info.provider, "ws");
        assert_eq!(info.api_name, "claude-sonnet-4-6");
        assert_eq!(info.context_window, 200000);
        assert!(info.supports(&Capability::Thinking));
    }

    #[test]
    fn lenient_synthesizes_unconfigured_model_with_guessed_meta() {
        let cfg = config_with_model();
        // 配置里没有 `claude-sonnet-4-5-20250929` 这个块；按 anthropic 猜到元数据。
        let info = resolve_model_lenient(&cfg, CACHE, "ws/claude-sonnet-4-5-20250929").unwrap();
        assert_eq!(info.key, "ws/claude-sonnet-4-5-20250929");
        assert_eq!(info.api_name, "claude-sonnet-4-5-20250929");
        assert_eq!(info.context_window, 200000);
        assert!(info.supports(&Capability::Thinking));
        // 完全猜不到 → 通用默认，但仍可用。
        let info = resolve_model_lenient(&cfg, CACHE, "ws/totally-new").unwrap();
        assert_eq!(info.context_window, 128_000);
        assert_eq!(info.pricing.input, 0.0);
        assert!(info.supports(&Capability::Tools));
        // 配置里有块的仍走严格路径（api_name 覆盖生效）。
        let info = resolve_model_lenient(&cfg, CACHE, "ws/sonnet").unwrap();
        assert_eq!(info.api_name, "claude-sonnet-4-6");
        // provider 不存在仍报错。
        assert!(resolve_model_lenient(&cfg, CACHE, "nope/x").is_err());
    }

    #[test]
    fn api_name_defaults_to_block_name() {
        // 直接用真实模型名做块名、省略 api_name → api_name 取块名。
        let toml = r#"
[providers.p]
kind = "anthropic"

  [providers.p.models.claude-sonnet-4-6]
  meta = "anthropic/claude-sonnet-4-5"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let info = resolve_model(&cfg, CACHE, "p/claude-sonnet-4-6").unwrap();
        assert_eq!(info.api_name, "claude-sonnet-4-6");
        assert_eq!(info.key, "p/claude-sonnet-4-6");
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
