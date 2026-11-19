//! Agent 层 —— 编排器、Turn 状态机、会话历史、上下文工程。

pub mod ask_user;
pub mod bootstrap;
pub mod checkpoint;
pub mod context;
pub mod steer;
pub mod subagent;
pub mod thread;
pub mod turn;
pub mod ui;

pub use ask_user::{AskOption, AskQuestion, AskUserQuestionTool};
pub use bootstrap::build_tools;
pub use context::generate_title;
pub use steer::{STEER_HEAD, SteerInbox, is_steer, wrap};
pub use subagent::{TaskTool, ToolFactory};
pub use thread::Thread;
#[allow(unused_imports)]
pub use turn::{
    CompactModel, RunOptions, Steer, TurnOutcome, TurnState, current_call_id, current_session_id,
    run_turn,
};
#[allow(unused_imports)]
pub use ui::{
    AgentRun, AskResponder, CancelToken, Capture, NullSink, ReplayMsg, ReplayRole, StdoutSink,
    UiEvent, UiSink, replay_from_messages,
};
