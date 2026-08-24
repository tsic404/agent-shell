//! GNOME backend 服务封装（设计文档 §21.6 GNOME 装配清单、§3.5 GNOME 调研）。
//!
//! 内部路由：`services.rs` 探测 `org.gnome.SettingsDaemon.Power` →
//! 命中 GnomePower，否则回退 UPower/portal 公共组件。
//!
//! - 锁屏：`org.gnome.ScreenSaver.Lock()`（版本稳定，§21.36.3）。
//! - 音频：GNOME 无 DE 层音量 D-Bus——直接公共组件（不在本 crate 范围）。
//! - 通知：GNOME 后端即 `org.freedesktop.Notifications` 实现
//!   （org.gnome.Shell.Notifications 已弃用），公共封装覆盖。
//! - 外观：gsettings picture-uri / color-scheme + portal。

pub mod services;

pub use agent_shell_power::{probe_first_existing, service_exists};
