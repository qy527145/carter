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

impl CarterError {
    /// 这个错误是不是暂时性的 —— 原样再发一次有可能就好了。
    ///
    /// `is_retryable()` 之外还要看**错误类别**：它读的是 `ErrorContext.retryable`，
    /// 而流中途出的错（读 SSE 时断线、读空闲超时）上那一位未必填对。限流、网络、
    /// 超时这三类按类别直接认。鉴权错、请求非法、上下文溢出重发多少次都一样。
    pub fn is_transient(&self) -> bool {
        use llm_adapt_core::LLMError;
        match self {
            CarterError::Llm(e) => {
                e.is_retryable()
                    || matches!(
                        e,
                        LLMError::RateLimit { .. }
                            | LLMError::Network { .. }
                            | LLMError::Timeout { .. }
                    )
            }
            _ => false,
        }
    }

    /// 上游点名要等多久再来（429 的 `Retry-After`）。
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            CarterError::Llm(llm_adapt_core::LLMError::RateLimit { retry_after_ms, .. }) => {
                *retry_after_ms
            }
            _ => None,
        }
    }
}
