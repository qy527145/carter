//! 插话队列：一轮**进行中**用户又发了一条消息，不等这一轮结束，在**下一次请求**里带给模型。
//!
//! ## 为什么不再排到轮末
//!
//! 一轮里模型可能连调几十次工具、跑好几分钟。用户看着它干活补一句「顺便把测试也跑一下」，
//! 排到轮末才被看到时它往往已经做完、甚至做偏了。`run_turn` 每次发请求前问一次
//! [`Steer`]，这里把排着的话交出去，作为一条 user 消息紧跟在工具结果后面。
//!
//! ## 什么时候收、什么时候不收
//!
//! 只在 `run_turn` 跑着时「开门」（[`SteerInbox::set_open`]）：空闲时没人来取，输入照旧
//! 走原来的队列。这一轮后面不再发请求（模型已经在写最后一段话了）时没被取走的，由宿主
//! 用 [`SteerInbox::drain`] 取回来，作为下一轮正常输入。
//!
//! ## 抬头
//!
//! 送进历史的正文带一行 [`wrap`] 抬头，告诉模型：这是用户在它干活途中补的话，不是工具
//! 输出。宿主可以按它把这条消息显示成插话而不是普通用户气泡。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// 插话正文的第一行：标记 + 给模型的一句说明。
pub const STEER_HEAD: &str = "【插话】用户在这一轮进行中补充的话（不是工具输出）：";

/// 给模型看的正文。
pub fn wrap(text: &str) -> String {
    format!("{STEER_HEAD}\n{text}")
}

/// 这段用户正文是不是插话（认整行抬头，不只看前缀）。
pub fn is_steer(text: &str) -> bool {
    text.starts_with(STEER_HEAD)
}

/// 一条会话的插话队列。宿主持有它，同时交给 `RunOptions.steer`。
#[derive(Debug, Default)]
pub struct SteerInbox {
    open: AtomicBool,
    pending: Mutex<Vec<String>>,
}

impl SteerInbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// `run_turn` 开跑时开门、跑完关门。
    pub fn set_open(&self, open: bool) {
        self.open.store(open, Ordering::SeqCst);
    }

    /// 此刻有没有一轮在跑、能在下一次请求里把话带过去。
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    /// 排一条。调用方应当已经用 [`wrap`] 加过抬头。
    pub fn push(&self, text: String) {
        self.pending.lock().unwrap().push(text);
    }

    /// 全部取走（按到达顺序）。
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending.lock().unwrap())
    }

    /// 排着几条（不取走）。
    pub fn len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl crate::agent::Steer for SteerInbox {
    fn take(&self) -> Vec<String> {
        self.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Steer;

    #[test]
    fn wrapped_text_is_recognised_and_keeps_the_original() {
        let t = wrap("顺便跑一下测试");
        assert!(is_steer(&t));
        assert_eq!(t.strip_prefix(STEER_HEAD).unwrap(), "\n顺便跑一下测试");
        assert!(!is_steer("顺便跑一下测试"));
        assert!(!is_steer("【插话】这是用户自己打的"));
    }

    #[test]
    fn taking_hands_over_in_order_and_consumes() {
        let q = SteerInbox::default();
        q.push(wrap("m1"));
        q.push(wrap("m2"));
        assert_eq!(q.take(), vec![wrap("m1"), wrap("m2")]);
        assert!(q.take().is_empty(), "取走即消费");
    }

    #[test]
    fn open_flag_marks_whether_a_turn_is_running() {
        let q = SteerInbox::default();
        assert!(!q.is_open());
        q.set_open(true);
        assert!(q.is_open());
        q.set_open(false);
        assert!(!q.is_open());
        assert!(q.is_empty());
    }
}
