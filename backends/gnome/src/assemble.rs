//! GNOME backend 装配实现（§4.3、§3.3 装配矩阵 GNOME 行）。

use agent_shell_a11y::AtSpiComponent;
use agent_shell_audio::{self};
use agent_shell_capture::CaptureDispatcher;
use agent_shell_clipboard::{SessionKind, WlClipboard};
use agent_shell_compositor_mutter::MutterCompositor;
use agent_shell_core::component::{
    A11yComponent, AppearanceComponent, AudioServerComponent, CaptureComponent, ClipboardComponent,
    CompositorComponent, InputComponent, LauncherComponent, NetworkComponent,
    NotificationComponent, PowerComponent,
};
use agent_shell_core::error::Result;
use agent_shell_core::registry::{BackendKind, ComponentRegistry};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_notification::FreedesktopNotification;

use crate::services::{GnomeAppearance, GnomeLauncher, GnomePower};

/// 会话类型（§4.2：GNOME 两会话均 MutterCompositor，但输入/剪贴板会话相关）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionType {
    Wayland,
    X11,
}

impl SessionType {
    /// 从环境变量探测会话类型。
    pub fn from_env() -> Self {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some()
        {
            SessionType::Wayland
        } else {
            SessionType::X11
        }
    }
}

/// GNOME backend 装配器（§4.3 `GnomeBackend`）。
pub struct GnomeBackend {
    session: SessionType,
}

impl GnomeBackend {
    pub fn new(session: SessionType) -> Self {
        Self { session }
    }

    /// 装配清单（§3.3 GNOME 行）：
    /// - 合成器：MutterCompositor（D-Bus Eval/Extension，两会话均用）
    /// - 音频：公共探测链（GNOME 无 DE 层音量 D-Bus，§3.5）
    /// - 网络：NetworkManager
    /// - 输入 / 截图 / 无障碍 / 剪贴板：公共探测链
    /// - 电源：GnomePower（gsd.Power），回退 UPower
    /// - 通知：freedesktop Notifications
    /// - 外观：GnomeAppearance（gsettings）
    /// - 启动器：GnomeLauncher（.desktop + gio）
    /// - 系统服务：Systemd + Logind
    pub async fn assemble(&self) -> Result<ComponentRegistry> {
        // 1. 合成器：MutterCompositor（Eval/Extension 双路径，会话无关）
        let compositor: Option<Box<dyn CompositorComponent>> = match MutterCompositor::new().await {
            Ok(c) => Some(Box::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "Mutter assembly failed; compositor slot None");
                None
            }
        };

        // 2. DE 专有组件优先，失败回退公共组件
        let power = match GnomePower::try_new().await {
            Ok(Some(p)) => Some(Box::new(p) as Box<dyn PowerComponent>),
            Ok(None) => match agent_shell_power::UPowerComponent::new().await {
                Ok(u) => Some(Box::new(u) as Box<dyn PowerComponent>),
                Err(e) => {
                    tracing::warn!(error = %e, "UPower fallback failed; power slot None");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "GnomePower probe error; trying UPower fallback");
                match agent_shell_power::UPowerComponent::new().await {
                    Ok(u) => Some(Box::new(u) as Box<dyn PowerComponent>),
                    Err(e2) => {
                        tracing::warn!(error = %e2, "UPower fallback failed; power slot None");
                        None
                    }
                }
            }
        };

        let notification: Option<Box<dyn NotificationComponent>> =
            match FreedesktopNotification::new().await {
                Ok(n) => Some(Box::new(n)),
                Err(e) => {
                    tracing::warn!(error = %e, "freedesktop notification failed; slot None");
                    None
                }
            };

        let appearance: Option<Box<dyn AppearanceComponent>> = Some(Box::new(GnomeAppearance));
        let launcher: Option<Box<dyn LauncherComponent>> = Some(Box::new(GnomeLauncher::default()));

        // 3. 公共组件探测
        let audio = assemble_audio().await?;
        let network = assemble_network().await;
        let input = assemble_input(DesktopEnvironment::GNOME).await;
        let capture = CaptureDispatcher::assemble().await;
        let a11y = AtSpiComponent::probe().await;
        let clipboard = assemble_clipboard(self.session);
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
    let router = agent_shell_audio::router::assemble_audio_router(BackendKind::Gnome).await?;
    Ok(router.map(|r| Box::new(r) as Box<dyn AudioServerComponent>))
}

async fn assemble_network() -> Option<Box<dyn NetworkComponent>> {
    match agent_shell_network::detect(BackendKind::Gnome).await {
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

fn assemble_clipboard(session: SessionType) -> Option<Box<dyn ClipboardComponent>> {
    let kind = match session {
        SessionType::Wayland => SessionKind::Wayland,
        SessionType::X11 => SessionKind::X11,
    };
    Some(Box::new(WlClipboard::with_session(kind)))
}
