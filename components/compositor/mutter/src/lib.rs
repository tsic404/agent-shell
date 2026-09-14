//! Mutter 合成器组件（设计文档 §8，`components/compositor/mutter/`）。
//!
//! GNOME Wayland 协议有意留空（不实现 foreign-toplevel、也无私有协议），因此 D-Bus
//! 通道（`org.gnome.Shell`）就是事实基础通道。窗口语义双路径：Eval（[`eval`]，
//! GNOME <47）与 Extension（[`extension`]，47+ 推荐）；路径选择按版本探测、初始
//! 路径失败自动回退另一路径（§8.4）。显示器走 [`display_config`] 的
//! `Mutter.DisplayConfig`；截图/输入不在本组件，按设计走 portal。能力如实上报：
//! GNOME 是最受限后端——无 move/resize/workspace/原生事件流。

pub mod display_config;
pub mod error;
pub mod eval;
pub mod extension;
pub mod install;
pub mod version;

mod mutter_compositor;

pub use display_config::{
    ApplyMethod, DisplayConfig, LogicalMonitor, MonitorLayout, MonitorMode, PhysicalMonitor,
};
pub use error::{MutterError, Result, EXTENSION_ID};
pub use eval::GnomeEvalBridge;
pub use extension::{ExtensionRunner, EXTENSION_JS, EXTENSION_METADATA};
pub use mutter_compositor::{GnomePath, GnomePathKind, MutterCompositor, SessionKind};
pub use version::{detect_version, parse_shell_version, GnomeMajor, GnomeVersion};
