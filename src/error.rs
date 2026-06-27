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

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
