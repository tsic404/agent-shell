//! X11 显示服务器协议层（设计文档 §6）。
//!
//! [`X11DisplayServer`] 提供 x11rb 连接管理、EWMH 原子缓存（[`ewmh`]）、
//! ICCCM 协议操作（[`icccm`]：`WM_PROTOCOLS`/`WM_DELETE_WINDOW`/`WM_STATE`）
//! 与 `_NET_WM` 协议操作。不实现 `CompositorComponent`——各 X11 会话合成器
//! （KWin/Mutter/DDE/通用兜底）在其上追加 D-Bus 接口。
//!
//! XTest 注意（§6.3）：仅在原生 X11 会话（`XDG_SESSION_TYPE=X11`）下可用；
//! XWayland 下被禁用，输入注入应走 libei/EIS（T2a）。
pub mod ewmh;
pub mod icccm;

pub use ewmh::server::X11DisplayServer;
pub use ewmh::{moveresize_flags, wm_state_action, EwmhAtoms};
pub use icccm::{size_hints_flags, wm_state as icccm_wm_state, WmProtocol, WmProtocolAtoms};
