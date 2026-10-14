use thiserror::Error;

pub type Result<T> = std::result::Result<T, CarterError>;

#[derive(Debug, Error)]
pub enum CarterError {
    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("provider error: {0}")]
    Provider(String),

    /// 上游 LLM 调用失败。
    ///
    /// 不压成字符串是因为嵌入方需要按类别分流（限流要退避、鉴权要提示换 key、
    /// 上下文溢出要触发压缩），而这些在 [`llm_adapt_core::LLMError`] 里是分类好的。
    #[error(transparent)]
    Llm(#[from] llm_adapt_core::LLMError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
