//! Wayland 显示服务器协议层（**纯 Wayland core**，设计文档 §5）。
//!
//! 提供 [`WaylandDisplayServer`]：`wl_display` 连接管理、registry 遍历、
//! global 接口探测。**不含 wlr 协议绑定**——wlr 标准协议属于
//! `agent-shell-compositor-wlr-wayland`（`components/compositor/wlr-wayland/`，
//! `WlrWaylandCompositor`），各 DE 私有协议属于具体合成器
//! （KWin/Hyprland/Sway 等在其上叠加）。
pub mod core_protocols;
pub mod registry;

pub use core_protocols::{protocol_versions, WaylandCoreBindings};
pub use registry::{global_is_advertised, RegistryState, WaylandDisplayServer};
