//! KDE backend 装配实现（§4.3 装配代码、§3.3 装配矩阵 KDE 行）。

use agent_shell_a11y::AtSpiComponent;
use agent_shell_capture::CaptureDispatcher;
use agent_shell_clipboard::{SessionKind, WlClipboard};
use agent_shell_compositor_kwin::KWinCompositor;
use agent_shell_core::component::{
    A11yComponent, AppearanceComponent, AudioServerComponent, CaptureComponent, ClipboardComponent,
    CompositorComponent, InputComponent, LauncherComponent, NetworkComponent,
    NotificationComponent, PowerComponent,
};
use agent_shell_core::error::Result;
use agent_shell_core::registry::{BackendKind, ComponentRegistry};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_notification::FreedesktopNotification;

use crate::services::{KdeAppearance, KdeLauncher, KdePowerDevil};

/// 会话类型（§4.2：`XDG_SESSION_TYPE` 决定合成器装配形态）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionType {
    /// Wayland 会话：KWin org_kde_* 私有协议基础通道。
    Wayland,
    /// X11 会话：KWin EWMH + Scripting D-Bus 通道。
    X11,
}

impl SessionType {
    /// 从环境变量探测会话类型（§16.1 判定顺序）。
    ///
    /// `WAYLAND_DISPLAY` 存在 → Wayland；`DISPLAY` 存在 → X11；两者皆无 →
    /// 调用方应已判定为 Tty，不应进入 DE backend。
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

/// KDE backend 装配器（§4.3 `KdeBackend`）。
pub struct KdeBackend {
    session: SessionType,
}

impl KdeBackend {
    /// 构造（§4.3 `KdeBackend::new(dbus, session_type)`）。
    pub fn new(session: SessionType) -> Self {
        Self { session }
    }

    /// 按装配矩阵初始化公共组件实例并返回 [`ComponentRegistry`]（§4.3）。
    ///
    /// 装配清单（§3.3 KDE 行）：
    /// - 合成器：KWin（Wayland org_kde_* / X11 EWMH+Scripting）
    /// - 音频：PipeWire / PulseAudio 探测
    /// - 网络：NetworkManager
    /// - 输入 / 截图 / 无障碍 / 剪贴板：公共探测链
    /// - 电源：KdePowerDevil 优先，回退 UPower
    /// - 通知：freedesktop Notifications（KDE 即其实现）
    /// - 外观：KdeAppearance（plasma-apply-* CLI）
    /// - 启动器：KdeLauncher（.desktop + gio）
    /// - 系统服务：Systemd + Logind
    pub async fn assemble(&self) -> Result<ComponentRegistry> {
        // 1. 合成器：KWin（Wayland 优先，回退 X11）
        let compositor: Option<Box<dyn CompositorComponent>> = match self.session {
            SessionType::Wayland => match KWinCompositor::new_wayland().await {
                Ok(c) => Some(Box::new(c)),
                Err(e) => {
                    tracing::warn!(error = %e, "KWin Wayland assembly failed; compositor slot None");
                    None
                }
            },
            SessionType::X11 => match KWinCompositor::new_x11().await {
                Ok(c) => Some(Box::new(c)),
                Err(e) => {
                    tracing::warn!(error = %e, "KWin X11 assembly failed; compositor slot None");
                    None
                }
            },
        };

        // 2. DE 专有组件优先（org.kde.* 接口），失败回退公共组件
        let power: Option<Box<dyn PowerComponent>> = match KdePowerDevil::try_new().await? {
            Some(p) => Some(Box::new(p)),
            None => match agent_shell_power::UPowerComponent::new().await {
                Ok(u) => Some(Box::new(u)),
                Err(e) => {
                    tracing::warn!(error = %e, "UPower fallback failed; power slot None");
                    None
                }
            },
        };

        let notification: Option<Box<dyn NotificationComponent>> =
            match FreedesktopNotification::new().await {
                Ok(n) => Some(Box::new(n)),
                Err(e) => {
                    tracing::warn!(error = %e, "freedesktop notification failed; slot None");
                    None
                }
            };

        let appearance: Option<Box<dyn AppearanceComponent>> = Some(Box::new(KdeAppearance));
        let launcher: Option<Box<dyn LauncherComponent>> = Some(Box::new(KdeLauncher::default()));

        // 3. 公共组件探测（跨 DE 一致）
        let audio = assemble_audio().await?;
        let network = assemble_network().await;
        let input = assemble_input(DesktopEnvironment::KDE).await;
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
    let router = agent_shell_audio::router::assemble_audio_router(BackendKind::Kde).await?;
    Ok(router.map(|r| Box::new(r) as Box<dyn AudioServerComponent>))
}

async fn assemble_network() -> Option<Box<dyn NetworkComponent>> {
    match agent_shell_network::detect(BackendKind::Kde).await {
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
