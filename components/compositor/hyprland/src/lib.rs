//! Hyprland 合成器组件 crate（设计文档 §9 / §3.3 继承层次）。
//!
//! [`HyprlandCompositor`] 组合 [`WlrWaylandCompositor`] 基类（wlr 标准
//! 协议完整实现），叠加 hyprland_* 私有协议（[`wayland`]）与 hyprctl
//! socket IPC（[`hyprctl`]/[`event_socket`]）双通道：
//!
//! ```text
//! CompositorComponent → WaylandCompositor → WlrWaylandCompositor
//!                                            └── HyprlandCompositor（+ hyprland_* + hyprctl）
//! ```
//!
//! 纯 Wayland 会话（无 X11 变体）；操作选择矩阵见设计文档 §9.5。

pub mod cache;
pub mod compositor;
pub mod event_socket;
pub mod hyprctl;
pub mod protocol_gen;
pub mod wayland;

pub use cache::WindowCache;
pub use compositor::HyprlandCompositor;
pub use event_socket::EventTaskHandle;
pub use hyprctl::Hyprctl;
pub use wayland::{protocol_versions, HyprlandBindings, HyprlandState};
