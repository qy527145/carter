//! NDJSON 模式 —— host driver 通过 stdin/stdout 与 carter 双向通信。
//!
//! 用途：嵌入到 VSCode 扩展、服务端、自动化脚本等 host 程序里。host 启动 `carter --ndjson`
//! 子进程，通过管道送 [`protocol::Command`]，读 [`protocol::Event`]。
//!
//! 与 TUI 模式平级：相同的 agent loop + 工具池 + Hook + 子代理 + AskUser 反向 RPC。
//! 唯一区别是 sink 写 stdout JSON 行而不是渲染终端。

mod protocol;
mod reader;
mod runner;
mod sink;

pub use runner::run_ndjson;
