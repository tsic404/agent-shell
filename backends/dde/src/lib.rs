//! DDE backend 服务封装（设计文档 §21.6 DDE 装配清单、§21.36 版本兼容矩阵）。
//!
//! DDE20/25 双服务名探测（§21.36.1/§21.36.4）：DDE25 主名 `org.deepin.dde.*`、
//! DDE20 主名 `com.deepin.daemon.*`（实测 UOS 20 Pro 混合命名）——启动先探新名、
//! 失败退旧名，不把版本假设写死（HIERARCHY 原则）。通知/外观/启动器等
//! 「❓需真机确认」项按探测失败逐步回退公共通道；音频 [`DdeAudio`] 控制统一
//! 走 Sink 子对象（§21.36.1 实测矩阵），小步探测服务名 → 对象 → 属性。

pub mod compositor;
pub mod dde_api;
pub mod dde_audio;
pub mod protocol_gen;
#[allow(clippy::module_inception)]
pub mod treeland;
pub mod version;

mod assemble;
mod dde_compositor;

pub use compositor::{detect_compositor, CompositorKind};
pub use dde_compositor::{Compositor, DdeCompositor};
pub use treeland::{doctor_line as treeland_doctor_line, TreelandBindings};
pub use version::DdeVersion;

pub use agent_shell_power::{probe_first_existing, service_exists};
pub use assemble::{DdeBackend, SessionType};
pub use dde_audio::{AudioServiceVariant, DdeAudio};
