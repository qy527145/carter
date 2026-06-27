//! models.dev 元数据爬取。`carter update` 调用：GET api.json → 缓存到 `~/.carter/models.json`。
//! 纪律：reqwest 仅在本文件出现；registry 其余处不碰 HTTP。

use std::path::Path;

use crate::config::paths::models_cache_path;
use crate::error::CarterError;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// 拉取 models.dev 聚合 JSON（原始文本）。
pub async fn fetch_models_dev() -> crate::Result<String> {
    let resp = reqwest::get(MODELS_DEV_URL)
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

/// 读缓存。缺失返回友好错误（提示先 `carter update`）。
pub fn read_cache() -> crate::Result<String> {
    let path = models_cache_path();
    read_cache_at(&path)
}

fn read_cache_at(path: &Path) -> crate::Result<String> {
    std::fs::read_to_string(path).map_err(|_| {
        CarterError::Config(format!(
            "模型元数据缓存缺失（{}）；请先运行 `carter update` 拉取 models.dev。",
            path.display()
        ))
    })
}
