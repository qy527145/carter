//! 模型静态元数据类型。接入库不提供，必须自管。

use serde::Deserialize;

/// capability flags。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Tools,
    Vision,
    Thinking,
    Pdf,
    Streaming,
    PromptCache,
}

/// 推理强度（自研镜像，映射到底座枚举）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Budget(u32),
}

/// 单价（每百万 token 美元）。
#[derive(Debug, Clone, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: Option<f64>,
    pub cache_read: Option<f64>,
}

/// 一个模型的完整元数据。`key` 在反序列化后由 registry 回填。
#[allow(dead_code)] // 多数字段在 M2+（能力校验/上下文占用/分词）读取
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    #[serde(skip)]
    pub key: String,
    pub provider: String,
    pub api_name: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub tokenizer: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    pub pricing: Pricing,
    pub default_reasoning: Option<ReasoningEffort>,
}

impl ModelInfo {
    #[allow(dead_code)] // M2 运行时能力校验（带图但无 vision → 早失败）
    pub fn supports(&self, cap: &Capability) -> bool {
        self.capabilities.contains(cap)
    }
}
