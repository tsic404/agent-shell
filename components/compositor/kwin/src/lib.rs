//! KWin 合成器组件（设计文档 §7，`components/compositor/kwin/`）。
//!
//! 继承层次（§3.3）：`CompositorComponent → WaylandCompositor →
//! KWinCompositor`——Wayland 会话下组合纯 core 的 `WaylandDisplayServer`
//! 基类通道并叠加 org_kde_* 私有协议；X11 会话下组合 `X11DisplayServer`
//! （EWMH + ICCCM + XTest）。
//!
//! 双通道架构（§7.1）：
//! - **Wayland 私有协议通道（基础，首选）**——[`wayland`] 的 `org_kde_*`
//!   协议族（窗口管理 / fake_input / 虚拟桌面），零开销原生接口；
//! - **D-Bus / KWin Scripting 通道（补充，回退）**——[`dbus_bridge`] 经
//!   `org.kde.KWin` Scripting 加载 JS 脚本，callDBus 回传结果。
//!
//! 能力够的走协议，协议不够的走 Scripting（§7.2 通道选择矩阵）。

pub mod dbus_bridge;
pub mod error;
pub mod event_script;
pub mod scripts;
pub mod version;
pub mod wayland;

mod kwin_compositor;

pub use error::{KWinError, Result};
pub use kwin_compositor::{KWinCompositor, SessionKind};
pub use scripts::ScriptTemplate;
pub use version::{parse_support_information, KWinMajor, KWinVersion};
pub use wayland::{BindFailureKind, FakeInput, KWinProtocols, WindowManagement};
