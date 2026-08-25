//! `WaylandCompositor` 抽象基类（纯 Wayland core，不绑定 wlr 协议）。
//!
//! 层次：`CompositorComponent → WaylandCompositor → {WlrWayland, KWin, Mutter}`。
pub use agent_shell_displayserver_wayland::WaylandDisplayServer;

mod compositor;

pub use compositor::WaylandCompositor;
