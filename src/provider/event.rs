//! 统一事件枚举 —— 屏蔽 genai 与底座差异，agent loop 只认这套类型。

/// 流式事件。底座（genai/逃生舱）产出的原生事件都归一到此枚举。
#[derive(Debug, Clone)]
pub enum Event {
    /// assistant 文本增量。
    TextDelta(String),
    /// 思考/推理增量。
    ThinkingDelta(String),
    /// 工具调用（流末从 captured 取完整调用后 emit）。
    ToolCall(ToolCall),
    /// 用量快照。单次快照语义，累加策略由调用方决定
    /// （注意 Gemini 流式 usage 为累计值，应取最后一条而非逐块加）。
    Usage(Usage),
    /// 流结束 + 终止原因。
    Done(StopReason),
}

/// 归一化的终止原因。
#[allow(dead_code)] // Other 载荷保留 provider 原始原因，M2 日志/诊断使用
#[derive(Debug, Clone)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Stop,
    Other(String),
}

/// 归一化的用量。token 数；成本由 cost 模块结合 Pricing 计算。
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 推理 token（已含在 output 内，仅用于展示）。
    pub reasoning: u64,
}

impl Usage {
    /// 多轮累加各字段（用于跨 turn 汇总）。
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.reasoning += other.reasoning;
    }
}

/// 工具调用。
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}
