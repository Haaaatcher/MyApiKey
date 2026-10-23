//! # myapikey-cli
//!
//! MyApiKey 的终端前端。`myapikey` 与 `mak` 两个二进制共享本 crate 的
//! [`cli::run`]，行为完全一致（ADR D11）。
//!
//! 分层铁律：本 crate 只负责“交互”（参数解析、终端输入输出、剪贴板），
//! 所有业务逻辑都调 `myapikey-core`，不自己实现任何加密/同步/存储。

pub mod cli;
pub mod commands;
pub mod console;
pub mod prompt;
pub mod table;

/// 程序入口：解析参数并执行子命令。
///
/// # Errors
/// 任何命令失败都返回 [`myapikey_core::Error`]，由 `main` 统一转成
/// 退出码 1 与错误输出。
pub fn run() -> myapikey_core::Result<()> {
    cli::run()
}
