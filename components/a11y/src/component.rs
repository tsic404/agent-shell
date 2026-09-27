//! `AtSpiComponent`——核心 `A11yComponent` 契约的实现与探测装配
//! （设计文档 §3.1 组件总表、§4.2 装配流程 a11y 探测行）。
//!
//! 探测是**被动**的：session bus 上 `org.a11y.Bus` 已注册或可激活即装配，
//! 不激活 a11y bus——激活是会话级副作用（`$XDG_RUNTIME_DIR/at-spi/bus_0`
//! 按 runtime dir 共享，嵌套 session bus 里激活会把正在服务的 bus 换掉，
//! 详见 `atspi_bridge` 类型文档）。连接延迟到首次使用，失效后自动重连。
//! TTY / 无 at-spi2 时为 None（§3.1 装配矩阵最后一列）。KDE/DDE/GNOME/
//! Hyprland/Sway/X11Generic 各 backend 共用本公共组件。

use crate::action::ElementActions;
use crate::atspi_bridge::AtspiBridge;
use crate::semantic_locator::SemanticLocator;
use crate::tree::ElementNode;
use crate::A11yOps;
use agent_shell_core::component::{
    A11yComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use agent_shell_core::error::Result;
use agent_shell_core::types::SemanticTarget;
use async_trait::async_trait;

/// 置位启用开关后等待工具包注册的上限。
///
/// 工具包收到 `org.a11y.Status` 变更信号后才连 a11y bus 并注册（实测 KDE
/// Plasma 6 Wayland 首次置位后 <1s 完成 5 → 23 个应用）；上限只用于防御
/// 「本会话确实没有应用注册」时不把 a11y 命令拖长。
const ENABLE_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// 注册等待轮询间隔。
const ENABLE_SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// AT-SPI 公共无障碍组件。
///
/// 语义定位与元素操作共享同一连接槽位（`AtspiBridge::clone_bridge`）：
/// 连接按需建立、失效重连，见 `atspi_bridge`。
pub struct AtSpiComponent {
    bridge: AtspiBridge,
}

impl AtSpiComponent {
    /// 连接 a11y bus 并构造组件（显式立即连接；装配期用 [`Self::probe`]）。
    ///
    /// 失败时返回错误——装配层应捕获并把 registry 的 `a11y` 槽位保持
    /// 为 None；本组件自身不做半可用状态。
    pub async fn connect() -> Result<Self> {
        let bridge = AtspiBridge::connect().await?;
        Ok(Self { bridge })
    }

    /// 运行时探测（被动，**不建连接**）：AT-SPI 支持存在（`org.a11y.Bus` 已注册
    /// 或可激活）则返回组件，否则 None。
    ///
    /// 装配只做被动判定，连接（socket 候选 → `GetAddress` → dial）一律延迟到
    /// 首次使用：装配期激活 a11y bus 有会话级副作用，且退化态（launcher 挂死、
    /// socket 已被替换）下同步连接会把装配拖到 GetAddress 挂满超时。
    pub async fn probe() -> Option<Self> {
        let Some(launcher) = AtspiBridge::launcher_state().await else {
            tracing::info!("AT-SPI unavailable; session bus unreachable");
            return None;
        };
        if !launcher.is_supported() {
            tracing::info!("AT-SPI unavailable; org.a11y.Bus absent and not activatable");
            return None;
        }
        Some(Self {
            bridge: AtspiBridge::deferred(),
        })
    }

    /// 语义定位引擎（共享桥接连接）。
    pub fn locator(&self) -> SemanticLocator {
        SemanticLocator::new(self.bridge.clone_bridge())
    }

    /// 元素操作封装（共享桥接连接）。
    pub fn actions(&self) -> ElementActions {
        ElementActions::new(self.bridge.clone_bridge())
    }

    /// 无障碍树可见性前置：建立连接 → 确保会话 AT-SPI 已启用，刚置位时等待
    /// 工具包注册。
    ///
    /// 先连接后置位：a11y bus 不可达时立即返回精确错误，不去改动会话启用状态。
    /// 默认会话里 `org.a11y.Status` 两位皆为 false，工具包不注册 → Registry
    /// 可达而应用树为空。置位是**异步生效**的（工具包收到属性变更信号后才连
    /// a11y bus），置位后立即枚举会漏掉已开应用，故有界轮询到出现应用或超时。
    /// 已启用（含用户自开读屏）时不写总线、不等待。
    async fn prepare_tree(&self) -> Result<()> {
        self.bridge.ensure_connected().await?;
        if !self.bridge.ensure_enabled().await? {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + ENABLE_SETTLE_TIMEOUT;
        loop {
            if !self.bridge.list_applications().await?.is_empty() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::debug!("no a11y application registered after enabling AT-SPI");
                return Ok(());
            }
            tokio::time::sleep(ENABLE_SETTLE_POLL).await;
        }
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

    /// 构造期可用性：`probe()` 通过即 AT-SPI 支持存在，恒为 true。运行期
    /// 总线断开等降级由 [`Self::health`] 异步判定——与其它组件的「构造探测
    /// 结果缓存」语义一致（DesktopComponent 契约）。
    fn is_available(&self) -> bool {
        true
    }

    /// a11y bus 未启动（仅可激活）只报 Degraded——首次使用时按需启动，不是
    /// 永久不可用；装配期不激活是刻意的（见 [`AtSpiComponent::probe`]）。
    async fn health(&self) -> ComponentHealth {
        match AtspiBridge::launcher_state().await {
            None => ComponentHealth::Unavailable,
            Some(launcher) if !launcher.is_supported() => ComponentHealth::Unavailable,
            Some(launcher) if !launcher.has_owner => ComponentHealth::Degraded(
                "a11y bus not started (org.a11y.Bus activatable on demand)".into(),
            ),
            Some(_) => match self.bridge.list_applications().await {
                // 空应用集是合法状态（无 a11y 应用注册），不算降级
                Ok(_) => ComponentHealth::Healthy,
                Err(e) => ComponentHealth::Degraded(format!("tree query failed: {e}")),
            },
        }
    }
}

#[async_trait]
impl A11yComponent for AtSpiComponent {
    /// AT-SPI Registry 是否可达。
    ///
    /// 被动判定（不激活 a11y bus）：有存活连接时查该连接上的注册；无存活连接
    /// 时按 socket 候选 → 已注册 launcher 的顺序被动探测；a11y bus 仅「可激活」
    /// （未启动）时返回 false（fail-closed）——与 daemon 探测报
    /// `a11y bus not started` 口径一致，不用「支持存在」冒充「Registry 可达」。
    async fn registry_available(&self) -> Result<bool> {
        Ok(self.bridge.registry_reachable().await)
    }
}

#[async_trait]
impl A11yOps for AtSpiComponent {
    async fn locate(&self, target: &SemanticTarget) -> Result<Vec<ElementNode>> {
        self.prepare_tree().await?;
        self.locator().locate(target).await
    }

    async fn locate_by_path(&self, bus: Option<&str>, path: &str) -> Result<ElementNode> {
        self.prepare_tree().await?;
        self.locator().locate_by_path(bus, path).await
    }

    async fn click(&self, element: &ElementNode) -> Result<()> {
        self.prepare_tree().await?;
        self.actions().click(element).await
    }
}
