//! WlrWaylandCompositor crate（设计文档 §3.3 继承层次 / §11）。
//!
//! [`WlrWaylandCompositor`] 继承 `WaylandCompositor`（纯 core 抽象基类）
//! 并叠加 wlr 标准协议绑定：foreign-toplevel / output-management /
//! screencopy / virtual-pointer。自身是完整可装配的兜底合成器；
//! Treeland / Hyprland / Sway 组合它并叠加各自私有协议。
pub mod compositor;
pub mod wlr_protocols;

pub use compositor::{WlrState, WlrWaylandCompositor};
pub use wlr_protocols::{protocol_versions, WlrBindings};
