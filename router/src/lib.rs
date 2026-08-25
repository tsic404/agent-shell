//! 语义路由层（design/07 §15）。
//!
//! 路由层是 CLI/daemon 与各组件之间的唯一命令入口：
//! 把语义化的 [`Command`] 翻译为对 CompositorComponent / Input / Capture /
//! A11y 的具体调用，并在语义通道不可用时按链降级。
//!
//! # 语义定位优先级链（§15.3）
//!
//! `resolve_target` 及各命令的目标解析遵循以下优先级，从最可靠到最终降级：
//!
//! 1. WindowId（精确，最可靠）
//! 2. app_id + AT-SPI（元素级语义定位）
//! 3. 窗口标题（子串 → 精确 → 正则 → glob）
//! 4. PID
//! 5. 桌面文件
//! 6. 坐标（最终降级）

#![forbid(unsafe_code)]

pub mod command;
pub mod dispatcher;
pub mod executor;

pub use command::{Command, CommandResult};
pub use executor::Executor;
