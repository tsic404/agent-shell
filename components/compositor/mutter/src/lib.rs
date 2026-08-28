//! Mutter 合成器组件（设计文档 §8，`components/compositor/mutter/`）。
//!
//! GNOME 的 Wayland 协议有意留空——Mutter 不实现
//! `wlr-foreign-toplevel-management` / `ext-foreign-toplevel-list`
//! （隐私设计决策），也无窗口管理私有协议。因此 **D-Bus 通道
//! （`org.gnome.Shell`）就是事实上的基础通道**（「协议缺失 → D-Bus
//! 顶替」的唯一情况），窗口语义双路径：
//!
//! - **Eval**（[`eval`]，GNOME <47）：`org.gnome.Shell.Eval` 直接执行 JS；
//! - **Extension**（[`extension`]，GNOME 47+ 推荐）：Shell Extension
//!   `agent-shell-bridge@multica.dev` 注册 `org.gnome.Shell.AgentShell`。
//!
//! 路径选择（§8.4）：版本探测 → GNOME <47 先 Eval、47+ 先 Extension；
//! 初始路径探测失败自动回退另一路径。显示器配置走 [`display_config`] 的
//! `Mutter.DisplayConfig`（会话无关共享）。截图/输入不在本组件——按设计
//! 走 portal（ScreenCast / RemoteDesktop）。
//!
//! 能力如实上报：GNOME 是最受限的后端——无 move/resize/workspace/
//! 原生事件流（事件仅 Extension 三信号，且归一化依赖 extension.js 部署，
//! T3b 前不声明 window_events）。

pub mod display_config;
pub mod error;
pub mod eval;
pub mod extension;
pub mod version;

mod mutter_compositor;

pub use display_config::{
    ApplyMethod, DisplayConfig, LogicalMonitor, MonitorLayout, MonitorMode, PhysicalMonitor,
};
pub use error::{MutterError, Result, EXTENSION_ID};
pub use eval::GnomeEvalBridge;
pub use extension::{ExtensionRunner, EXTENSION_JS};
pub use mutter_compositor::{GnomePath, MutterCompositor, SessionKind};
pub use version::{detect_version, parse_shell_version, GnomeMajor, GnomeVersion};
