//! Agent 层 —— 编排器、Turn 状态机、会话历史、上下文工程。

pub mod ask_user;
pub mod bootstrap;
pub mod checkpoint;
pub mod context;
pub mod subagent;
pub mod thread;
pub mod turn;
pub mod ui;

pub use ask_user::AskUserQuestionTool;
pub use bootstrap::build_tools;
pub use context::generate_title;
pub use subagent::{TaskTool, ToolFactory};
pub use thread::Thread;
#[allow(unused_imports)]
pub use turn::{CompactModel, RunOptions, TurnOutcome, TurnState, run_turn};
#[allow(unused_imports)]
pub use ui::{
    CancelToken, ReplayMsg, ReplayRole, StdoutSink, UiEvent, UiSink, replay_from_messages,
};
