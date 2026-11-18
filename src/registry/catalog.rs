//! 给「选择器」用的模型元数据目录视图。
//!
//! `models_dev::lookup` 是点查（知道 id 才能用）；向导和 `/model` 要的是**列表**：
//! 先挑 provider，再在它的模型里挑。这里把 models.dev 那棵 JSON 整理成排好序的
//! 列表，并给几个和「用户现在在配什么」相关的推荐。
//!
//! 纪律：纯解析，无 HTTP / 无终端。

use serde_json::Value;

/// 目录里的一个 provider。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    pub id: String,
    /// 展示名（models.dev 的 `name`，缺省同 id）。
    pub name: String,
    pub model_count: usize,
}

/// 目录里的一个模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    pub context: u32,
    pub reasoning: bool,
    pub tool_call: bool,
    /// `YYYY-MM-DD`，缺省空串；排序用（新的在前）。
    pub release_date: String,
}

impl ModelEntry {
    /// 一行摘要：上下文窗口 + 能力标记。列表里跟在 id 后面。
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.context > 0 {
            parts.push(format!("{}k", self.context / 1000));
        }
        if self.reasoning {
            parts.push("思考".into());
        }
        if self.tool_call {
            parts.push("工具".into());
        }
        if !self.release_date.is_empty() {
            parts.push(self.release_date.clone());
        }
        parts.join(" · ")
    }
}

/// 全部 provider，按模型数量降序（大厂在前），同数量按 id。
pub fn providers(cache_json: &str) -> Vec<ProviderEntry> {
    let Ok(root) = serde_json::from_str::<Value>(cache_json) else {
        return Vec::new();
    };
    let Some(obj) = root.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<ProviderEntry> = obj
        .iter()
        .map(|(id, p)| ProviderEntry {
            id: id.clone(),
            name: p
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string(),
            model_count: p
                .get("models")
                .and_then(Value::as_object)
                .map(|m| m.len())
                .unwrap_or(0),
        })
        .filter(|p| p.model_count > 0)
        .collect();
    out.sort_by(|a, b| {
        b.model_count
            .cmp(&a.model_count)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// 某 provider 下全部模型，按发布日期降序（新的在前），同日期按 id。
pub fn models(cache_json: &str, provider_id: &str) -> Vec<ModelEntry> {
    let Ok(root) = serde_json::from_str::<Value>(cache_json) else {
        return Vec::new();
    };
    let Some(models) = root
        .get(provider_id)
        .and_then(|p| p.get("models"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut out: Vec<ModelEntry> = models
        .iter()
        .map(|(id, m)| ModelEntry {
            id: id.clone(),
            name: m
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string(),
            context: m
                .get("limit")
                .and_then(|l| l.get("context"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            reasoning: m.get("reasoning").and_then(Value::as_bool).unwrap_or(false),
            tool_call: m.get("tool_call").and_then(Value::as_bool).unwrap_or(false),
            release_date: m
                .get("release_date")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .collect();
    out.sort_by(|a, b| {
        b.release_date
            .cmp(&a.release_date)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// 按端点返回的模型名在目录里找最像的 `provider/model`。
///
/// 用户从 `/models` 列表里选了 `claude-sonnet-4-5-20250929`，models.dev 里对应的
/// 多半是 `anthropic/claude-sonnet-4-5`：先在**首选 provider**（按协议种类猜的那个）
/// 里找精确、再找前缀；都没有再去全目录找精确匹配。返回 `(provider_id, model_id)`。
pub fn guess_meta(
    cache_json: &str,
    preferred_provider: Option<&str>,
    api_model: &str,
) -> Option<(String, String)> {
    let Ok(root) = serde_json::from_str::<Value>(cache_json) else {
        return None;
    };
    let obj = root.as_object()?;
    let needle = api_model.to_ascii_lowercase();

    // 端点模型名常带路径（openrouter 的 `anthropic/claude-sonnet-4-5`）或日期后缀。
    let bare = needle.rsplit('/').next().unwrap_or(&needle).to_string();

    let find_in = |pid: &str| -> Option<String> {
        let models = obj.get(pid)?.get("models")?.as_object()?;
        if let Some((id, _)) = models
            .iter()
            .find(|(id, _)| id.to_ascii_lowercase() == bare)
        {
            return Some(id.clone());
        }
        // 前缀：`claude-sonnet-4-5-20250929` → `claude-sonnet-4-5`；取最长的那个。
        models
            .keys()
            .filter(|id| {
                let l = id.to_ascii_lowercase();
                bare.starts_with(&l) && bare.len() > l.len()
            })
            .max_by_key(|id| id.len())
            .cloned()
    };

    if let Some(pid) = preferred_provider
        && let Some(mid) = find_in(pid)
    {
        return Some((pid.to_string(), mid));
    }
    // 全目录精确匹配（按 provider 规模优先，避免落到某个小聚合站）。
    for p in providers(cache_json) {
        if Some(p.id.as_str()) == preferred_provider {
            continue;
        }
        let models = obj.get(&p.id)?.get("models")?.as_object()?;
        if let Some((id, _)) = models
            .iter()
            .find(|(id, _)| id.to_ascii_lowercase() == bare)
        {
            return Some((p.id.clone(), id.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "anthropic": { "id": "anthropic", "name": "Anthropic", "models": {
        "claude-sonnet-4-5": { "id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5",
          "reasoning": true, "tool_call": true, "release_date": "2025-09-29",
          "limit": { "context": 200000, "output": 64000 } },
        "claude-haiku-4-5": { "id": "claude-haiku-4-5", "name": "Claude Haiku 4.5",
          "reasoning": true, "tool_call": true, "release_date": "2025-10-15",
          "limit": { "context": 200000, "output": 64000 } }
      }},
      "openrouter": { "name": "OpenRouter", "models": {
        "anthropic/claude-sonnet-4-5": { "id": "anthropic/claude-sonnet-4-5", "release_date": "2025-09-29" },
        "openai/gpt-5": { "id": "openai/gpt-5" },
        "x/y": { "id": "x/y" }
      }},
      "empty": { "name": "Empty", "models": {} }
    }"#;

    #[test]
    fn providers_sorted_by_size_and_skip_empty() {
        let ps = providers(SAMPLE);
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].id, "openrouter");
        assert_eq!(ps[0].model_count, 3);
        assert_eq!(ps[1].name, "Anthropic");
    }

    #[test]
    fn models_newest_first_with_summary() {
        let ms = models(SAMPLE, "anthropic");
        assert_eq!(ms[0].id, "claude-haiku-4-5");
        assert_eq!(ms[1].id, "claude-sonnet-4-5");
        assert_eq!(ms[0].summary(), "200k · 思考 · 工具 · 2025-10-15");
        assert!(models(SAMPLE, "nope").is_empty());
    }

    #[test]
    fn guess_meta_prefers_exact_then_prefix_in_preferred_provider() {
        assert_eq!(
            guess_meta(SAMPLE, Some("anthropic"), "claude-sonnet-4-5"),
            Some(("anthropic".into(), "claude-sonnet-4-5".into()))
        );
        assert_eq!(
            guess_meta(SAMPLE, Some("anthropic"), "claude-sonnet-4-5-20250929"),
            Some(("anthropic".into(), "claude-sonnet-4-5".into()))
        );
        // 端点名带路径：取最后一段。
        assert_eq!(
            guess_meta(SAMPLE, Some("anthropic"), "anthropic/claude-haiku-4-5"),
            Some(("anthropic".into(), "claude-haiku-4-5".into()))
        );
    }

    #[test]
    fn guess_meta_falls_back_to_whole_catalog() {
        assert_eq!(
            guess_meta(SAMPLE, Some("anthropic"), "openai/gpt-5"),
            None,
            "bare 名 gpt-5 在任何 provider 都不是 id，不该乱猜"
        );
        // 端点名 `x/y` 取最后一段 `y`，目录里没有 id 叫 `y`。
        assert_eq!(guess_meta(SAMPLE, None, "x/y"), None);
        assert_eq!(guess_meta(SAMPLE, Some("nope"), "zzz"), None);
    }
}
