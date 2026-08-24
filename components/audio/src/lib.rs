//! 音频组件 crate（设计文档 §21 `components/audio/`）。
//!
//! 提供 [`AudioServerComponent`](agent_shell_core::component::AudioServerComponent)
//! 的公共实现与 DE 封装优先路由：
//!
//! - [`pipewire::PipeWireAudioServer`]：wpctl CLI（WirePlumber 会话管理器）
//! - [`pulseaudio::PulseAudioAudioServer`]：pactl CLI（PipeWire-pulse / pipewire-pulse
//!   / 原生 PulseAudio 均兼容，`@DEFAULT_SINK@` 别名屏蔽服务器差异）
//! - [`DePriorityRouter`]：DE 专有 D-Bus 封装优先 → 公共实现降级（§21.4），
//!   capability 记录实际命中的通道供 `doctor` 输出（§21.36.4）
//!
//! 探测顺序（design/02 §4.2）：pipewire 运行 → PipeWire，否则 pulseaudio 运行 →
//! PulseAudio，否则 None。

pub mod pulseaudio;
pub mod router;

pub mod pipewire;

pub use pipewire::PipeWireAudioServer;
pub use pulseaudio::PulseAudioAudioServer;
pub use router::{AudioChannel, DePriorityRouter};

/// 组件名（doctor 报告用）。
pub const COMPONENT_NAME: &str = "audio";
