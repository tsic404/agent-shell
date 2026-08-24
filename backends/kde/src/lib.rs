//! KDE backend 服务封装（设计文档 §21.6 KDE 装配清单、§3.5 KDE/Plasma 6 调研）。
//!
//! 内部路由（§21.4 DePriorityRouter）：`services.rs` 探测
//! `org.kde.Solid.PowerManagement` → 命中用 KdePowerDevil，
//! 否则回退 UPower/portal 公共组件。
//!
//! - 电源：org.kde.Solid.PowerManagement（powerdevil）+ org.kde.kscreenlocker 锁屏
//!   （bus `org.kde.screensaver`，接口 `org.kde.screensaver`，方法 `configure` +
//!   信号 `AboutToLock`；锁屏动作按 §21.3 走 kscreenlocker `lock()` 语义）。
//! - 通知：KDE 后端即 `org.freedesktop.Notifications` 的实现
//!   （xdg-desktop-portal-kde 转发同一总线），公共封装即可覆盖。
//! - 外观：plasma-apply-wallpaperimage / plasma-apply-colorscheme CLI。
//! - 启动器：公共 `.desktop + gio launch` 路径；klauncher 的 exec_blind 已属
//!   遗留接口，KDE Plasma 6 实际经 KIO/applicationlauncher，不依赖。

pub mod services;

pub use agent_shell_power::{probe_first_existing, service_exists};
