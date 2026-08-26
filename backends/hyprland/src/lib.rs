//! Hyprland backend 装配器（§4.2–4.3、§3.3 装配矩阵 Hyprland 行）。
//!
//! 纯 Wayland 合成器（无 X11 变体）。装配清单：
//! - 合成器：HyprlandCompositor（wlr 基类 + hyprland_* 私有协议 + hyprctl socket）
//! - 音频：公共探测链（PipeWire / PulseAudio）
//! - 网络：NetworkManager
//! - 输入 / 截图 / 无障碍 / 剪贴板：公共探测链
//! - 电源：UPower（Hyprland 无 DE 层电源封装）
//! - 通知 / 外观 / 启动器：portal 三件套
//! - 系统服务：Systemd + Logind

use agent_shell_a11y::AtSpiComponent;
use agent_shell_appearance::PortalAppearance;
use agent_shell_capture::CaptureDispatcher;
use agent_shell_clipboard::{SessionKind, WlClipboard};
use agent_shell_compositor_hyprland::HyprlandCompositor;
use agent_shell_core::component::{
    A11yComponent, AppearanceComponent, AudioServerComponent, CaptureComponent, ClipboardComponent,
    CompositorComponent, InputComponent, LauncherComponent, NetworkComponent,
    NotificationComponent, PowerComponent,
};
use agent_shell_core::error::Result;
use agent_shell_core::registry::{BackendKind, ComponentRegistry};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_launcher::DesktopFileLauncher;
use agent_shell_notification::PortalNotification;
use agent_shell_power::UPowerComponent;

/// Hyprland backend 装配器（§4.3 `HyprlandBackend`）。
pub struct HyprlandBackend;

impl Default for HyprlandBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl HyprlandBackend {
    pub fn new() -> Self {
        Self
    }

    pub async fn assemble(&self) -> Result<ComponentRegistry> {
        // 1. 合成器：HyprlandCompositor（wlr 基类 + hyprland_* + hyprctl）
        let compositor: Option<Box<dyn CompositorComponent>> = match HyprlandCompositor::connect() {
            Ok(c) => Some(Box::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "HyprlandCompositor assembly failed; compositor slot None");
                None
            }
        };

        // 2. 电源：UPower（Hyprland 无 DE 层电源封装）
        let power: Option<Box<dyn PowerComponent>> = match UPowerComponent::new().await {
            Ok(u) => Some(Box::new(u)),
            Err(e) => {
                tracing::warn!(error = %e, "UPower failed; power slot None");
                None
            }
        };

        // 3. portal 三件套（通知 / 外观 / 启动器）
        let notification: Option<Box<dyn NotificationComponent>> =
            match PortalNotification::new().await {
                Ok(n) => Some(Box::new(n)),
                Err(e) => {
                    tracing::warn!(error = %e, "portal notification failed; slot None");
                    None
                }
            };
        let appearance: Option<Box<dyn AppearanceComponent>> = match PortalAppearance::new().await {
            Ok(a) => Some(Box::new(a)),
            Err(e) => {
                tracing::warn!(error = %e, "portal appearance failed; slot None");
                None
            }
        };
        let launcher: Option<Box<dyn LauncherComponent>> =
            Some(Box::new(DesktopFileLauncher::new()));

        // 4. 公共组件探测
        let audio = assemble_audio().await?;
        let network = assemble_network().await;
        let input = assemble_input(DesktopEnvironment::Hyprland).await;
        let capture = CaptureDispatcher::assemble().await;
        let a11y = AtSpiComponent::probe().await;
        // Hyprland 纯 Wayland → 剪贴板走 wl-clipboard
        let clipboard: Option<Box<dyn ClipboardComponent>> =
            Some(Box::new(WlClipboard::with_session(SessionKind::Wayland)));
        let (init_system, session_manager) = agent_shell_systemd::assemble_system_services().await;

        Ok(ComponentRegistry {
            compositor,
            audio,
            network,
            input,
            capture: capture.map(|c| Box::new(c) as Box<dyn CaptureComponent>),
            a11y: a11y.map(|a| Box::new(a) as Box<dyn A11yComponent>),
            clipboard,
            power,
            notification,
            appearance,
            launcher,
            display_layout: None,
            init_system,
            session_manager,
        })
    }
}

async fn assemble_audio() -> Result<Option<Box<dyn AudioServerComponent>>> {
    let router = agent_shell_audio::router::assemble_audio_router(BackendKind::Hyprland).await?;
    Ok(router.map(|r| Box::new(r) as Box<dyn AudioServerComponent>))
}

async fn assemble_network() -> Option<Box<dyn NetworkComponent>> {
    match agent_shell_network::detect(BackendKind::Hyprland).await {
        Some((n, _)) => Some(n),
        None => {
            tracing::info!("network component unavailable (no NM / networkd)");
            None
        }
    }
}

async fn assemble_input(de: DesktopEnvironment) -> Option<Box<dyn InputComponent>> {
    match agent_shell_input::detect(de).await {
        Ok(h) => Some(Box::new(h)),
        Err(e) => {
            tracing::info!(error = %e, "input component unavailable");
            None
        }
    }
}
