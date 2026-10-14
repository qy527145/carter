//! Thread —— append-only 消息历史 + 会话级 todo 状态。
//!
//! 消息是 llm_adapt 的 IR（[`WireMessage`]）：它带得住思考签名、工具结果里的图片、
//! 认不出的服务端工具块，也保得住块与块的顺序。旧的纯文本枚举做不到这些 ——
//! 图片只能靠 `[img:hash]` 文本 token 绕，思考签名跨轮就丢。
//!
//! 可选挂一个 session `Recorder`：append/set_todos 时顺手落盘。

use std::sync::Arc;

use crate::provider::WireMessage;
use crate::session::{RecordKind, Recorder, cap_for_persist};
use crate::tools::TodoItem;

use super::checkpoint::CheckpointStore;

#[derive(Debug, Default)]
pub struct Thread {
    pub messages: Vec<WireMessage>,
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
            messages: vec![WireMessage::user_text(user_prompt)],
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
    pub fn from_parts(
        messages: Vec<WireMessage>,
        todos: Vec<TodoItem>,
        recorder: Arc<Recorder>,
    ) -> Self {
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
        let msg = WireMessage::user_text(prompt);
        self.record_msg(&msg);
        self.messages.push(msg);
    }

    pub fn append(&mut self, msg: WireMessage) {
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
    fn record_msg(&self, msg: &WireMessage) {
        if let Some(rec) = &self.recorder {
            rec.record(RecordKind::Message(cap_for_persist(msg)));
        }
    }
}
