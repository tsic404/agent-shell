//! 公共组件注册表（装配结果）。
//!
//! 对应设计文档 §3.5：`AgentShell` 持有公共组件装配结果 + 当前 backend 类型。
//! 所有组件均为可选——TTY 模式无合成器/音频/输入/截图等 DE 组件，仅
//! `init_system` 和 `session_manager` 等系统服务可用。

use crate::component::{
    A11yComponent, AppearanceComponent, AudioServerComponent, CaptureComponent, ClipboardComponent,
    ComponentHealth, ComponentType, CompositorComponent, DesktopComponent, DisplayLayoutComponent,
    InputComponent, LauncherComponent, NetworkComponent, NotificationComponent, PowerComponent,
    SessionManagerComponent, SystemComponent,
};
use crate::types::DesktopEnvironment;

/// Backend 类型（当前处于哪个桌面环境）。
///
/// 对应设计文档 §3.3 装配矩阵的行键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// KDE Plasma（KWin）
    Kde,
    /// Deepin DDE（deepin-kwin / Treeland）
    Dde,
    /// GNOME（Mutter）
    Gnome,
    /// Hyprland（wlroots 系）
    Hyprland,
    /// Sway（i3 兼容 wlroots）
    Sway,
    /// X11 通用兜底（无法识别 DE）
    X11Generic,
    /// 未知 Wayland 合成器兜底（wlr 标准协议）
    WlrWayland,
    /// TTY 纯终端（仅系统服务组件族可用）
    Tty,
}

impl From<DesktopEnvironment> for BackendKind {
    fn from(de: DesktopEnvironment) -> Self {
        match de {
            DesktopEnvironment::KDE => Self::Kde,
            DesktopEnvironment::DDE => Self::Dde,
            DesktopEnvironment::GNOME => Self::Gnome,
            DesktopEnvironment::Hyprland => Self::Hyprland,
            DesktopEnvironment::Sway => Self::Sway,
            DesktopEnvironment::X11Generic => Self::X11Generic,
            DesktopEnvironment::WLRWayland => Self::WlrWayland,
            DesktopEnvironment::Tty => Self::Tty,
            // 已识别但未实现专用装配器的 DE 统一走通用兜底（保守按 X11 处理）。
            // 会话类型感知的兜底请用 `with_session_fallback`——de_detection 任务
            // （TSI-2322）落地后由 detect_backend 直接构造，不再经过本 trait。
            _ => Self::X11Generic,
        }
    }
}

impl BackendKind {
    /// 未实现专用装配器的 DE 的会话类型感知兜底。
    ///
    /// Wayland 会话 → `WlrWayland`（wlr 标准协议）；其余 → `X11Generic`（EWMH）。
    pub fn with_session_fallback(de: DesktopEnvironment, wayland_session: bool) -> Self {
        match de {
            DesktopEnvironment::KDE => Self::Kde,
            DesktopEnvironment::DDE => Self::Dde,
            DesktopEnvironment::GNOME => Self::Gnome,
            DesktopEnvironment::Hyprland => Self::Hyprland,
            DesktopEnvironment::Sway => Self::Sway,
            DesktopEnvironment::WLRWayland
            | DesktopEnvironment::Budgie
            | DesktopEnvironment::XFCE
                if wayland_session =>
            {
                Self::WlrWayland
            }
            DesktopEnvironment::Tty => Self::Tty,
            _ => Self::X11Generic,
        }
    }
}

/// AgentShell 公共组件装配结果。
///
/// 每个 slot 装一个具体实例（DE 封装优先路由的产物）；`None` 表示该能力
/// 当前环境不可用（如 TTY 无合成器）。
pub struct ComponentRegistry {
    /// 合成器（有 DE 时才存在，TTY 模式为 None）
    pub compositor: Option<Box<dyn CompositorComponent>>,
    /// 音频服务器（有音频服务器时才存在）
    pub audio: Option<Box<dyn AudioServerComponent>>,
    /// 网络
    pub network: Option<Box<dyn NetworkComponent>>,
    /// 输入
    pub input: Option<Box<dyn InputComponent>>,
    /// 截图
    pub capture: Option<Box<dyn CaptureComponent>>,
    /// 无障碍
    pub a11y: Option<Box<dyn A11yComponent>>,
    /// 剪贴板
    pub clipboard: Option<Box<dyn ClipboardComponent>>,
    /// 电源管理
    pub power: Option<Box<dyn PowerComponent>>,
    /// 通知
    pub notification: Option<Box<dyn NotificationComponent>>,
    /// 外观
    pub appearance: Option<Box<dyn AppearanceComponent>>,
    /// 启动器
    pub launcher: Option<Box<dyn LauncherComponent>>,
    /// 屏幕布局
    pub display_layout: Option<Box<dyn DisplayLayoutComponent>>,
    /// init 系统（systemd，任何 Linux 环境都有）
    pub init_system: Option<Box<dyn SystemComponent>>,
    /// 会话管理器（logind/elogind）
    pub session_manager: Option<Box<dyn SessionManagerComponent>>,
}

impl ComponentRegistry {
    /// 创建全空的注册表（TTY 兜底：仅后续按探测填充系统服务族）。
    pub fn empty() -> Self {
        Self {
            compositor: None,
            audio: None,
            network: None,
            input: None,
            capture: None,
            a11y: None,
            clipboard: None,
            power: None,
            notification: None,
            appearance: None,
            launcher: None,
            display_layout: None,
            init_system: None,
            session_manager: None,
        }
    }

    /// 按组件类型查询基础接口视图（doctor / 健康检查用）。
    ///
    /// 返回 `None` 表示该类型当前环境不可用。
    pub fn component(&self, ct: ComponentType) -> Option<&dyn DesktopComponent> {
        macro_rules! upcast {
            ($slot:expr) => {
                $slot.as_deref().map(|c| c as &dyn DesktopComponent)
            };
        }
        match ct {
            ComponentType::Compositor => upcast!(self.compositor),
            ComponentType::AudioServer => upcast!(self.audio),
            ComponentType::Network => upcast!(self.network),
            ComponentType::Input => upcast!(self.input),
            ComponentType::Capture => upcast!(self.capture),
            ComponentType::A11y => upcast!(self.a11y),
            ComponentType::Clipboard => upcast!(self.clipboard),
            ComponentType::Power => upcast!(self.power),
            ComponentType::Notification => upcast!(self.notification),
            ComponentType::Appearance => upcast!(self.appearance),
            ComponentType::Launcher => upcast!(self.launcher),
            ComponentType::InitSystem => upcast!(self.init_system),
            ComponentType::SessionManager => upcast!(self.session_manager),
        }
    }

    /// 全部已装配组件的健康检查（doctor 命令调用）。
    pub async fn doctor(&self) -> Vec<(&'static str, ComponentHealth)> {
        let mut results = Vec::new();
        for ct in [
            ComponentType::Compositor,
            ComponentType::AudioServer,
            ComponentType::Network,
            ComponentType::Input,
            ComponentType::Capture,
            ComponentType::A11y,
            ComponentType::Clipboard,
            ComponentType::Power,
            ComponentType::Notification,
            ComponentType::Appearance,
            ComponentType::Launcher,
            ComponentType::InitSystem,
            ComponentType::SessionManager,
        ] {
            if let Some(c) = self.component(ct) {
                results.push((c.name(), c.health().await));
            }
        }
        results
    }

    /// 已装配组件数量。
    pub fn len(&self) -> usize {
        [
            self.compositor.is_some(),
            self.audio.is_some(),
            self.network.is_some(),
            self.input.is_some(),
            self.capture.is_some(),
            self.a11y.is_some(),
            self.clipboard.is_some(),
            self.power.is_some(),
            self.notification.is_some(),
            self.appearance.is_some(),
            self.launcher.is_some(),
            self.display_layout.is_some(),
            self.init_system.is_some(),
            self.session_manager.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count()
    }

    /// 注册表是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for ComponentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names = Vec::new();
        for ct in ComponentType::all() {
            if let Some(c) = self.component(ct) {
                names.push(c.name());
            }
        }
        f.debug_struct("ComponentRegistry")
            .field("components", &names)
            .finish()
    }
}

impl ComponentType {
    /// 全部组件类别（doctor 遍历用）。
    pub const fn all() -> [ComponentType; 13] {
        [
            Self::Compositor,
            Self::AudioServer,
            Self::Network,
            Self::Input,
            Self::Capture,
            Self::A11y,
            Self::Clipboard,
            Self::Power,
            Self::Notification,
            Self::Appearance,
            Self::Launcher,
            Self::InitSystem,
            Self::SessionManager,
        ]
    }
}

// ───────────────────────── 测试 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{BackendCapabilities, ComponentHealth};
    use crate::error::Result;
    use crate::event::EventStream;
    use crate::types::{Rect, WindowId, WindowInfo, WorkspaceId, WorkspaceInfo};

    struct Stub;

    #[async_trait::async_trait]
    impl DesktopComponent for Stub {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn component_type(&self) -> ComponentType {
            ComponentType::Compositor
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn health(&self) -> ComponentHealth {
            ComponentHealth::Healthy
        }
    }

    #[async_trait::async_trait]
    impl CompositorComponent for Stub {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::default()
        }
        async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Ok(vec![])
        }
        async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
            Ok(None)
        }
        async fn focus_window(&self, _id: &WindowId) -> Result<()> {
            unimplemented!()
        }
        async fn move_window(&self, _id: &WindowId, _x: i32, _y: i32) -> Result<()> {
            unimplemented!()
        }
        async fn resize_window(&self, _id: &WindowId, _w: i32, _h: i32) -> Result<()> {
            unimplemented!()
        }
        async fn minimize_window(&self, _id: &WindowId) -> Result<()> {
            unimplemented!()
        }
        async fn unminimize_window(&self, _id: &WindowId) -> Result<()> {
            unimplemented!()
        }
        async fn maximize_window(&self, _id: &WindowId) -> Result<()> {
            unimplemented!()
        }
        async fn close_window(&self, _id: &WindowId) -> Result<()> {
            unimplemented!()
        }
        async fn set_window_geometry(&self, _id: &WindowId, _geo: Rect) -> Result<()> {
            unimplemented!()
        }
        async fn get_window_info(&self, _id: &WindowId) -> Result<WindowInfo> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
            Ok(vec![])
        }
        async fn activate_workspace(&self, _id: &WorkspaceId) -> Result<()> {
            unimplemented!()
        }
        async fn move_window_to_workspace(&self, _wid: &WindowId, _ws: &WorkspaceId) -> Result<()> {
            unimplemented!()
        }
        async fn list_monitors(&self) -> Result<Vec<crate::types::MonitorInfo>> {
            Ok(vec![])
        }
        async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
            unimplemented!()
        }
    }

    #[test]
    fn empty_registry_has_no_components() {
        let r = ComponentRegistry::empty();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.component(ComponentType::Compositor).is_none());
        assert!(r.component(ComponentType::InitSystem).is_none());
    }

    #[tokio::test]
    async fn registry_exposes_dyn_views_and_doctor() {
        let mut r = ComponentRegistry::empty();
        r.compositor = Some(Box::new(Stub));
        assert_eq!(r.len(), 1);
        let view = r.component(ComponentType::Compositor).unwrap();
        assert_eq!(view.name(), "stub");
        let report = r.doctor().await;
        assert_eq!(report.len(), 1);
        assert_eq!(report[0], ("stub", ComponentHealth::Healthy));
    }

    #[test]
    fn backend_kind_from_de() {
        assert_eq!(BackendKind::from(DesktopEnvironment::KDE), BackendKind::Kde);
        assert_eq!(BackendKind::from(DesktopEnvironment::Tty), BackendKind::Tty);
        // 未实现专用装配器的 DE 落到通用兜底
        assert_eq!(
            BackendKind::from(DesktopEnvironment::XFCE),
            BackendKind::X11Generic
        );
    }

    #[test]
    fn session_aware_fallback_respects_session_type() {
        // Wayland 会话下的未适配 DE → wlr 兜底
        assert_eq!(
            BackendKind::with_session_fallback(DesktopEnvironment::XFCE, true),
            BackendKind::WlrWayland
        );
        // X11 会话 → X11Generic
        assert_eq!(
            BackendKind::with_session_fallback(DesktopEnvironment::XFCE, false),
            BackendKind::X11Generic
        );
        // 已有专用装配器的 DE 不受会话类型影响
        assert_eq!(
            BackendKind::with_session_fallback(DesktopEnvironment::KDE, true),
            BackendKind::Kde
        );
    }
}
