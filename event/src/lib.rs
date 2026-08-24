//! agent-shell event crate — 事件流系统（协议无关）。
//!
//! 见 [`crate`] 文档与设计文档 §18 / §22.5（D4）。

pub mod adapter;
pub mod events;
pub mod hub;
pub mod normalize;
pub mod ring;

pub use adapter::{EventNormalizer, RawEvent, RawSource};
pub use hub::{EventHub, EventSubscription};
pub use ring::EventRing;

pub use crate::events::{DesktopEvent, EventFilter, EventPriority, EventSource};

// 与 core 的 EventStream trait 保持兼容出口：合成器 subscribe() 返回的
// Box<dyn EventStream> 由具体组件包装为本 crate 的 EventSource。
pub use agent_shell_core::event::EventStream;
