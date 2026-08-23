//! Wayland 协议通道 crate（设计文档 §1 `components/compositor/wayland/`）。
//!
//! 提供 [`WaylandDisplayServer`]：`wl_display` 连接管理、registry 遍历、
//! 通用 wlr 标准协议绑定。各 DE 合成器（KWin/Hyprland/Sway 等）在其上
//! 叠加私有协议。
pub mod registry;
pub mod wlr_protocols;

pub use registry::{global_is_advertised, RegistryState, WaylandDisplayServer};
pub use wlr_protocols::{protocol_versions, WaylandBindings};
