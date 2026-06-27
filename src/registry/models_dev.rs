//! models.dev `api.json` → `ModelInfo` 映射。
//! 懒解析：`serde_json::Value` 导航到 `[provider][models][model]`，局部 `from_value` 成精简结构。
//! 纪律：纯解析，无 HTTP / 无 genai。

use serde::Deserialize;
use serde_json::Value;

use super::model::{Capability, ModelInfo, Pricing, ReasoningEffort};

/// models.dev 单模型精简视图（只取我们用到的字段）。
#[derive(Debug, Deserialize)]
struct DevModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    reasoning_options: Vec<Value>,
    #[serde(default)]
    modalities: DevModalities,
    #[serde(default)]
    limit: DevLimit,
    #[serde(default)]
    cost: Option<DevCost>,
}

#[derive(Debug, Default, Deserialize)]
struct DevModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DevLimit {
    #[serde(default)]
    context: u32,
    #[serde(default)]
    output: u32,
}

#[derive(Debug, Deserialize)]
struct DevCost {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

/// 在缓存 JSON 中查 `provider_id/model_id`，映射成 `ModelInfo`。
/// `provider`/`api_name` 暂以 model id 占位，由调用方（main.rs）用 config 的 `[models.X]` 覆盖。
pub fn lookup(cache_json: &str, provider_id: &str, model_id: &str) -> Option<ModelInfo> {
    let root: Value = serde_json::from_str(cache_json).ok()?;
    let node = root.get(provider_id)?.get("models")?.get(model_id)?.clone();
    let dev: DevModel = serde_json::from_value(node).ok()?;
    Some(to_model_info(provider_id, dev))
}

fn to_model_info(provider_id: &str, dev: DevModel) -> ModelInfo {
    let pricing = match dev.cost {
        Some(c) => Pricing {
            input: c.input,
            output: c.output,
            cache_read: c.cache_read,
            cache_write: c.cache_write,
        },
        // 免费/本地模型无 cost → 全 0。
        None => Pricing {
            input: 0.0,
            output: 0.0,
            cache_read: None,
            cache_write: None,
        },
    };

    let mut capabilities = Vec::new();
    if dev.tool_call {
        capabilities.push(Capability::Tools);
    }
    let has_image = dev.modalities.input.iter().any(|m| m == "image");
    if dev.attachment && has_image {
        capabilities.push(Capability::Vision);
    }
    if dev.reasoning {
        capabilities.push(Capability::Thinking);
    }
    if dev.modalities.input.iter().any(|m| m == "pdf") {
        capabilities.push(Capability::Pdf);
    }
    // models.dev 无显式 streaming 布尔：现代云模型默认支持。
    capabilities.push(Capability::Streaming);
    // prompt cache：仅当声明了 cache_read 价。
    if pricing.cache_read.is_some() {
        capabilities.push(Capability::PromptCache);
    }

    let default_reasoning = if dev.reasoning_options.is_empty() {
        None
    } else {
        Some(ReasoningEffort::Medium)
    };

    ModelInfo {
        key: format!("{provider_id}/{}", dev.id),
        // 占位：调用方用 config 的 [models.X] 覆盖。
        provider: provider_id.to_string(),
        api_name: dev.id.clone(),
        context_window: dev.limit.context,
        max_output_tokens: dev.limit.output,
        tokenizer: provider_id.to_string(),
        capabilities,
        pricing,
        default_reasoning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "anthropic": {
        "id": "anthropic",
        "models": {
          "claude-sonnet-4-5": {
            "id": "claude-sonnet-4-5",
            "attachment": true,
            "reasoning": true,
            "reasoning_options": [{"type":"budget_tokens","min":1024}],
            "tool_call": true,
            "modalities": { "input": ["text","image","pdf"], "output": ["text"] },
            "limit": { "context": 200000, "output": 64000 },
            "cost": { "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75 }
          }
        }
      },
      "local": {
        "models": {
          "free-model": {
            "id": "free-model",
            "tool_call": true,
            "modalities": { "input": ["text"], "output": ["text"] },
            "limit": { "context": 8192, "output": 2048 }
          }
        }
      }
    }"#;

    #[test]
    fn maps_full_model() {
        let m = lookup(SAMPLE, "anthropic", "claude-sonnet-4-5").unwrap();
        assert_eq!(m.context_window, 200000);
        assert_eq!(m.max_output_tokens, 64000);
        assert_eq!(m.pricing.input, 3.0);
        assert_eq!(m.pricing.output, 15.0);
        assert_eq!(m.pricing.cache_read, Some(0.3));
        assert!(m.supports(&Capability::Tools));
        assert!(m.supports(&Capability::Vision));
        assert!(m.supports(&Capability::Thinking));
        assert!(m.supports(&Capability::Pdf));
        assert!(m.supports(&Capability::Streaming));
        assert!(m.supports(&Capability::PromptCache));
        assert_eq!(m.default_reasoning, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn missing_cost_degrades_to_zero() {
        let m = lookup(SAMPLE, "local", "free-model").unwrap();
        assert_eq!(m.pricing.input, 0.0);
        assert_eq!(m.pricing.output, 0.0);
        assert!(m.pricing.cache_read.is_none());
        // 无 cache_read → 不加 PromptCache。
        assert!(!m.supports(&Capability::PromptCache));
        // 无 image modality → 无 Vision。
        assert!(!m.supports(&Capability::Vision));
        // 无 reasoning → 无 Thinking + default_reasoning None。
        assert!(!m.supports(&Capability::Thinking));
        assert_eq!(m.default_reasoning, None);
    }

    #[test]
    fn unknown_returns_none() {
        assert!(lookup(SAMPLE, "anthropic", "nope").is_none());
        assert!(lookup(SAMPLE, "nope", "x").is_none());
    }
}
