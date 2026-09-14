//! KWin 合成器组件（设计文档 §7，`components/compositor/kwin/`）。
//!
//! 继承层次（§3.3）：CompositorComponent → WaylandCompositor → KWinCompositor；Wayland
//! 会话组合 `WaylandDisplayServer` 基类通道叠加 org_kde_* 私有协议，X11 会话组合
//! `X11DisplayServer`（EWMH + ICCCM + XTest）。
//!
//! 双通道（§7.1）：Wayland 私有协议（[`wayland`]，零开销原生接口）为首选，D-Bus/KWin
//! Scripting（[`dbus_bridge`]，callDBus 回传）为回退——能力够走协议、不够走 Scripting（§7.2）。
pub mod dbus_bridge;
pub mod error;
pub mod event_ewmh;
pub mod event_script;
pub mod event_source;
pub mod scripts;
pub mod version;
pub mod wayland;

mod kwin_compositor;

pub use error::{KWinError, Result};
pub use kwin_compositor::{KWinCompositor, SessionKind};
pub use scripts::ScriptTemplate;
pub use version::{parse_support_information, KWinMajor, KWinVersion};
pub use wayland::{BindFailureKind, FakeInput, KWinProtocols, WindowManagement};
