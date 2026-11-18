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

/// 一次拉取的结果。带上实际请求的 URL —— 出错、或者用户想确认「它到底打到了哪」时
/// 看的第一样东西就是它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedModels {
    pub url: String,
    pub models: Vec<RemoteModel>,
}

/// 只要模型名的版本，给不在乎 URL 的调用方（向导、`/model` 候选）。
pub async fn list_models(pcfg: &ProviderConfig) -> Result<Vec<RemoteModel>, String> {
    Ok(fetch_models(pcfg).await?.models)
}

/// 拉取端点的模型列表。失败返回错误文本（调用方只会把它当提示打出来）。
pub async fn fetch_models(pcfg: &ProviderConfig) -> Result<FetchedModels, String> {
    let client_cfg = pcfg.to_client_config().map_err(|e| e.to_string())?;
    let url = models_url(&client_cfg.base_url);
    if url.is_empty() {
        return Err("还没填 Base URL —— 不知道去哪里问模型列表".into());
    }
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
    let resp = req.send().await.map_err(|e| describe(&url, &e))?;
    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        // 正文只取一小截：网关 502 时常回一整页 HTML，全塞进错误里反而看不清。
        let snippet: String = body.trim().chars().take(200).collect();
        return Err(if snippet.is_empty() {
            format!("{}（HTTP {code}）：{url}", status_hint(code))
        } else {
            format!("{}（HTTP {code}）：{url} —— {snippet}", status_hint(code))
        });
    }
    let body = resp.text().await.map_err(|e| describe(&url, &e))?;
    let models = parse_models(&body).map_err(|e| format!("{url}：{e}"))?;
    Ok(FetchedModels { url, models })
}

/// 只有 kind / base_url / key 的渠道配置 —— 向导和渠道编辑器手里常常只有这三样。
///
/// `base_url` 留空时落回该 kind 的协议默认地址（与真正发聊天请求时同一个来源），
/// 否则「填了密钥、类型选 DeepSeek、地址留空」这种最省事的配置会在这里报「没填地址」。
pub fn probe_provider(kind: &str, base_url: &str, api_key: &str) -> ProviderConfig {
    let key = api_key.trim();
    ProviderConfig {
        kind: kind.to_string(),
        api_key: (!key.is_empty()).then(|| key.to_string()),
        base_url: {
            let base = base_url.trim();
            (!base.is_empty()).then(|| base.to_string())
        },
        ..Default::default()
    }
}

fn status_hint(code: u16) -> &'static str {
    match code {
        401 | 403 => "密钥不对，或这个渠道不接受拉列表",
        404 => "这个地址下没有 /models —— 检查 Base URL 是否多写/少写了版本段",
        429 => "被限流了，稍等再试",
        _ => "上游拒绝了这次请求",
    }
}

fn describe(url: &str, e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("GET {url} 超时（检查 base_url 是否可达，或是否需要代理）")
    } else if e.is_connect() {
        format!("连不上上游：{e}")
    } else {
        format!("GET {url}: {e}")
    }
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

/// 列表接口的 URL。
///
/// 规则：`base_url` 末尾已经是版本段（`/v1`、`/v4`、`/v1beta/openai` 这种）就直接接
/// `/models`；否则补 `/v1/models`。这正是各家文档里写的那个地址，少补一次或多补一次
/// 都会得到 404，而 404 在日志里和「路径写错」长得一模一样。
///
/// 对话接口不做这个判断（底座永远拼 `/v1/...`），但列表接口是我们自己拼的，
/// 而 `.../v1` 结尾的 base_url 在 OpenAI 兼容网关里太常见，不处理一半人拉不到。
fn models_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return String::new();
    }
    let last = base.rsplit('/').next().unwrap_or(base);
    let versioned = last.len() >= 2
        && last.starts_with('v')
        && last[1..].chars().next().is_some_and(|c| c.is_ascii_digit());
    // `…/v1beta/openai`、`…/compatible-mode/v1`、`…/api/paas/v4` 都算「已经到版本层」。
    if versioned || last == "openai" {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

/// 解析各家端点实际返回的形状。
///
/// - OpenAI 兼容 / Anthropic：`{"data":[{"id": "…"}]}`（Anthropic 还带 `display_name`）；
/// - Gemini 原生：`{"models":[{"name": "models/gemini-3.1-pro"}]}` —— `models/` 前缀要剥掉，
///   带着它发给上游是 404；
/// - 少数实现只给 `{"models":["a","b"]}` 或裸数组 `["a","b"]`，条目还可能重复。
///
/// 一条都认不出时**报错而不是返回空表**：空表在上层会被渲染成「端点没有模型」，
/// 而真实情况是「我们没看懂它回的东西」，两者的处理方式完全不同。
fn parse_models(body: &str) -> Result<Vec<RemoteModel>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("响应不是合法 JSON：{e}"))?;

    let mut out: Vec<RemoteModel> = Vec::new();
    let mut push = |raw: &str, name: Option<&str>| {
        let id = raw.strip_prefix("models/").unwrap_or(raw).trim();
        if id.is_empty() || out.iter().any(|m| m.id == id) {
            return;
        }
        let name = name
            .map(|n| n.strip_prefix("models/").unwrap_or(n).trim())
            .filter(|n| !n.is_empty() && *n != id)
            .unwrap_or(id);
        out.push(RemoteModel {
            id: id.to_string(),
            name: name.to_string(),
        });
    };

    let items = v
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| v.get("models").and_then(|d| d.as_array()))
        .or_else(|| v.as_array());
    let Some(items) = items else {
        return Err("认不出上游返回的模型列表格式（既没有 data 也没有 models 数组）".into());
    };
    for item in items {
        match item {
            serde_json::Value::String(s) => push(s, None),
            serde_json::Value::Object(o) => {
                let id = o
                    .get("id")
                    .and_then(|s| s.as_str())
                    .or_else(|| o.get("name").and_then(|s| s.as_str()));
                let name = o
                    .get("display_name")
                    .and_then(|s| s.as_str())
                    .or_else(|| o.get("name").and_then(|s| s.as_str()));
                if let Some(id) = id {
                    push(id, name);
                }
            }
            _ => {}
        }
    }

    if out.is_empty() {
        return Err("上游返回了模型列表，但里面没有任何模型名".into());
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_gets_the_version_segment_exactly_once() {
        for (base, want) in [
            (
                "https://api.anthropic.com",
                "https://api.anthropic.com/v1/models",
            ),
            (
                "https://api.deepseek.com",
                "https://api.deepseek.com/v1/models",
            ),
            (
                "https://api.groq.com/openai/v1/",
                "https://api.groq.com/openai/v1/models",
            ),
            (
                "https://openrouter.ai/api/v1",
                "https://openrouter.ai/api/v1/models",
            ),
            (
                "https://open.bigmodel.cn/api/paas/v4",
                "https://open.bigmodel.cn/api/paas/v4/models",
            ),
            (
                "https://generativelanguage.googleapis.com/v1beta/openai",
                "https://generativelanguage.googleapis.com/v1beta/openai/models",
            ),
            // 中转站：裸域名，要补 /v1。
            (
                "http://192.168.31.1:3000",
                "http://192.168.31.1:3000/v1/models",
            ),
            ("  ", ""),
        ] {
            assert_eq!(models_url(base), want, "base={base}");
        }
    }

    #[test]
    fn parses_the_shapes_upstreams_actually_return() {
        let openai = r#"{"object":"list","data":[{"id":"gpt-5.6"},{"id":"gpt-4.1"}]}"#;
        let ms = parse_models(openai).unwrap();
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].id, "gpt-4.1", "按 id 排序");
        assert_eq!(ms[0].name, "gpt-4.1", "没有 display_name 时同 id");

        // Anthropic 的分页字段不影响解析，展示名要留着。
        let anthropic = r#"{"data":[{"id":"claude-opus-5","type":"model","display_name":"Claude Opus 5"}],"has_more":false}"#;
        let ms = parse_models(anthropic).unwrap();
        assert_eq!(ms[0].id, "claude-opus-5");
        assert_eq!(ms[0].name, "Claude Opus 5");

        // Gemini 原生会带 `models/` 前缀；带着它发回上游是 404。
        let gemini = r#"{"models":[{"name":"models/gemini-3.1-pro"}]}"#;
        assert_eq!(parse_models(gemini).unwrap()[0].id, "gemini-3.1-pro");

        // 少数实现给字符串数组，还可能在两个字段里重复。
        let loose = r#"["kimi-k3", "kimi-k3", "glm-5.3"]"#;
        assert_eq!(
            parse_models(loose)
                .unwrap()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["glm-5.3", "kimi-k3"]
        );
        assert_eq!(parse_models(r#"{"models":["a","b"]}"#).unwrap().len(), 2);
    }

    #[test]
    fn an_unrecognisable_body_says_so_instead_of_returning_nothing() {
        assert!(parse_models(r#"{"error":"x"}"#).is_err());
        assert!(parse_models("<html>502</html>").is_err());
        assert!(parse_models(r#"{"data":[]}"#).is_err());
    }

    #[test]
    fn probe_provider_leaves_a_blank_base_url_to_the_protocol_default() {
        let p = probe_provider("deepseek", "   ", "  sk-x  ");
        assert_eq!(p.kind, "deepseek");
        assert_eq!(p.api_key.as_deref(), Some("sk-x"), "首尾空白要剪掉");
        assert!(p.base_url.is_none(), "留空要落回协议默认地址");

        // 网址给全的时候原样带过去（只剪空白，不自己补 /v1）。
        let p = probe_provider("openai", " https://api.openai.com ", "");
        assert_eq!(p.base_url.as_deref(), Some("https://api.openai.com"));
        assert!(p.api_key.is_none(), "空密钥等于不带鉴权");
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
