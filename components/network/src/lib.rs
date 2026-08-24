//! 网络组件 crate（设计文档 §21 `components/network/`）。
//!
//! 提供 [`NetworkComponent`](agent_shell_core::component::NetworkComponent)
//! 的公共实现与探测装配：
//!
//! - [`networkmanager::NetworkManagerComponent`]：`org.freedesktop.NetworkManager`
//!   （system bus）标准 D-Bus 接口——跨 DE 首选。
//! - [`networkd::SystemdNetworkdComponent`]：`systemd-networkd` 环境的只读降级
//!   （状态查询 + 明确 Unavailable 写操作），不 panic（§21 验收）。
//! - [`detect()`]：探测顺序 `org.freedesktop.NetworkManager` 在位 → NM，
//!   否则 networkd-only → 只读组件，否则 None。
//!
//! DDE 专有封装（DDE25 dde-network-core + NM / DDE20 `com.deepin.daemon.Network`）
//! 归属 backend 装配层；两版差异见设计文档 §21.36.1。

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
