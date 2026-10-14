//! 从配置解析出「模型元数据 + provider」。
//!
//! 这是接线逻辑，不是协议逻辑：读 `[providers.*]`、定线协议、装抓包钩子。
//! 命令行启动与会话内 `/model` 切换共用同一份 —— 两边各写一遍必然长歪。

use std::sync::Arc;

use crate::config::Config;
use crate::error::Result;
use crate::provider::wire::WireProvider;
use crate::provider::LlmProvider;

/// 解析模型引用 → (ModelInfo, provider)。按 model.provider 查 `[providers.*]`，
/// 没配的按 provider 名合成一份（key 从环境变量取）。startup 与会话内 `/model` 共用。
pub fn resolve_provider(
    config: &Config,
    cache_json: &str,
    model_ref: &str,
) -> Result<(crate::registry::ModelInfo, Arc<dyn LlmProvider>)> {
    let model = crate::registry::resolve_model(config, cache_json, model_ref)?;
    let pcfg = config.provider_for(&model.provider);
    let protocol = pcfg.protocol();
    let client_config = pcfg.to_client_config()?;

    // 请求报文日志：只在 `[debug] log_requests` 打开时挂。
    // 它是底座的 RequestHook，拿到的是**最终字节** —— 记下来的是真正的原始报文
    // （旧底座只能给归一化视图，排障时经常对不上）。
    let hook: Option<Arc<dyn llm_adapt_core::RequestHook>> = config.debug.log_requests.then(|| {
        let dir = config
            .debug
            .llm_log_dir
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(crate::config::paths::llm_log_dir);
        Arc::new(crate::provider::llm_log::LlmLogHook::new(dir))
            as Arc<dyn llm_adapt_core::RequestHook>
    });

    let provider: Arc<dyn LlmProvider> =
        Arc::new(WireProvider::new(client_config, protocol, hook)?);
    Ok((model, provider))
}
