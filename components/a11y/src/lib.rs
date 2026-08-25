//! agent-shell a11y crate（设计文档 §14 AT-SPI 无障碍模块）。
//!
//! 四层结构：
//! - [`atspi_bridge`]：AT-SPI D-Bus 桥接（a11y bus，`org.a11y.atspi.*`）
//! - [`tree`]：无障碍树模型（`ApplicationNode` / `WindowNode` / `ElementNode`）
//! - [`semantic_locator`]：语义定位引擎（按角色/名称/父子关系定位）
//! - [`action`]：元素操作封装（click/focus/get_text/set_text）
//!
//! 装配：[`AtSpiComponent::probe`] 探测 `org.a11y.Bus` 可达后构造组件，
//! 实现核心的 [`A11yComponent`] 契约；不可达时 `is_available() == false`、
//! doctor 显示 Unavailable 而非崩溃（验收标准）。

#![forbid(unsafe_code)]

pub mod action;
pub mod atspi_bridge;
pub mod component;
pub mod semantic_locator;
pub mod tree;

pub use action::{ElementActions, PointerInput};
pub use atspi_bridge::AtspiBridge;
pub use component::AtSpiComponent;
pub use semantic_locator::SemanticLocator;
pub use tree::{ApplicationNode, AtspiRole, AtspiState, ElementNode, WindowNode};
