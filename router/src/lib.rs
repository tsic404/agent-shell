//! 语义路由层（design §15）。
//!
//! 路由层是 CLI/daemon 与各组件之间的唯一命令入口：把语义化 [`Command`] 翻译为对
//! CompositorComponent / Input / Capture / A11y 的具体调用，并在语义通道不可用时按链
//! 降级。**现状**：本 crate 为测试用实现——[`Executor`] 无生产调用点（仅
//! `executor/tests.rs` 使用），daemon JSON-RPC handler 直调 Daemon/CaptureDispatcher 与
//! 合成器后端。语义定位优先级链见设计文档 §15.3。

#![forbid(unsafe_code)]

pub mod command;
pub mod dispatcher;
pub mod executor;

pub use command::{Command, CommandResult};
pub use executor::Executor;
