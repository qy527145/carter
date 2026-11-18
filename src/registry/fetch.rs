//! models.dev 元数据爬取。`carter update` 调用：GET api.json → 缓存到 `~/.carter/models.json`。
//! 纪律：reqwest 仅在本文件出现；registry 其余处不碰 HTTP。

use std::path::Path;

use crate::config::paths::models_cache_path;
use crate::error::CarterError;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// 拉取 models.dev 聚合 JSON（原始文本）。
///
/// 带 User-Agent：models.dev 前面的 CDN 会对没有 UA 的请求回 403。
pub async fn fetch_models_dev() -> crate::Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("carter/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| CarterError::Provider(format!("build http client: {e}")))?;
    let resp = client
        .get(MODELS_DEV_URL)
        .send()
        .await
        .map_err(|e| CarterError::Provider(format!("fetch models.dev: {e}")))?;
    if !resp.status().is_success() {
        return Err(CarterError::Provider(format!(
            "models.dev returned {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| CarterError::Provider(format!("read models.dev body: {e}")))
}

/// 写缓存到 `~/.carter/models.json`（建父目录）。
pub fn write_cache(json: &str) -> crate::Result<()> {
    let path = models_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, json)?;
    Ok(())
}

/// 读模型元数据：优先 `~/.carter/models.json` 缓存，缺失或读不出就用内置快照。
///
/// 永远能返回一份可用的 JSON —— 首次运行、断网、models.dev 403 都不该让用户
/// 连第一句话都发不出去。快照可能比线上旧，新模型查不到时提示 `carter update`。
pub fn read_cache() -> crate::Result<String> {
    Ok(read_cache_at(&models_cache_path()))
}

fn read_cache_at(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => super::bundled::bundled_json().to_string(),
    }
}

/// 本地缓存是否存在（向导用来决定要不要提议联网拉取）。
pub fn cache_exists() -> bool {
    models_cache_path().exists()
}
