//! Thread —— append-only 消息历史 + 会话级 todo 状态。

use crate::provider::Message;
use crate::tools::TodoItem;

#[derive(Debug, Default)]
pub struct Thread {
    pub messages: Vec<Message>,
    pub turns: u32,
    /// 最新 todo 快照（todo_write 工具整表覆盖）。压缩中保留、每轮复诵到 context 末尾。
    pub todos: Vec<TodoItem>,
}

impl Thread {
    /// 用一条用户消息开启会话。
    pub fn new(user_prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::User(user_prompt.into())],
            turns: 0,
            todos: Vec::new(),
        }
    }

    /// 空会话（REPL：首条 user 在第一轮提交时再 append）。
    pub fn new_empty() -> Self {
        Self::default()
    }

    /// REPL 每轮提交：追加一条 user 消息。
    pub fn append_user(&mut self, prompt: impl Into<String>) {
        self.messages.push(Message::User(prompt.into()));
    }

    pub fn append(&mut self, msg: Message) {
        self.messages.push(msg);
    }
}
