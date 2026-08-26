//! TTY backend 装配器（§4.2–4.3、§3.3 装配矩阵 TTY 行、§11.2）。
//!
//! TTY 是**无 DE 的操作系统 Agent**——仅提供系统服务（init、session）。
//! 装配矩阵：合成器/音频/输入/截图/无障碍/剪贴板/电源/通知/外观/启动器全 None，
//! 仅 `init_system`（SystemdComponent）与 `session_manager`（LogindComponent）非 None。
//! 网络可选（systemd-networkd / NetworkManager 探测）。

use agent_shell_core::error::Result;
use agent_shell_core::registry::{BackendKind, ComponentRegistry};

/// TTY backend 装配器（§11.2 `TtyBackend`）。
pub struct TtyBackend;

impl Default for TtyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TtyBackend {
    pub fn new() -> Self {
        Self
    }

    pub async fn assemble(&self) -> Result<ComponentRegistry> {
        // 系统服务（TTY 必选组件族）
        let (init_system, session_manager) = agent_shell_systemd::assemble_system_services().await;

        // 网络：systemd-networkd / NetworkManager（可选，探测失败不致命）
        let network = match agent_shell_network::detect(BackendKind::Tty).await {
            Some((n, _)) => Some(n),
            None => {
                tracing::info!("network unavailable in TTY (no NM / networkd)");
                None
            }
        };

        Ok(ComponentRegistry {
            compositor: None, // 无合成器
            audio: None,
            network,
            input: None,
            capture: None,
            a11y: None,
            clipboard: None,
            power: None,
            notification: None,
            appearance: None,
            launcher: None,
            display_layout: None,
            init_system,
            session_manager,
        })
    }
}
