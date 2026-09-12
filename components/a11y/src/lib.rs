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

use agent_shell_core::error::Result;
use agent_shell_core::types::SemanticTarget;
use async_trait::async_trait;

/// daemon 侧 a11y 操作抽象（语义定位 + 元素动作）。
///
/// 生产实现为 [`AtSpiComponent`]；测试注入 fake 断言「定位 → 动作」的
/// 成功派发，而非只在 headless 下断言参数校验（建议项 4）。
#[async_trait]
pub trait A11yOps: Send + Sync {
    /// 按语义描述定位元素（`a11y.query`，§14.3）。
    async fn locate(&self, target: &SemanticTarget) -> Result<Vec<ElementNode>>;
    /// 按 `(bus_name, path)` 二元组定位元素（`a11y.action`，§14.4）。
    async fn locate_by_path(&self, bus: Option<&str>, path: &str) -> Result<ElementNode>;
    /// 触发元素主动作（`Action.DoAction(0)`，§14.4）。
    async fn click(&self, element: &ElementNode) -> Result<()>;
}

pub use action::{ElementActions, PointerInput};
pub use atspi_bridge::AtspiBridge;
pub use component::AtSpiComponent;
pub use semantic_locator::SemanticLocator;
pub use tree::{ApplicationNode, AtspiRole, AtspiState, ElementNode, WindowNode};
