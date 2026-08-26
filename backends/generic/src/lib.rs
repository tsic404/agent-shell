//! 通用兜底 backend 装配器（§4.2–4.3、§3.3 装配矩阵 X11Generic / WLRWayland 行）。
//!
//! 两个兜底后端：
//! - [`X11Backend`]：无法识别 DE 时的 X11 兜底（X11Compositor，EWMH/XTest 原生）
//! - [`WlrWaylandBackend`]：未知 Wayland 合成器兜底（WlrWaylandCompositor，wlr 标准协议）
//!
//! X11Generic 装配矩阵特点：外观/启动器为 None（无 DE 层服务）；
//! 通知走 freedesktop 公共路径（无 DE 专有通知实现）。

use agent_shell_a11y::AtSpiComponent;
use agent_shell_capture::CaptureDispatcher;
use agent_shell_clipboard::{SessionKind, WlClipboard};
use agent_shell_compositor_wlr_wayland::WlrWaylandCompositor;
use agent_shell_compositor_x11::X11Compositor;
use agent_shell_core::component::{
    A11yComponent, AppearanceComponent, AudioServerComponent, CaptureComponent, ClipboardComponent,
    CompositorComponent, InputComponent, LauncherComponent, NetworkComponent,
    NotificationComponent, PowerComponent,
};
use agent_shell_core::error::Result;
use agent_shell_core::registry::{BackendKind, ComponentRegistry};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_notification::FreedesktopNotification;
use agent_shell_power::UPowerComponent;

/// X11 通用兜底 backend（§3.3 X11Generic 行：无 DE，EWMH/XTest 原生）。
pub struct X11Backend;

impl Default for X11Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl X11Backend {
    pub fn new() -> Self {
        Self
    }

    pub async fn assemble(&self) -> Result<ComponentRegistry> {
        // 1. 合成器：X11Compositor（EWMH/XTest 原生）
        let compositor: Option<Box<dyn CompositorComponent>> = match X11Compositor::new() {
            Ok(c) => Some(Box::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "X11Compositor assembly failed; compositor slot None");
                None
            }
        };

        // 2. 电源：UPower（无 DE 层封装）
        let power: Option<Box<dyn PowerComponent>> = match UPowerComponent::new().await {
            Ok(u) => Some(Box::new(u)),
            Err(e) => {
                tracing::warn!(error = %e, "UPower failed; power slot None");
                None
            }
        };

        // 3. 通知：freedesktop 公共路径（无 DE 专有）
        let notification: Option<Box<dyn NotificationComponent>> =
            match FreedesktopNotification::new().await {
                Ok(n) => Some(Box::new(n)),
                Err(e) => {
                    tracing::warn!(error = %e, "freedesktop notification failed; slot None");
                    None
                }
            };

        // 外观 / 启动器：None（§3.3 矩阵：X11Generic 外观/启动器为 None）
        let appearance: Option<Box<dyn AppearanceComponent>> = None;
        let launcher: Option<Box<dyn LauncherComponent>> = None;

        // 4. 公共组件探测
        let audio = assemble_audio(BackendKind::X11Generic).await?;
        let network = assemble_network(BackendKind::X11Generic).await;
        let input = assemble_input(DesktopEnvironment::X11Generic).await;
        let capture = CaptureDispatcher::assemble().await;
        let a11y = AtSpiComponent::probe().await;
        // X11 会话 → xclip
        let clipboard: Option<Box<dyn ClipboardComponent>> =
            Some(Box::new(WlClipboard::with_session(SessionKind::X11)));
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

/// WlrWayland 兜底 backend（§3.3 WLRWayland 行：未知 wlroots 系合成器）。
pub struct WlrWaylandBackend;

impl Default for WlrWaylandBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WlrWaylandBackend {
    pub fn new() -> Self {
        Self
    }

    pub async fn assemble(&self) -> Result<ComponentRegistry> {
        // 1. 合成器：WlrWaylandCompositor（wlr 标准协议全实现）
        let compositor: Option<Box<dyn CompositorComponent>> = match WlrWaylandCompositor::connect()
        {
            Ok(c) => Some(Box::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "WlrWaylandCompositor assembly failed; compositor slot None");
                None
            }
        };

        // 2. 电源：UPower
        let power: Option<Box<dyn PowerComponent>> = match UPowerComponent::new().await {
            Ok(u) => Some(Box::new(u)),
            Err(e) => {
                tracing::warn!(error = %e, "UPower failed; power slot None");
                None
            }
        };

        // 3. portal 三件套（通知 / 外观 / 启动器）
        let notification: Option<Box<dyn NotificationComponent>> =
            match agent_shell_notification::PortalNotification::new().await {
                Ok(n) => Some(Box::new(n)),
                Err(e) => {
                    tracing::warn!(error = %e, "portal notification failed; slot None");
                    None
                }
            };
        let appearance: Option<Box<dyn AppearanceComponent>> =
            match agent_shell_appearance::PortalAppearance::new().await {
                Ok(a) => Some(Box::new(a)),
                Err(e) => {
                    tracing::warn!(error = %e, "portal appearance failed; slot None");
                    None
                }
            };
        let launcher: Option<Box<dyn LauncherComponent>> =
            Some(Box::new(agent_shell_launcher::DesktopFileLauncher::new()));

        // 4. 公共组件探测
        let audio = assemble_audio(BackendKind::WlrWayland).await?;
        let network = assemble_network(BackendKind::WlrWayland).await;
        let input = assemble_input(DesktopEnvironment::WLRWayland).await;
        let capture = CaptureDispatcher::assemble().await;
        let a11y = AtSpiComponent::probe().await;
        // Wayland → wl-clipboard
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

async fn assemble_audio(de: BackendKind) -> Result<Option<Box<dyn AudioServerComponent>>> {
    let router = agent_shell_audio::router::assemble_audio_router(de).await?;
    Ok(router.map(|r| Box::new(r) as Box<dyn AudioServerComponent>))
}

async fn assemble_network(de: BackendKind) -> Option<Box<dyn NetworkComponent>> {
    match agent_shell_network::detect(de).await {
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
