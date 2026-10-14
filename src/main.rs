//! `carter` 可执行入口。
//!
//! 逻辑全在库里（`carter::run_cli`），这里只负责进程退出码与顶层错误打印 ——
//! 这样 GUI 与 CLI 共用同一份实现，不会各退化成一套。

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match carter::run_cli().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
