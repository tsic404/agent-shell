//! X11Compositor crate（设计文档 §3.3 / §11）。
//!
//! [`X11Compositor`] 组合 [`X11DisplayServer`]（EWMH + ICCCM + XTest + MIT-SHM），
//! 用 x11rb 原生协议实现 [`CompositorComponent`] 全部 17 方法；CLI 工具
//! （[`commands::X11Commands`]）仅在原生路径失败时兜底、缺失不阻塞（`cmd` 为 Option）。
//! 装配定位（§4.3）：`DesktopEnvironment::X11Generic → X11Compositor`。事件流：
//! X11 无合成器级原生事件推送，`window_events = false`。

pub mod capture;
pub mod commands;
pub mod ewmh;
pub mod xtest;

mod compositor;

pub use commands::X11Commands;
pub use compositor::X11Compositor;
