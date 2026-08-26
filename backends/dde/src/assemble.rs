//! DDE backend 装配实现（§4.3、§3.3 装配矩阵 DDE 行、§21.6）。

use agent_shell_a11y::AtSpiComponent;
use agent_shell_appearance::PortalAppearance;
use agent_shell_audio::{self};
use agent_shell_capture::CaptureDispatcher;
use agent_shell_clipboard::{SessionKind, WlClipboard};
use agent_shell_core::component::{
    A11yComponent, AppearanceComponent, AudioServerComponent, CaptureComponent, ClipboardComponent,
    CompositorComponent, InputComponent, LauncherComponent, NetworkComponent,
    NotificationComponent, PowerComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::registry::{BackendKind, ComponentRegistry};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_launcher::DesktopFileLauncher;
use agent_shell_notification::FreedesktopNotification;

use crate::dde_api::{DdeAppearance, DdeLauncher, DdeNotification, DdePower};
use crate::dde_audio::{self, DdeAudio};
use crate::DdeCompositor;

/// 会话类型（§4.2：Wayland / X11）。
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

/// DDE backend 装配器（§4.3 `DdeBackend`）。
pub struct DdeBackend {
    session: SessionType,
}

impl DdeBackend {
    /// 构造（§4.3 `DdeBackend::new(dbus, session_type)`）。
    pub fn new(session: SessionType) -> Self {
        Self { session }
    }

    /// 按装配矩阵初始化公共组件实例并返回 [`ComponentRegistry`]（§4.3）。
    ///
    /// 装配清单（§3.3 DDE 行）：
    /// - 合成器：DdeCompositor（deepin-kwin 复用 KWin / Treeland / X11 复合装配）
    /// - 音频：DdeAudio（org.deepin.dde.Audio1）优先，回退 PipeWire/PulseAudio
    /// - 网络：org.deepin.dde.Network（未来）/ NetworkManager
    /// - 输入 / 截图 / 无障碍 / 剪贴板：公共探测链
    /// - 电源：DdePower（Power1 双名），回退 UPower
    /// - 通知：DdeNotification（双名），回退 freedesktop Notifications
    /// - 外观：DdeAppearance（双名），回退 PortalAppearance
    /// - 启动器：DdeLauncher（dde-am），回退 DesktopFileLauncher
    /// - 系统服务：Systemd + Logind
    pub async fn assemble(&self) -> Result<ComponentRegistry> {
        // 1. 合成器：DdeCompositor 复合装配（Wayland/Treeland/X11 自动检测）
        let compositor: Option<Box<dyn CompositorComponent>> = match DdeCompositor::connect().await
        {
            Ok(c) => Some(Box::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "DdeCompositor assembly failed; compositor slot None");
                None
            }
        };

        // 2. DE 专有组件优先（org.deepin.dde.* 双名），失败回退公共组件
        let power = assemble_power().await;
        let notification = assemble_notification().await;
        let appearance = assemble_appearance().await;
        let launcher = assemble_launcher().await;

        // 3. 音频：DdeAudio 优先，回退公共探测链
        let audio = assemble_audio().await?;

        // 4. 公共组件探测（跨 DE 一致）
        let network = assemble_network().await;
        let input = assemble_input(DesktopEnvironment::DDE).await;
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

async fn assemble_power() -> Option<Box<dyn PowerComponent>> {
    match DdePower::try_new().await {
        Ok(p) => Some(Box::new(p)),
        Err(e) => {
            tracing::warn!(error = %e, "DdePower failed; trying UPower fallback");
            match agent_shell_power::UPowerComponent::new().await {
                Ok(u) => Some(Box::new(u)),
                Err(e2) => {
                    tracing::warn!(error = %e2, "UPower fallback failed; power slot None");
                    None
                }
            }
        }
    }
}

async fn assemble_notification() -> Option<Box<dyn NotificationComponent>> {
    match DdeNotification::try_new().await {
        Ok(Some(n)) => Some(Box::new(n)),
        Ok(None) => {
            tracing::info!("DDE notification service absent; falling back to freedesktop");
            match FreedesktopNotification::new().await {
                Ok(n) => Some(Box::new(n)),
                Err(e) => {
                    tracing::warn!(error = %e, "freedesktop notification fallback failed");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "DdeNotification probe error; trying freedesktop");
            match FreedesktopNotification::new().await {
                Ok(n) => Some(Box::new(n)),
                Err(_) => None,
            }
        }
    }
}

async fn assemble_appearance() -> Option<Box<dyn AppearanceComponent>> {
    match DdeAppearance::try_new().await {
        Ok(Some(a)) => Some(Box::new(a)),
        Ok(None) => {
            tracing::info!("DDE appearance service absent; falling back to portal");
            match PortalAppearance::new().await {
                Ok(p) => Some(Box::new(p)),
                Err(e) => {
                    tracing::warn!(error = %e, "portal appearance fallback failed");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "DdeAppearance probe error; trying portal");
            match PortalAppearance::new().await {
                Ok(p) => Some(Box::new(p)),
                Err(_) => None,
            }
        }
    }
}

async fn assemble_launcher() -> Option<Box<dyn LauncherComponent>> {
    match DdeLauncher::try_new().await {
        Ok(l) => Some(Box::new(l)),
        Err(e) => {
            tracing::warn!(error = %e, "DdeLauncher failed; using desktop-file launcher");
            Some(Box::new(DesktopFileLauncher::new()))
        }
    }
}

async fn assemble_audio() -> Result<Option<Box<dyn AudioServerComponent>>> {
    // DdeAudio 优先（org.deepin.dde.Audio1 / com.deepin.daemon.Audio）
    if let Ok(Some(dde_audio)) = try_dde_audio().await {
        return Ok(Some(Box::new(dde_audio)));
    }
    // 回退公共探测链（wpctl → pactl → None）
    let router = agent_shell_audio::router::assemble_audio_router(BackendKind::Dde).await?;
    Ok(router.map(|r| Box::new(r) as Box<dyn AudioServerComponent>))
}

/// 探测 DdeAudio：服务在位且属性可读才返回实例，否则 None。
async fn try_dde_audio() -> Result<Option<DdeAudio>> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| AgentShellError::DBus(format!("session bus connect: {e}")))?;
    // 先探两个服务名是否在 bus 上
    use agent_shell_power::probe_first_existing;
    let names = [dde_audio::DDE25_AUDIO, dde_audio::DDE20_AUDIO];
    let existing = probe_first_existing(&conn, &names).await?;
    if existing.is_none() {
        return Ok(None);
    }
    // 服务在位 → 尝试构造（属性可读性由 DdeAudio::connect 内部小步探测）
    match DdeAudio::connect().await {
        Ok(d) => Ok(Some(d)),
        Err(e) => {
            tracing::info!(error = %e, "DdeAudio connect failed; falling back to public audio");
            Ok(None)
        }
    }
}

async fn assemble_network() -> Option<Box<dyn NetworkComponent>> {
    match agent_shell_network::detect(BackendKind::Dde).await {
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
