//! X11Compositor crate（设计文档 §3.3 / §11）。
//!
//! [`X11Compositor`]：通用 X11 兜底合成器。组合纯协议层的
//! [`X11DisplayServer`]（EWMH + ICCCM + XTest + MIT-SHM），通过 x11rb 原生
//! 协议实现 [`CompositorComponent`] 全部 17 方法；CLI 工具
//! （[`commands::X11Commands`]，xdotool/wmctrl）仅在原生路径失败时兜底，
//! 缺失不阻塞初始化（`cmd` 字段为 `Option`）。
//!
//! 装配定位（§4.3）：`DesktopEnvironment::X11Generic → X11Compositor`。
//! 事件流：X11 无合成器级原生事件推送，`capabilities().window_events = false`；
//! XDamage/XRecord 可选且不属本任务范围（`event` 模块见 T3b）。

pub mod capture;
pub mod commands;
pub mod ewmh;
pub mod xtest;

mod compositor;

pub use commands::X11Commands;
pub use compositor::X11Compositor;
