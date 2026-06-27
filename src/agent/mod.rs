//! Agent 层 —— 编排器、Turn 状态机、会话历史、上下文工程。

pub mod context;
pub mod subagent;
pub mod thread;
pub mod turn;
pub mod ui;

pub use context::generate_title;
pub use subagent::TaskTool;
pub use thread::Thread;
#[allow(unused_imports)]
pub use turn::{run_turn, CompactModel, RunOptions, TurnOutcome, TurnState};
#[allow(unused_imports)]
pub use ui::{CancelToken, StdoutSink, UiEvent, UiSink};
