//! 组件 trait 契约。
//!
//! 对应设计文档 §3.4（Component trait）与 §21.4（系统服务组件接口）：
//! 所有公共组件实现统一的 `DesktopComponent` 基础接口；合成器实现
//! `CompositorComponent`（17 方法）；各能力组件实现各自的扩展 trait。
//! 具体实现位于 `components/` 各 crate，本 crate 仅定义契约与注册表。

use async_trait::async_trait;

#[cfg(test)]
use crate::error::AgentShellError;
use crate::error::Result;
use crate::event::EventStream;
use crate::services::{
    AppInfo, AppTarget, AudioDevice, AudioState, BatteryState, ColorScheme, MonitorLayout,
    NetworkState, NotificationSpec, SystemdUnit, WifiNetwork,
};
use crate::types::{
    MonitorId, Rect, SessionInfo, UnitStatus, WindowId, WindowInfo, WorkspaceId, WorkspaceInfo,
};

// ───────────────────────── 组件类型与健康状态 ─────────────────────────

/// 组件类型（公共组件抽象类别）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ComponentType {
    /// 合成器（窗口/工作区/监视器管理）
    Compositor,
    /// 音频服务器
    AudioServer,
    /// 网络
    Network,
    /// 输入
    Input,
    /// 截图
    Capture,
    /// 无障碍
    A11y,
    /// 剪贴板
    Clipboard,
    /// 电源
    Power,
    /// 通知
    Notification,
    /// 外观
    Appearance,
    /// 启动器
    Launcher,
    /// init 系统（systemd）
    InitSystem,
    /// 会话管理器（logind）
    SessionManager,
}

/// 组件健康状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentHealth {
    /// 正常运行。
    Healthy,
    /// 降级运行（如：协议部分绑定失败，回退 Scripting）。
    Degraded(String),
    /// 组件不可用（如：Wayland 下无 X11）。
    Unavailable,
}

/// 合成器能力掩码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// 窗口管理
    pub window_management: bool,
    /// 工作区管理
    pub workspace_management: bool,
    /// 监视器布局
    pub monitor_layout: bool,
    /// 窗口事件流
    pub window_events: bool,
    /// 工作区事件流
    pub workspace_events: bool,
    /// 原生输入注入
    pub native_input: bool,
    /// 原生截图
    pub native_capture: bool,
    /// 虚拟桌面
    pub virtual_desktops: bool,
    /// 特效控制
    pub effects_control: bool,
}

// ───────────────────────── 基础接口 ─────────────────────────

/// 公共组件统一基础接口——所有组件 trait 的 supertrait。
#[async_trait]
pub trait DesktopComponent: Send + Sync {
    /// 组件名（用于日志与 doctor 报告）。
    fn name(&self) -> &'static str;

    /// 组件类型。
    fn component_type(&self) -> ComponentType;

    /// 同步可用性（构造时探测结果）。
    fn is_available(&self) -> bool;

    /// 异步健康检查（doctor 命令调用）。
    async fn health(&self) -> ComponentHealth;
}

// ───────────────────────── 合成器 ─────────────────────────

/// 合成器组件统一接口（17 方法）。
///
/// KWin/Mutter/Hyprland/Sway/X11/WlrWayland 等所有合成器实现此 trait；
/// Wayland 系合成器经由 `WaylandCompositor` 中间抽象叠加私有协议。
#[async_trait]
pub trait CompositorComponent: DesktopComponent {
    /// 能力掩码。
    fn capabilities(&self) -> BackendCapabilities;

    /// 列出全部窗口。
    async fn list_windows(&self) -> Result<Vec<WindowInfo>>;

    /// 获取当前活动窗口（可能为空，如桌面无焦点窗口时）。
    async fn get_active_window(&self) -> Result<Option<WindowInfo>>;

    /// 聚焦窗口。
    async fn focus_window(&self, id: &WindowId) -> Result<()>;

    /// 移动窗口到指定坐标。
    async fn move_window(&self, id: &WindowId, x: i32, y: i32) -> Result<()>;

    /// 缩放窗口到指定尺寸。
    async fn resize_window(&self, id: &WindowId, w: i32, h: i32) -> Result<()>;

    /// 最小化窗口。
    async fn minimize_window(&self, id: &WindowId) -> Result<()>;

    /// 还原（取消最小化）窗口。
    async fn unminimize_window(&self, id: &WindowId) -> Result<()>;

    /// 最大化窗口。
    async fn maximize_window(&self, id: &WindowId) -> Result<()>;

    /// 关闭窗口。
    async fn close_window(&self, id: &WindowId) -> Result<()>;

    /// 设置窗口几何（x/y/w/h 一次设定）。
    async fn set_window_geometry(&self, id: &WindowId, geo: Rect) -> Result<()>;

    /// 查询单个窗口信息。
    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo>;

    /// 列出工作区。
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>>;

    /// 激活工作区。
    async fn activate_workspace(&self, id: &WorkspaceId) -> Result<()>;

    /// 将窗口移动到工作区。
    async fn move_window_to_workspace(&self, wid: &WindowId, ws: &WorkspaceId) -> Result<()>;

    /// 列出监视器。
    async fn list_monitors(&self) -> Result<Vec<crate::types::MonitorInfo>>;

    /// 订阅合成器事件流。
    async fn subscribe(&self) -> Result<Box<dyn EventStream>>;
}

// ───────────────────────── 系统服务（TTY 必选组件族） ─────────────────────────

/// 系统服务组件接口（init 系统）。
///
/// 所有 Linux 环境（含 TTY）均有，是唯一"必选"组件族。
#[async_trait]
pub trait SystemComponent: DesktopComponent {
    /// 重载 systemd 配置。
    async fn daemon_reload(&self) -> Result<()>;

    /// 列出已加载单元。
    async fn list_units(&self) -> Result<Vec<SystemdUnit>>;

    /// 启动单元。
    async fn start_unit(&self, name: &str) -> Result<()>;

    /// 停止单元。
    async fn stop_unit(&self, name: &str) -> Result<()>;

    /// 启用单元（开机自启）。
    async fn enable_unit(&self, name: &str) -> Result<()>;

    /// 禁用单元。
    async fn disable_unit(&self, name: &str) -> Result<()>;

    /// 查询单元状态。
    async fn unit_status(&self, name: &str) -> Result<UnitStatus>;
}

/// 会话管理器组件接口（logind/elogind）。
#[async_trait]
pub trait SessionManagerComponent: DesktopComponent {
    /// 列出登录会话。
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>>;

    /// 查询单个会话。
    async fn get_session(&self, id: &str) -> Result<SessionInfo>;

    /// 锁定会话。
    async fn lock_session(&self, id: &str) -> Result<()>;

    /// 解锁会话。
    async fn unlock_session(&self, id: &str) -> Result<()>;

    /// 是否允许重启。
    async fn can_reboot(&self) -> Result<bool>;

    /// 重启系统。
    async fn reboot(&self) -> Result<()>;

    /// 是否允许关机。
    async fn can_poweroff(&self) -> Result<bool>;

    /// 关机。
    async fn poweroff(&self) -> Result<()>;
}

// ───────────────────────── 能力组件接口（§21.4） ─────────────────────────

/// 屏幕布局组件接口（components/compositor/x11 + DE 封装优先）。
#[async_trait]
pub trait DisplayLayoutComponent: DesktopComponent {
    /// 获取当前监视器布局。
    async fn get_monitor_layout(&self) -> Result<MonitorLayout>;

    /// 应用监视器布局。
    async fn apply_monitor_layout(&self, layout: &MonitorLayout) -> Result<()>;

    /// 设置监视器 DPMS 电源。
    async fn set_dpms(&self, monitor_id: &MonitorId, on: bool) -> Result<()>;
}

/// 音频组件接口（components/audio）。
#[async_trait]
pub trait AudioServerComponent: DesktopComponent {
    /// 获取主音量状态。
    async fn get_volume(&self) -> Result<AudioState>;

    /// 设置主音量（0.0 - 1.0）。
    async fn set_volume(&self, volume: f64) -> Result<()>;

    /// 设置静音。
    async fn set_mute(&self, muted: bool) -> Result<()>;

    /// 列出音频设备。
    async fn list_audio_devices(&self) -> Result<Vec<AudioDevice>>;

    /// 设置默认输出设备。
    async fn set_default_sink(&self, sink_name: &str) -> Result<()>;
}

/// 网络组件接口（components/network）。
#[async_trait]
pub trait NetworkComponent: DesktopComponent {
    /// 获取网络状态。
    async fn get_network_state(&self) -> Result<NetworkState>;

    /// 扫描并列出 WiFi 网络。
    async fn list_wifi_networks(&self) -> Result<Vec<WifiNetwork>>;

    /// 连接 WiFi。
    async fn connect_wifi(&self, ssid: &str, password: Option<&str>) -> Result<()>;

    /// 断开当前 WiFi。
    async fn disconnect_wifi(&self) -> Result<()>;
}

/// 应用启动器组件接口（components/launcher）。
#[async_trait]
pub trait LauncherComponent: DesktopComponent {
    /// 列出已安装应用（解析 .desktop 文件）。
    async fn list_installed_apps(&self) -> Result<Vec<AppInfo>>;

    /// 启动应用。
    async fn launch_app(&self, app: &AppTarget) -> Result<()>;

    /// 用默认应用打开 URI。
    async fn launch_uri(&self, uri: &str) -> Result<()>;
}

/// 通知组件接口（components/notification）。
#[async_trait]
pub trait NotificationComponent: DesktopComponent {
    /// 发送通知，返回通知 ID。
    async fn send_notification(&self, notif: &NotificationSpec) -> Result<u32>;

    /// 关闭通知。
    async fn close_notification(&self, id: u32) -> Result<()>;
}

/// 电源组件接口（components/power + backends DE 专有）。
#[async_trait]
pub trait PowerComponent: DesktopComponent {
    /// 锁屏。
    async fn lock_screen(&self) -> Result<()>;

    /// 注销当前用户。
    async fn logout(&self) -> Result<()>;

    /// 挂起。
    async fn suspend(&self) -> Result<()>;

    /// 休眠。
    async fn hibernate(&self) -> Result<()>;

    /// 关机。
    async fn power_off(&self) -> Result<()>;

    /// 查询电池状态。
    async fn get_battery_status(&self) -> Result<BatteryState>;
}

/// 剪贴板组件接口（components/clipboard）。
#[async_trait]
pub trait ClipboardComponent: DesktopComponent {
    /// 读取剪贴板文本。
    async fn clipboard_read(&self) -> Result<String>;

    /// 写入剪贴板文本。
    async fn clipboard_write(&self, text: &str) -> Result<()>;
}

/// 外观组件接口（components/appearance）。
#[async_trait]
pub trait AppearanceComponent: DesktopComponent {
    /// 设置壁纸。
    async fn set_wallpaper(&self, path: &str) -> Result<()>;

    /// 获取配色方案。
    async fn get_color_scheme(&self) -> Result<ColorScheme>;

    /// 设置配色方案。
    async fn set_color_scheme(&self, scheme: ColorScheme) -> Result<()>;
}

// ───────────────────────── 探测期占位接口（后续任务实现具体语义） ─────────────────────────

/// 输入组件接口（components/input，libei → ydotool → XTest 降级链）。
///
/// T2 任务（TSI-2315）实现注入方法；此处仅定义契约形状。
#[async_trait]
pub trait InputComponent: DesktopComponent {
    /// 当前激活的输入后端名。
    async fn active_backend(&self) -> Result<&'static str>;
}

/// 截图组件接口（components/capture，ScreenCast → Screenshot portal → X11 降级链）。
///
/// T2 任务（TSI-2316）实现捕获路径；此处仅定义契约形状。
#[async_trait]
pub trait CaptureComponent: DesktopComponent {
    /// 当前激活的捕获后端名。
    async fn active_backend(&self) -> Result<&'static str>;
}

/// 无障碍组件接口（components/a11y，AT-SPI）。
///
/// T2 任务（TSI-2317）实现树模型与语义定位；此处仅定义契约形状。
#[async_trait]
pub trait A11yComponent: DesktopComponent {
    /// AT-SPI Registry 是否可达。
    async fn registry_available(&self) -> Result<bool>;
}

// ───────────────────────── 测试 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeCompositor;

    #[async_trait]
    impl DesktopComponent for FakeCompositor {
        fn name(&self) -> &'static str {
            "fake-compositor"
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

    #[async_trait]
    impl CompositorComponent for FakeCompositor {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                window_management: true,
                ..Default::default()
            }
        }
        async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Ok(vec![])
        }
        async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
            Ok(None)
        }
        async fn focus_window(&self, _id: &WindowId) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn move_window(&self, _id: &WindowId, _x: i32, _y: i32) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn resize_window(&self, _id: &WindowId, _w: i32, _h: i32) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn minimize_window(&self, _id: &WindowId) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn unminimize_window(&self, _id: &WindowId) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn maximize_window(&self, _id: &WindowId) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn close_window(&self, _id: &WindowId) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn set_window_geometry(&self, _id: &WindowId, _geo: Rect) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn get_window_info(&self, _id: &WindowId) -> Result<WindowInfo> {
            Err(AgentShellError::WindowNotFound("fake".into()))
        }
        async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
            Ok(vec![])
        }
        async fn activate_workspace(&self, _id: &WorkspaceId) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn move_window_to_workspace(&self, _wid: &WindowId, _ws: &WorkspaceId) -> Result<()> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
        async fn list_monitors(&self) -> Result<Vec<crate::types::MonitorInfo>> {
            Ok(vec![])
        }
        async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
            Err(AgentShellError::NotImplemented("fake".into()))
        }
    }

    #[tokio::test]
    async fn compositor_via_dyn_desktop_component() {
        let c = FakeCompositor;
        let d: &dyn DesktopComponent = &c;
        assert_eq!(d.name(), "fake-compositor");
        assert_eq!(d.component_type(), ComponentType::Compositor);
        assert_eq!(d.health().await, ComponentHealth::Healthy);
    }

    #[tokio::test]
    async fn compositor_methods_dispatch_dynamically() {
        let c = FakeCompositor;
        let comp: Box<dyn CompositorComponent> = Box::new(c);
        assert!(comp.capabilities().window_management);
        assert!(comp.list_windows().await.unwrap().is_empty());
        assert!(matches!(
            comp.focus_window(&crate::types::WindowId {
                native_id: "1".into(),
                de_type: crate::types::DesktopEnvironment::KDE,
            })
            .await,
            Err(AgentShellError::NotImplemented(_))
        ));
    }

    #[test]
    fn capabilities_default_all_false() {
        let caps = BackendCapabilities::default();
        assert!(!caps.window_management);
        assert!(!caps.effects_control);
    }

    // MonitorLayout / MonitorConfig 引用检查（保证 services 类型与 trait 对齐）
    #[test]
    fn layout_types_are_constructible() {
        use crate::services::MonitorConfig;

        let layout = MonitorLayout {
            serial: 1,
            monitors: vec![],
        };
        assert_eq!(layout.serial, 1);
        let cfg: Option<MonitorConfig> = None;
        assert!(cfg.is_none());
    }
}
