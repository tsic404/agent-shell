//! DDE backend 服务封装（设计文档 §21.6 DDE 装配清单、§21.36 版本兼容矩阵）。
//!
//! DDE20/25 双服务名探测（§21.36.1/§21.36.4）：DDE25 主名 `org.deepin.dde.*`
//! （`com.deepin.daemon.*` 别名并存、方法集一致），DDE20 以 `com.deepin.daemon.*`
//! 为主、少量 `org.deepin.dde.*` 兼容存在（实测 UOS 20 Pro 混合命名）。
//! 启动时先探新名，失败退旧名——不把版本假设写死（HIERARCHY 原则）。
//!
//! 「❓需真机确认」项的可降级路径：
//! - 通知 `org.deepin.dde.Notification1`：服务归属未核实（可能在 dde-shell）。
//!   探测失败 → 回退 freedesktop Notifications → portal。
//! - 外观 `org.deepin.dde.Appearance1`：服务文件不在 dde-daemon。探测失败 →
//!   回退 portal Wallpaper/Settings。
//! - 启动器：DDE25 走 `dde-am` CLI；DDE20 未发现 Application1。两者皆失败 →
//!   公共 `.desktop + gio launch`。
//!
//! 音频封装 [`DdeAudio`]：根对象无 SetVolume/SetMute，控制统一走 Sink 子对象
//! （§21.36.1 实测矩阵）；小步探测服务名 → 对象 → 属性。

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
