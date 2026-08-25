//! DDE 合成器形态检测：Treeland vs deepin-kwin vs X11（设计文档 §10.4）。
//!
//! 检测顺序（§10.4 / §16.1，不可调换）：
//!
//! 1. Wayland 会话（`WAYLAND_DISPLAY` 可连）→ 看 registry globals：
//!    `org_kde_plasma_window_management` 在 → **deepin-kwin**（KWin fork，
//!    复用 [`KWinCompositor`] 双通道）；`treeland_foreign_toplevel_manager_v1`
//!    或 `treeland_window_management_v1` 在 → **Treeland**（未来首选通道；
//!    当前 deepin 25 的 deepin-kwin 未启用）；都没有 → 无协议通道。
//! 2. X11 会话（`DISPLAY` 存在）→ X11 分支（EWMH + dde-api D-Bus）。
//!
//! DDE 下 `XDG_CURRENT_DESKTOP=Deepin` 但 KWin 服务名仍为 `org.kde.KWin`——
//! DE 判定层（core::de_detection）已保证「先判 DDE 再判 KDE」，本模块只在
//! 已确认 DDE 之后做**合成器形态**细分。

use agent_shell_core::error::AgentShellError;
use agent_shell_displayserver_wayland::WaylandDisplayServer;

/// deepin-kwin 公布的 KWin 私有窗口管理协议接口名。
pub const ORG_KDE_WINDOW_MANAGEMENT: &str = "org_kde_plasma_window_management";
/// Treeland 外部窗口管理协议接口名。
pub const TREELAND_FOREIGN_TOPLEVEL: &str = "treeland_foreign_toplevel_manager_v1";
/// Treeland 桌面状态协议接口名。
pub const TREELAND_WINDOW_MANAGEMENT: &str = "treeland_window_management_v1";

/// DDE 合成器形态（§10.4 复合装配的分支键）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompositorKind {
    /// Treeland（wlroots 系新一代，treeland_* 私有协议）。
    Treeland,
    /// deepin-kwin（KWin fork，org_kde_* 协议 + Scripting）。
    DeepinKwin,
    /// X11 会话（EWMH/ICCCM 基础通道）。
    X11,
    /// 无法确定（无显示服务器连接）。
    #[default]
    Unknown,
}

impl CompositorKind {
    /// doctor 展示名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Treeland => "Treeland (Wayland)",
            Self::DeepinKwin => "deepin-kwin (Wayland)",
            Self::X11 => "X11",
            Self::Unknown => "unknown",
        }
    }
}

/// 纯函数版形态判定：给定 registry 中已公布的接口集合与是否为 Wayland
/// 连接，返回合成器形态。注入式设计使检测逻辑可离线单测。
///
/// deepin-kwin 与 Treeland 同时公布时优先 Treeland（设计决策 D9：
/// 优先 Treeland Wayland 协议，其次 deepin-kwin）。
pub fn classify(wl_connected: bool, interfaces: &[&str]) -> CompositorKind {
    if !wl_connected {
        // 非 Wayland：有 X11 形态由调用方按 DISPLAY 判定，这里只认显式传入。
        return CompositorKind::Unknown;
    }
    if interfaces.contains(&TREELAND_FOREIGN_TOPLEVEL)
        || interfaces.contains(&TREELAND_WINDOW_MANAGEMENT)
    {
        return CompositorKind::Treeland;
    }
    if interfaces.contains(&ORG_KDE_WINDOW_MANAGEMENT) {
        return CompositorKind::DeepinKwin;
    }
    CompositorKind::Unknown
}

/// 连接真实显示环境并检测合成器形态。
///
/// 先试 Wayland（`$WAYLAND_DISPLAY`），成功后读 registry globals 分类；
/// Wayland 不可用且 `DISPLAY` 存在 → X11。两者皆缺 → Unknown（错误不抛，
/// 由装配方决定降级链——DdeCompositor 构造时才需要硬失败）。
pub async fn detect_compositor() -> CompositorKind {
    // 同步 wayland-client 调用包进阻塞安全上下文（连接含 roundtrip IO）。
    let wl = tokio::task::spawn_blocking(WaylandDisplayServer::connect);
    let wl = match wl.await {
        Ok(Ok(server)) => Some(server),
        // 连接失败 = 无 Wayland 会话，属正常探测路径而非错误。
        Ok(Err(AgentShellError::BackendUnavailable(_))) | Err(_) => None,
        Ok(Err(e)) => {
            tracing::debug!("dde compositor: wayland probe failed unexpectedly: {e}");
            None
        }
    };
    if let Some(server) = wl {
        let globals: Vec<String> = server
            .globals()
            .contents()
            .clone_list()
            .iter()
            .map(|g| g.interface.clone())
            .collect();
        let interfaces: Vec<&str> = globals.iter().map(String::as_str).collect();
        return classify(true, &interfaces);
    }
    if std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty()) {
        return CompositorKind::X11;
    }
    CompositorKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn treeland_wins_over_deepin_kwin() {
        // D9：优先 Treeland 协议通道。
        let kwin_only = vec![ORG_KDE_WINDOW_MANAGEMENT];
        assert_eq!(classify(true, &kwin_only), CompositorKind::DeepinKwin);

        let both = vec![ORG_KDE_WINDOW_MANAGEMENT, TREELAND_FOREIGN_TOPLEVEL];
        assert_eq!(classify(true, &both), CompositorKind::Treeland);

        let wm_variant = vec![TREELAND_WINDOW_MANAGEMENT];
        assert_eq!(classify(true, &wm_variant), CompositorKind::Treeland);
    }

    #[test]
    fn no_wayland_means_unknown_here() {
        assert_eq!(
            classify(false, &[ORG_KDE_WINDOW_MANAGEMENT]),
            CompositorKind::Unknown
        );
        assert_eq!(classify(true, &[]), CompositorKind::Unknown);
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(CompositorKind::DeepinKwin.label(), "deepin-kwin (Wayland)");
        assert_eq!(CompositorKind::Treeland.label(), "Treeland (Wayland)");
    }
}
