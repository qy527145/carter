//! Thread —— append-only 消息历史 + 会话级 todo 状态。
//! 可选挂一个 session `Recorder`：append/set_todos 时顺手落盘（见 docs/04）。

use std::sync::Arc;

use crate::provider::Message;
use crate::session::{cap_for_persist, Recorder, RecordKind};
use crate::tools::TodoItem;

use super::checkpoint::CheckpointStore;

#[derive(Debug, Default)]
pub struct Thread {
    pub messages: Vec<Message>,
    pub turns: u32,
    /// 最新 todo 快照（todo_write 工具整表覆盖）。压缩中保留、每轮复诵到 context 末尾。
    pub todos: Vec<TodoItem>,
    /// 会话级写前文件检查点栈（`/rewind` 用）。
    pub checkpoints: CheckpointStore,
    /// 可选会话录制器。None = 不持久化（oneshot 无会话 / 测试）。
    recorder: Option<Arc<Recorder>>,
}

impl Thread {
    /// 用一条用户消息开启会话。
    #[allow(dead_code)] // 子 agent 改用 new_empty + 手动 push 后此构造未被生产路径使用，但单测仍用。
    pub fn new(user_prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::User(user_prompt.into())],
            turns: 0,
            todos: Vec::new(),
            checkpoints: CheckpointStore::default(),
            recorder: None,
        }
    }

    /// 空会话（REPL：首条 user 在第一轮提交时再 append）。
    pub fn new_empty() -> Self {
        Self::default()
    }

    /// 从已加载的历史重建（resume/fork 用）。不重复落盘已有消息。
    pub fn from_parts(messages: Vec<Message>, todos: Vec<TodoItem>, recorder: Arc<Recorder>) -> Self {
        Self {
            messages,
            turns: 0,
            todos,
            checkpoints: CheckpointStore::default(),
            recorder: Some(recorder),
        }
    }

    /// 挂上录制器（新会话）。
    pub fn set_recorder(&mut self, recorder: Arc<Recorder>) {
        self.recorder = Some(recorder);
    }

    /// 取录制器句柄（供 main 落标题等会话级记录）。
    pub fn recorder(&self) -> Option<Arc<Recorder>> {
        self.recorder.clone()
    }

    /// REPL 每轮提交：追加一条 user 消息。
    pub fn append_user(&mut self, prompt: impl Into<String>) {
        let msg = Message::User(prompt.into());
        self.record_msg(&msg);
        self.messages.push(msg);
    }

    pub fn append(&mut self, msg: Message) {
        self.record_msg(&msg);
        self.messages.push(msg);
    }

    /// 整表覆盖 todo 并落盘快照。
    pub fn set_todos(&mut self, todos: Vec<TodoItem>) {
        if let Some(rec) = &self.recorder {
            rec.record(RecordKind::Todo(todos.clone()));
        }
        self.todos = todos;
    }

    /// 落一条 message 记录（落盘前对超大工具输出截断）。
    fn record_msg(&self, msg: &Message) {
        if let Some(rec) = &self.recorder {
            rec.record(RecordKind::Message(cap_for_persist(msg)));
        }
    }
}
