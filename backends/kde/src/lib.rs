//! KDE backend 装配器（设计文档 §4.2–4.3、§3.3 装配矩阵 KDE 行、§21.6）。
//!
//! [`KdeBackend::assemble`] 按装配矩阵初始化公共组件实例并返回
//! [`ComponentRegistry`]。核心原则「DE 封装优先」：电源/通知/外观/启动器
//! 先探测 `org.kde.*` 接口，失败回退 portal / freedesktop 公共组件。
//!
//! 合成器按会话类型选 KWin 双通道（Wayland org_kde_* / X11 EWMH+Scripting）。

pub mod services;

pub use agent_shell_power::{probe_first_existing, service_exists};

mod assemble;

pub use assemble::{KdeBackend, SessionType};
