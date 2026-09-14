//! 网络组件 crate（设计文档 §21 `components/network/`）。
//!
//! 提供 [`NetworkComponent`](agent_shell_core::component::NetworkComponent) 的公共实现
//! 与探测装配：networkmanager（system bus 标准 D-Bus，跨 DE 首选）、networkd（只读
//! 降级、写操作明确 Unavailable）、[`detect()`]（NM 在位 → NM，否则 networkd-only →
//! 只读，否则 None）。DDE 专有封装归属 backend 装配层，版本差异见 §21.36.1。

pub mod networkd;
pub mod networkmanager;

/// 组件名（doctor 报告用）。
pub const COMPONENT_NAME: &str = "network";

pub use networkd::SystemdNetworkdComponent;
pub use networkmanager::NetworkManagerComponent;

use agent_shell_core::component::NetworkComponent;
use agent_shell_core::registry::BackendKind;

/// NetworkManager 系统总线服务名。
pub const NM_SERVICE: &str = "org.freedesktop.NetworkManager";

/// 探测并组装网络组件（design/02 §4.2：NM 在位 → NM；TTY 用 systemd-networkd）。
///
/// 返回：
/// - `(Backend, true)`：NetworkManager 可达
/// - `(networkd-only, false)`：无 NM 但 systemd-networkd 存在 → 只读降级
/// - `None`：两者皆不可用（容器/无网络管理环境）
///
pub async fn detect(_de: BackendKind) -> Option<(Box<dyn NetworkComponent>, bool)> {
    let nm = NetworkManagerComponent::connect().await.ok()?;
    if !nm.probe().await {
        let nd = SystemdNetworkdComponent::new();
        return if nd.probe().await {
            Some((Box::new(nd), false))
        } else {
            None
        };
    }
    Some((Box::new(nm), true))
}
