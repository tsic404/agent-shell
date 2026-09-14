//! X11 显示服务器协议层（设计文档 §6）。
//!
//! [`X11DisplayServer`] 提供 x11rb 连接管理、EWMH 原子缓存（[`ewmh`]）、ICCCM 协议
//! 操作（[`icccm`]）与 `_NET_WM` 操作；不实现 CompositorComponent——各 X11 会话合成器
//! 在其上追加 D-Bus 接口。XTest 注意（§6.3）：仅原生 X11 可用，XWayland 下禁用。
pub mod ewmh;
pub mod icccm;

pub use ewmh::server::X11DisplayServer;
pub use ewmh::{moveresize_flags, wm_state_action, EwmhAtoms};
pub use icccm::{size_hints_flags, wm_state as icccm_wm_state, WmProtocol, WmProtocolAtoms};
