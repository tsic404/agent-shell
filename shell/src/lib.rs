//! agent-shell 顶层：[`AgentShell`] 结构与 [`detect_and_assemble`](AgentShell::detect_and_assemble)
//! 装配入口（设计文档 §4.1–4.3）。
//!
//! `AgentShell::detect_and_assemble()` 检测桌面环境 → 选定 backend → 调用
//! 对应 backend 的 `assemble()` 返回 [`ComponentRegistry`]。
//!
//! SSH 场景（无 `WAYLAND_DISPLAY` / `DISPLAY`）检测为 Tty，返回仅含系统服务的
//! 组件注册表（§23.2 SSH 场景仅系统服务可用）。

use agent_shell_core::de_detection::detect_desktop_environment;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::event::EventHub;
use agent_shell_core::registry::{BackendKind, ComponentRegistry};
use agent_shell_core::types::DesktopEnvironment;

/// 顶层 AgentShell 结构（§4.1）。
///
/// 持有事件枢纽、当前 backend 类型和公共组件装配结果。
pub struct AgentShell {
    /// 事件枢纽（所有组件事件归一化）。
    pub event_hub: EventHub,
    /// 当前 backend（桌面环境）。
    pub backend: BackendKind,
    /// 公共组件装配结果。
    pub components: ComponentRegistry,
}

impl AgentShell {
    /// 检测环境 → 选定 backend → 装配组件（§4.2 装配流程）。
    ///
    /// 流程：
    /// 1. 检测环境：`XDG_SESSION_TYPE` × `XDG_CURRENT_DESKTOP` → `DesktopEnvironment`
    /// 2. 选定 backend——决定公共组件实例的组合
    /// 3. 按 backend 装配清单初始化公共组件（由各 backend 的 `assemble()` 完成）
    /// 4. 返回 `AgentShell`
    pub async fn detect_and_assemble() -> Result<Self> {
        let de = detect_desktop_environment();
        let event_hub = EventHub::new();
        let components = assemble_for(de).await?;
        let backend = BackendKind::from(de);
        Ok(Self {
            event_hub,
            backend,
            components,
        })
    }
}

/// 按检测到的桌面环境分发到对应 backend 的 `assemble()`（§4.3 match 骨架）。
///
/// 未编译对应 backend feature 时返回 `UnsupportedDE`（D2 编译期 features）。
async fn assemble_for(de: DesktopEnvironment) -> Result<ComponentRegistry> {
    match de {
        #[cfg(feature = "kde")]
        DesktopEnvironment::KDE => {
            let backend = agent_shell_backend_kde::KdeBackend::new(
                agent_shell_backend_kde::SessionType::from_env(),
            );
            backend.assemble().await
        }
        #[cfg(not(feature = "kde"))]
        DesktopEnvironment::KDE => Err(unsupported("kde")),

        #[cfg(feature = "dde")]
        DesktopEnvironment::DDE => {
            let backend = agent_shell_backend_dde::DdeBackend::new(
                agent_shell_backend_dde::SessionType::from_env(),
            );
            backend.assemble().await
        }
        #[cfg(not(feature = "dde"))]
        DesktopEnvironment::DDE => Err(unsupported("dde")),

        #[cfg(feature = "gnome")]
        DesktopEnvironment::GNOME => {
            let backend = agent_shell_backend_gnome::GnomeBackend::new(
                agent_shell_backend_gnome::SessionType::from_env(),
            );
            backend.assemble().await
        }
        #[cfg(not(feature = "gnome"))]
        DesktopEnvironment::GNOME => Err(unsupported("gnome")),

        #[cfg(feature = "hyprland")]
        DesktopEnvironment::Hyprland => {
            let backend = agent_shell_backend_hyprland::HyprlandBackend::new();
            backend.assemble().await
        }
        #[cfg(not(feature = "hyprland"))]
        DesktopEnvironment::Hyprland => Err(unsupported("hyprland")),

        #[cfg(feature = "sway")]
        DesktopEnvironment::Sway => {
            let backend = agent_shell_backend_sway::SwayBackend::new();
            backend.assemble().await
        }
        #[cfg(not(feature = "sway"))]
        DesktopEnvironment::Sway => Err(unsupported("sway")),

        DesktopEnvironment::X11Generic => {
            let backend = agent_shell_backend_generic::X11Backend::new();
            backend.assemble().await
        }
        DesktopEnvironment::WLRWayland => {
            let backend = agent_shell_backend_generic::WlrWaylandBackend::new();
            backend.assemble().await
        }
        DesktopEnvironment::Tty => {
            let backend = agent_shell_backend_tty::TtyBackend::new();
            backend.assemble().await
        }
        _ => Err(AgentShellError::UnsupportedDE(format!("{de:?}"))),
    }
}

#[allow(dead_code)]
fn unsupported(feature: &str) -> AgentShellError {
    AgentShellError::UnsupportedDE(format!(
        "backend feature '{feature}' not compiled in; rebuild with --features {feature}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TTY 装配在无显示服务器时应返回仅含系统服务的注册表。
    /// 本测试在 CI 无 DBus/Systemd 环境下 init_system/session_manager 可能为 None，
    /// 但 assemble 本身不报错——验证降级语义而非依赖具体服务。
    #[tokio::test]
    async fn tty_assemble_does_not_panic() {
        // 不依赖具体环境——TTY backend 即使无 systemd/logind 也应返回 Ok。
        let reg = agent_shell_backend_tty::TtyBackend::new()
            .assemble()
            .await
            .expect("TTY assemble must not error");
        assert!(reg.compositor.is_none());
        assert!(reg.audio.is_none());
        assert!(reg.input.is_none());
        assert!(reg.capture.is_none());
        assert!(reg.a11y.is_none());
        assert!(reg.clipboard.is_none());
        assert!(reg.power.is_none());
        assert!(reg.notification.is_none());
        assert!(reg.appearance.is_none());
        assert!(reg.launcher.is_none());
    }
}
