//! `AtSpiComponent`——核心 `A11yComponent` 契约的实现与探测装配
//! （设计文档 §3.1 组件总表、§4.2 装配流程 a11y 探测行）。
//!
//! 探测：session bus 上 `org.a11y.Bus` 存在（a11y 支持已启用）即装配；
//! TTY 无图形会话时为 None（§3.1 装配矩阵最后一列）。KDE/DDE/GNOME/
//! Hyprland/Sway/X11Generic 各 backend 共用本公共组件。

use crate::action::ElementActions;
use crate::atspi_bridge::AtspiBridge;
use crate::semantic_locator::SemanticLocator;
use agent_shell_core::component::{
    A11yComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use agent_shell_core::error::Result;
use async_trait::async_trait;

/// AT-SPI 公共无障碍组件。
///
/// 桥接在构造时连接并缓存；语义定位与元素操作基于同一连接
/// （`AtspiBridge::clone_bridge` 共享 zbus 连接，内部 Arc）。
pub struct AtSpiComponent {
    bridge: AtspiBridge,
}

impl AtSpiComponent {
    /// 连接 a11y bus 并构造组件。
    ///
    /// 失败时返回错误——装配层应捕获并把 registry 的 `a11y` 槽位保持
    /// 为 None；本组件自身不做半可用状态。
    pub async fn connect() -> Result<Self> {
        let bridge = AtspiBridge::connect().await?;
        Ok(Self { bridge })
    }

    /// 运行时探测：a11y 总线可达则返回组件，否则 None。
    pub async fn probe() -> Option<Self> {
        match Self::connect().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::info!(error = %e.to_string(), "AT-SPI unavailable; a11y component not assembled");
                None
            }
        }
    }

    /// 语义定位引擎（共享桥接连接）。
    pub fn locator(&self) -> SemanticLocator {
        SemanticLocator::new(self.bridge.clone_bridge())
    }

    /// 元素操作封装（共享桥接连接）。
    pub fn actions(&self) -> ElementActions {
        ElementActions::new(self.bridge.clone_bridge())
    }
}

#[async_trait]
impl DesktopComponent for AtSpiComponent {
    fn name(&self) -> &'static str {
        "atspi-a11y"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::A11y
    }

    /// 构造期可用性：`probe()`/`connect()` 成功即已连上 a11y bus，恒为
    /// true。运行期总线断开等降级由 [`Self::health`] 异步判定——与
    /// 其它组件的「构造探测结果缓存」语义一致（DesktopComponent 契约）。
    fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        if !AtspiBridge::bus_available().await {
            return ComponentHealth::Unavailable;
        }
        match self.bridge.list_applications().await {
            // 空应用集是合法状态（无 a11y 应用注册），不算降级
            Ok(_) => ComponentHealth::Healthy,
            Err(e) => ComponentHealth::Degraded(format!("tree query failed: {e}")),
        }
    }
}

#[async_trait]
impl A11yComponent for AtSpiComponent {
    async fn registry_available(&self) -> Result<bool> {
        // trait 方法名沿用核心契约；实现委托给 bus_available
        Ok(AtspiBridge::bus_available().await)
    }
}
