//! systemd-networkd 只读降级组件。
//!
//! design/02 §4.2：`org.freedesktop.NetworkManager` 不在位且环境为
//! networkd-only（服务器/最小安装/TTY）时，网络管理降级为**只读状态查询**；
//! 写操作（连接/断开 WiFi）明确返回 `Unavailable`，不 panic、不静默成功
//! （§21 验收「无 NM 环境降级到只读或明确 Unavailable」）。
//!
//! 数据源：
//! - `networkctl` CLI（若在位）——仅用于 health 探测与链路枚举的旁证；
//! - `/run/systemd/netif/state` 与 `/sys/class/net`——无外部依赖的直读通道。

use std::path::PathBuf;

use async_trait::async_trait;

use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, NetworkComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{Connectivity, NetworkState, WifiNetwork};

/// networkd 运行时状态目录（systemd-networkd 运行时才存在）。
const NETIF_STATE_DIR: &str = "/run/systemd/netif";

/// systemd-networkd 只读网络组件。
#[derive(Debug, Default)]
pub struct SystemdNetworkdComponent;

impl SystemdNetworkdComponent {
    /// 构造。
    pub fn new() -> Self {
        Self
    }

    /// networkd 是否运行：netif 状态目录存在即视为运行时在位。
    pub async fn probe(&self) -> bool {
        tokio::fs::metadata(NETIF_STATE_DIR).await.is_ok()
    }

    /// 枚举非回环网络接口名。
    fn interfaces() -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
            return out;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name != "lo" {
                out.push(name);
            }
        }
        out.sort();
        out
    }

    /// 接口是否 up（operstate 为 unknown 的虚拟接口按 down 处理）。
    fn interface_up(name: &str) -> bool {
        std::fs::read_to_string(format!("/sys/class/net/{name}/operstate"))
            .map(|s| s.trim() == "up")
            .unwrap_or(false)
    }

    fn unavailable_err(op: &str) -> AgentShellError {
        AgentShellError::BackendUnavailable(format!(
            "write operation {op:?} requires NetworkManager (environment is networkd-only)"
        ))
    }
}

#[async_trait]
impl DesktopComponent for SystemdNetworkdComponent {
    fn name(&self) -> &'static str {
        "SystemdNetworkdComponent"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Network
    }

    fn is_available(&self) -> bool {
        // 同步视图：运行时目录存在性是廉价的同步可判定信号。
        PathBuf::from(NETIF_STATE_DIR).exists()
    }

    async fn health(&self) -> ComponentHealth {
        if self.probe().await {
            ComponentHealth::Degraded(
                "read-only: no NetworkManager on this host (networkd-only environment)".into(),
            )
        } else {
            ComponentHealth::Unavailable
        }
    }
}

#[async_trait]
impl NetworkComponent for SystemdNetworkdComponent {
    async fn get_network_state(&self) -> Result<NetworkState> {
        if !self.probe().await {
            return Err(AgentShellError::BackendUnavailable(
                "systemd-networkd runtime not present".into(),
            ));
        }
        let ifs = Self::interfaces();
        let any_up = ifs.iter().any(|i| Self::interface_up(i));
        Ok(NetworkState {
            connectivity: if any_up {
                // 有 up 接口只能证明链路层；外网可达性未知 → Limited 而非 Full。
                Connectivity::Limited
            } else {
                Connectivity::None
            },
            wifi_enabled: false,
            active_ssid: None,
            ip_address: None,
            metered: false,
        })
    }

    async fn list_wifi_networks(&self) -> Result<Vec<WifiNetwork>> {
        Err(Self::unavailable_err("wifi scan"))
    }

    async fn connect_wifi(&self, _ssid: &str, _password: Option<&str>) -> Result<()> {
        Err(Self::unavailable_err("wifi connect"))
    }

    async fn disconnect_wifi(&self) -> Result<()> {
        Err(Self::unavailable_err("wifi disconnect"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_ops_return_structured_unavailable() {
        // networkd-only 降级契约：写操作明确 Unavailable（不 panic、不错误类别混淆）。
        let err = futures_lite::future::block_on(async {
            SystemdNetworkdComponent::new()
                .connect_wifi("ssid", None)
                .await
                .unwrap_err()
        });
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("NetworkManager"));
    }

    #[tokio::test]
    async fn state_query_degrades_or_reports_absent() {
        let c = SystemdNetworkdComponent::new();
        match c.get_network_state().await {
            Ok(state) => {
                // networkd 在位：只读字段必须诚实为空/受限。
                assert_eq!(state.active_ssid, None);
                assert!(!state.wifi_enabled);
                assert!(matches!(
                    state.connectivity,
                    Connectivity::Limited | Connectivity::None
                ));
            }
            Err(e) => {
                assert!(matches!(e, AgentShellError::BackendUnavailable(_)));
            }
        }
    }
}
