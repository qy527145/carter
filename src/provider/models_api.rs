//! 向端点问「你有哪些模型」—— 向导和 `/model` 的列表来源。
//!
//! 三种线协议的模型列表接口长得差不多：
//! - Anthropic：`GET /v1/models`，头 `x-api-key` + `anthropic-version`，回 `{data:[{id}]}`；
//! - OpenAI 系（Chat / Responses 共用）：`GET /v1/models`，头 `Authorization: Bearer`，
//!   回 `{data:[{id}]}`；
//! - Gemini 的 OpenAI 兼容端点同上；Ollama 的 `/v1/models` 也镜像了这个形状。
//!
//! 所以这里只有一个函数：按 kind 决定鉴权头和路径，发 GET，取 `data[].id`。
//! 拉不到不是错误 —— 很多网关不开这个接口 —— 调用方回落到手填。
//!
//! 纪律：这是 `reqwest` 在 provider 目录里唯一出现的地方之外的第二处（第一处是
//! registry/fetch），别把它扩成通用 HTTP 层。

use std::time::Duration;

use crate::config::ProviderConfig;

/// 端点返回的一个模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteModel {
    pub id: String,
    /// 有些端点带 `display_name`（Anthropic）或 `name`；缺省同 id。
    pub name: String,
}

/// 拉取端点的模型列表。失败返回错误文本（调用方只会把它当提示打出来）。
pub async fn list_models(pcfg: &ProviderConfig) -> Result<Vec<RemoteModel>, String> {
    let client_cfg = pcfg.to_client_config().map_err(|e| e.to_string())?;
    let url = models_url(&client_cfg.base_url, pcfg.protocol().as_str());
    let api_key = client_cfg.api_key.clone();

    let client = reqwest::Client::builder()
        .user_agent(concat!("carter/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(&url);
    if pcfg.protocol().as_str() == llm_adapt_core::ProtocolId::ANTHROPIC {
        req = req
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01");
    } else if !api_key.is_empty() {
        req = req.bearer_auth(&api_key);
    }
    let resp = req.send().await.map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("GET {url} → {status}: {snippet}"));
    }
    parse_models(&body).ok_or_else(|| format!("GET {url}: 响应里没有 data[].id"))
}

/// 对配置里每个 provider 并发拉一次列表。拉不到的 provider 直接跳过（返回里没有它）——
/// 这是给 `/model` 弹窗补候选用的，缺一两个 provider 不影响已配置的模型。
pub async fn list_for_config(config: &crate::config::Config) -> Vec<(String, Vec<RemoteModel>)> {
    let futs = config.providers.iter().map(|(name, pcfg)| {
        let name = name.clone();
        let pcfg = pcfg.clone();
        async move {
            match list_models(&pcfg).await {
                Ok(list) if !list.is_empty() => Some((name, list)),
                Ok(_) => None,
                Err(e) => {
                    tracing::debug!("拉取 provider `{name}` 模型列表失败：{e}");
                    None
                }
            }
        }
    });
    let mut out: Vec<(String, Vec<RemoteModel>)> = futures::future::join_all(futs)
        .await
        .into_iter()
        .flatten()
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 列表接口的 URL：`base_url` 末尾已经是 `/v1` 就直接接 `/models`，否则接 `/v1/models`。
///
/// 对话接口不做这个判断（底座永远拼 `/v1/...`），但列表接口是我们自己拼的，
/// 而 `.../v1` 结尾的 base_url 在 OpenAI 兼容网关里太常见，不处理一半人拉不到。
fn models_url(base_url: &str, _protocol: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") || base.ends_with("/v1beta/openai") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

/// `{data:[{id, display_name?|name?}]}` → 列表，按 id 排序。
fn parse_models(body: &str) -> Option<Vec<RemoteModel>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let arr = v
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| v.get("models").and_then(|d| d.as_array()))?;
    let mut out: Vec<RemoteModel> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|s| s.as_str())?;
            let name = m
                .get("display_name")
                .or_else(|| m.get("name"))
                .and_then(|s| s.as_str())
                .unwrap_or(id);
            Some(RemoteModel {
                id: id.to_string(),
                name: name.to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_url_handles_v1_suffix() {
        assert_eq!(
            models_url("https://api.anthropic.com", "anthropic"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_url("https://api.groq.com/openai/v1/", "openai"),
            "https://api.groq.com/openai/v1/models"
        );
        assert_eq!(
            models_url(
                "https://generativelanguage.googleapis.com/v1beta/openai",
                "x"
            ),
            "https://generativelanguage.googleapis.com/v1beta/openai/models"
        );
    }

    #[test]
    fn parse_openai_and_anthropic_shapes() {
        let oa = r#"{"object":"list","data":[{"id":"gpt-5","object":"model"},{"id":"gpt-4o"}]}"#;
        let ms = parse_models(oa).unwrap();
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].id, "gpt-4o");
        assert_eq!(ms[0].name, "gpt-4o");

        let an = r#"{"data":[{"id":"claude-sonnet-4-5","display_name":"Claude Sonnet 4.5"}]}"#;
        let ms = parse_models(an).unwrap();
        assert_eq!(ms[0].name, "Claude Sonnet 4.5");

        assert!(parse_models(r#"{"error":"x"}"#).is_none());
        assert!(parse_models("not json").is_none());
    }
}

#[cfg(test)]
mod live_tests {
    /// 手动验证：CARTER_LIVE_KIND / BASE_URL / API_KEY 三个环境变量齐了才跑。
    #[tokio::test]
    #[ignore]
    async fn list_models_live() {
        let (Ok(kind), Ok(key)) = (
            std::env::var("CARTER_LIVE_KIND"),
            std::env::var("CARTER_LIVE_API_KEY"),
        ) else {
            return;
        };
        let pcfg = crate::config::ProviderConfig {
            kind,
            base_url: std::env::var("CARTER_LIVE_BASE_URL").ok(),
            api_key: Some(key),
            ..Default::default()
        };
        let list = super::list_models(&pcfg).await.expect("list ok");
        eprintln!(
            "{} models: {:?}",
            list.len(),
            list.iter().take(5).map(|m| &m.id).collect::<Vec<_>>()
        );
        assert!(!list.is_empty());
    }
}
