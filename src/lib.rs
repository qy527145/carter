//! Carter —— Rust agent CLI 的核心库。
//!
//! 这里是被 GUI（carter-app）、NDJSON host、服务端复用的那一层；命令行外壳在
//! `crate::cli`（`run_cli`），可执行入口是 `src/main.rs` 的六行 shim。
//!
//! ## 分层
//!
//! * [`provider`] —— 能力抽象层：`LlmProvider` trait 与归一化事件流。**agent loop
//!   只依赖这一层**，不认识任何 wire 细节。
//! * [`tools`] / [`skills`] / [`mcp`] —— 工具、技能与 MCP，三者最终合并进同一个
//!   [`tools::ToolRegistry`]。
//! * [`agent`] —— 多轮循环（`run_turn`）、子 agent、检查点、`UiEvent`/`UiSink`。
//! * [`session`] —— append-only JSONL 会话存储与续接 / 派生 / 压实。
//! * [`config`] / [`registry`] —— 配置加载与模型元数据解析。
//! * [`hooks`] —— 生命周期钩子（`PreToolUse` 可改写参数、可阻断）。
//!
//! ## 给嵌入方的接缝
//!
//! * 渲染 —— 实现 [`agent::UiSink`]，把 [`agent::UiEvent`] 送到你的 UI
//!   （TUI 与 NDJSON 各有一个实现，可作范本）。
//! * 会话 —— [`agent::Thread`] + [`session::Recorder`]。
//! * 自定义工具 —— 实现 [`tools::Tool`]，用 `ToolRegistry::push` 挂上。
//! * 交互式提问 —— [`agent::AskUserQuestionTool`]：模型主动提问，宿主负责弹窗作答。
//!
//! ## 两个只有独立进程才该碰的东西
//!
//! `crate::cli` 里的 `run_cli` 会设置进程环境变量（`[env]` 配置，如 http_proxy）
//! 并初始化 tracing 订阅器。**嵌入方不要调用它** —— 那些是进程级副作用，
//! 在宿主进程里会污染宿主自己的环境与日志。

pub mod agent;
pub mod commands;
pub mod config;
pub mod cost;
pub mod error;
pub mod hooks;
pub mod mcp;
pub mod media;
pub mod memory;
pub mod ndjson;
pub mod prompt;
pub mod provider;
pub mod registry;
pub mod session;
pub mod skills;
pub mod tokens;
pub mod tools;
pub mod tui;
pub mod wizard;

pub use error::{CarterError, Result};

mod cli;
pub use cli::run_cli;
