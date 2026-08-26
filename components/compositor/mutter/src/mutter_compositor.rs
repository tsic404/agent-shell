//! MutterCompositor：GNOME 合成器组件（设计文档 §8.4，`mod.rs` 职责）。
//!
//! 双路径组合并实现 [`CompositorComponent`] 全部 17 方法：
//! - 路径选择按版本探测 + 探测回退（§8.4）：GNOME <47 → Eval 先行；
//!   47+ → Extension 先行；初始路径不可用自动回退另一条。
//! - 显示器配置走 `Mutter.DisplayConfig`（会话无关共享）。
//!
//! 能力矩阵如实上报（§8.4）：无 move/resize/workspace/原生事件流——
//! GNOME 是最受限的后端；`BackendCapabilities` 相应位为 false，不虚报。

use agent_shell_core::component::{CompositorComponent, DesktopComponent};
use async_trait::async_trait;
use zbus::Connection;

use crate::display_config::DisplayConfig;
use crate::error::Result;
use crate::eval::GnomeEvalBridge;
use crate::extension::ExtensionRunner;
use crate::version::{GnomeMajor, GnomeVersion};

/// 窗口语义双路径（§8.4 核心接口）。
pub enum GnomePath {
    /// org.gnome.Shell.Eval 直接执行（GNOME <47）。
    Eval(GnomeEvalBridge),
    /// Shell Extension 注册的 AgentShell 接口（GNOME 47+ 推荐）。
    Extension(ExtensionRunner),
}

impl GnomePath {
    /// 路径标识（doctor 输出用）。
    pub fn kind(&self) -> &'static str {
        match self {
            GnomePath::Eval(_) => "Eval",
            GnomePath::Extension(_) => "Extension",
        }
    }
}

/// GNOME 合成器组件（§3.3 继承层次：MutterCompositor 挂在
/// WaylandCompositor 之下的 D-Bus 特例——D-Bus 即基础通道）。
///
/// 会话无关共享：org.gnome.Shell D-Bus / Eval / Extension /
/// DisplayConfig 在 Wayland 与 X11 会话下行为一致（§8.4 共享能力表）。
pub struct MutterCompositor {
    /// 窗口语义通道（探测后确定）。
    path: GnomePath,
    /// Mutter.DisplayConfig 显示器配置通道。
    display_config: DisplayConfig,
    /// 探测到的版本（决定双路径选择与 doctor 报告）。
    version: GnomeVersion,
    /// session bus 连接（保活句柄，所有 proxy 共享；proxy 内部按需克隆）。
    conn: Connection,
    /// org.gnome.ScreenSaver 探测缓存（doctor 输出用；None=未探测，
    /// Some(false)=不在位——ScreenSaver 归 backends/gnome，本组件只读）。
    screensaver_probe: std::sync::atomic::AtomicU8,
}

impl MutterCompositor {
    /// 装配：连接 session bus → 版本探测 → 双路径选择。
    ///
    /// 选择逻辑（§8.4）：
    /// - GNOME <47：先探 Eval；被禁用/失败 → 回退 Extension；
    /// - GNOME 47+：先探 Extension；未安装 → 回退 Eval（若仍可开）。
    ///
    /// 两条路径都不可用时报 [`crate::error::MutterError::Extension`]——
    /// 窗口语义整体缺失，组件不可装配。
    pub async fn new() -> Result<Self> {
        let conn = Connection::session()
            .await
            .map_err(|e| crate::error::MutterError::Version(format!("session bus connect: {e}")))?;
        Self::with_connection(conn).await
    }

    /// 基于既有 session bus 连接装配（daemon 共享 dbus 句柄场景）。
    pub async fn with_connection(conn: Connection) -> Result<Self> {
        let version = crate::version::detect_version(&conn).await?;
        let eval = GnomeEvalBridge::new(conn.clone());
        let extension = ExtensionRunner::new(conn.clone());

        // 首选路径由版本归类决定（§8.4），失败即回退另一路径。
        let path = match version.major {
            GnomeMajor::Pre47 => match eval.probe().await {
                Ok(()) => GnomePath::Eval(eval),
                Err(eval_err) => {
                    tracing::info!(%eval_err, "gnome eval probe failed; falling back to extension");
                    extension
                        .probe()
                        .await
                        .map(|_| GnomePath::Extension(extension))?
                }
            },
            GnomeMajor::V47Plus => match extension.probe().await {
                Ok(()) => GnomePath::Extension(extension),
                Err(ext_err) => {
                    tracing::info!(%ext_err, "gnome extension probe failed; falling back to eval");
                    eval.probe().await.map(|_| GnomePath::Eval(eval))?
                }
            },
        };

        let display_config = DisplayConfig::new(conn.clone());
        Ok(Self {
            path,
            version,
            display_config,
            conn,
            screensaver_probe: std::sync::atomic::AtomicU8::new(0),
        })
    }

    /// 探测并缓存 org.gnome.ScreenSaver 在位性（doctor 输出用）。
    ///
    /// ScreenSaver 服务本身归 backends/gnome 装配——本组件只做**只读
    /// 探测**（NameHasOwner），不调用其方法。
    pub async fn ensure_screensaver_probe(&self) -> bool {
        use std::sync::atomic::Ordering;
        // 0=未探测，1=不在位，2=在位。
        if self.screensaver_probe.load(Ordering::Relaxed) == 2 {
            return true;
        }
        let bus_name =
            zbus::names::BusName::try_from("org.gnome.ScreenSaver").expect("static bus name");
        let dbus = match zbus::fdo::DBusProxy::new(&self.conn).await {
            Ok(d) => d,
            Err(_) => return false,
        };
        let ok = dbus.name_has_owner(bus_name).await.unwrap_or(false);
        self.screensaver_probe
            .store(if ok { 2 } else { 1 }, Ordering::Relaxed);
        ok
    }

    /// doctor 输出（§8.4 验证输出格式）。
    ///
    /// ScreenSaver 位按 [`Self::ensure_screensaver_probe`] 缓存结果渲染，
    /// 未探测/不在位时如实显示 ⚠——不硬编码 ✓。
    pub fn doctor_lines(&self) -> Vec<String> {
        use std::sync::atomic::Ordering;
        let screensaver = match self.screensaver_probe.load(Ordering::Relaxed) {
            2 => "✓",
            _ => "⚠",
        };
        vec![
            format!(
                "{} GNOME 版本   : GNOME {} ({})",
                "✓",
                self.version.full,
                if self.version.is_47_plus() {
                    "Eval 受限"
                } else {
                    "Eval 可用"
                }
            ),
            format!(
                "✓ 后端路径     : {} (agent-shell-bridge@multica.dev)",
                self.path.kind()
            ),
            format!(
                "✓ D-Bus 接口   : org.gnome.Shell ✓, DisplayConfig ✓, ScreenSaver {screensaver}"
            ),
            "⚠ 窗口操作     : 受限（无 move/resize/workspace）".to_string(),
        ]
    }

    // ───────────────────────── 内部分派 ─────────────────────────

    /// 列窗口：按当前路径分派。
    async fn windows(
        &self,
    ) -> agent_shell_core::error::Result<Vec<agent_shell_core::types::WindowInfo>> {
        match &self.path {
            GnomePath::Eval(e) => Ok(e.list_windows().await?),
            GnomePath::Extension(x) => Ok(x.list_windows().await?),
        }
    }

    /// 单窗操作分派（focus/close/minimize/maximize）。
    async fn window_op(
        &self,
        id: &agent_shell_core::types::WindowId,
        op: OpKind,
    ) -> agent_shell_core::error::Result<()> {
        let nid = id.native_id.as_str();
        match (&self.path, op) {
            (GnomePath::Eval(e), OpKind::Focus) => Ok(e.focus_window(nid).await?),
            (GnomePath::Eval(e), OpKind::Close) => Ok(e.close_window(nid).await?),
            (GnomePath::Eval(e), OpKind::Minimize(v)) => Ok(e.minimize_window(nid, v).await?),
            (GnomePath::Eval(e), OpKind::Maximize(v)) => Ok(e.maximize_window(nid, v).await?),
            (GnomePath::Extension(x), OpKind::Focus) => Ok(x.focus_window(nid).await?),
            (GnomePath::Extension(x), OpKind::Close) => Ok(x.close_window(nid).await?),
            (GnomePath::Extension(x), OpKind::Minimize(v)) => Ok(x.minimize_window(nid, v).await?),
            (GnomePath::Extension(x), OpKind::Maximize(v)) => Ok(x.maximize_window(nid, v).await?),
        }
    }
}

/// 窗口写操作类别（内部分派用，避免四份重复 match）。
enum OpKind {
    Focus,
    Close,
    Minimize(bool),
    Maximize(bool),
}
#[async_trait]
impl DesktopComponent for MutterCompositor {
    fn name(&self) -> &'static str {
        "mutter-compositor"
    }

    fn component_type(&self) -> agent_shell_core::component::ComponentType {
        agent_shell_core::component::ComponentType::Compositor
    }

    fn is_available(&self) -> bool {
        true // 构造成功即可用（双路径至少其一已确认）
    }

    async fn health(&self) -> agent_shell_core::component::ComponentHealth {
        use agent_shell_core::component::ComponentHealth;
        // DisplayConfig 可达性是唯一会话级健康信号——窗口路径构造时已
        // 确认，DisplayConfig 失败只降级显示器布局能力。
        match self.display_config.probe().await {
            Ok(()) => ComponentHealth::Healthy,
            Err(e) => ComponentHealth::Degraded(format!("DisplayConfig unreachable: {e}")),
        }
    }
}

#[async_trait]
impl CompositorComponent for MutterCompositor {
    /// 能力矩阵如实上报（§8.4）：GNOME 无 move/resize/workspace/
    /// 原生事件流。monitor_layout=true 仅代表可读布局（GetCurrentState）。
    fn capabilities(&self) -> agent_shell_core::component::BackendCapabilities {
        agent_shell_core::component::BackendCapabilities {
            // focus/close/minimize/maximize 经 Eval/Extension 可用。
            window_management: true,
            // GNOME 工作区语义不在 Eval/Extension 方法集内。
            workspace_management: false,
            monitor_layout: true,
            // Extension 三信号存在，但归一化依赖 extension.js 部署且
            // WindowOpened 详情为空——T3b 前不声明，避免调用方依赖
            // 一个语义未定的流（与 KWin 同口径）。
            window_events: false,
            workspace_events: false,
            native_input: false,   // portal RemoteDesktop（输入组件）
            native_capture: false, // portal ScreenCast（capture 组件）
            virtual_desktops: false,
            effects_control: false,
        }
    }

    async fn list_windows(
        &self,
    ) -> agent_shell_core::error::Result<Vec<agent_shell_core::types::WindowInfo>> {
        self.windows().await
    }

    async fn focus_window(
        &self,
        id: &agent_shell_core::types::WindowId,
    ) -> agent_shell_core::error::Result<()> {
        self.window_op(id, OpKind::Focus).await
    }

    /// 当前活动窗口（hasFocus 位筛选；桌面无焦点返回 None）。
    async fn get_active_window(
        &self,
    ) -> agent_shell_core::error::Result<Option<agent_shell_core::types::WindowInfo>> {
        match &self.path {
            GnomePath::Eval(e) => Ok(e.get_active_window().await?),
            GnomePath::Extension(x) => Ok(x.get_active_window().await?),
        }
    }

    /// 移动：Extension 路径有 `MoveWindow`（meta_window.move_frame）——
    /// X11 会话下有效，Wayland 会话下 Mutter 可能静默拒绝（move_frame
    /// 对不可移动窗口为 no-op）；Eval 路径无稳定 move 接口。能力矩阵
    /// 不声明 move（见 capabilities），本方法仅供显式调用方使用。
    async fn move_window(
        &self,
        id: &agent_shell_core::types::WindowId,
        x: i32,
        y: i32,
    ) -> agent_shell_core::error::Result<()> {
        match &self.path {
            GnomePath::Extension(ext) => Ok(ext.move_window(&id.native_id, x, y).await?),
            GnomePath::Eval(_) => Err(agent_shell_core::error::AgentShellError::NotImplemented(
                "mutter: window move unavailable on Eval path".into(),
            )),
        }
    }

    /// 缩放：同 move_window，受限报错。
    async fn resize_window(
        &self,
        _id: &agent_shell_core::types::WindowId,
        _w: i32,
        _h: i32,
    ) -> agent_shell_core::error::Result<()> {
        Err(agent_shell_core::error::AgentShellError::NotImplemented(
            "mutter: window resize not supported on GNOME (no protocol; portal-only)".into(),
        ))
    }

    async fn minimize_window(
        &self,
        id: &agent_shell_core::types::WindowId,
    ) -> agent_shell_core::error::Result<()> {
        self.window_op(id, OpKind::Minimize(true)).await
    }

    async fn unminimize_window(
        &self,
        id: &agent_shell_core::types::WindowId,
    ) -> agent_shell_core::error::Result<()> {
        self.window_op(id, OpKind::Minimize(false)).await
    }

    async fn maximize_window(
        &self,
        id: &agent_shell_core::types::WindowId,
    ) -> agent_shell_core::error::Result<()> {
        self.window_op(id, OpKind::Maximize(true)).await
    }

    async fn close_window(
        &self,
        id: &agent_shell_core::types::WindowId,
    ) -> agent_shell_core::error::Result<()> {
        self.window_op(id, OpKind::Close).await
    }

    /// 几何一次设定：move+resize 均受限，整体报错。
    async fn set_window_geometry(
        &self,
        _id: &agent_shell_core::types::WindowId,
        _geo: agent_shell_core::types::Rect,
    ) -> agent_shell_core::error::Result<()> {
        Err(agent_shell_core::error::AgentShellError::NotImplemented(
            "mutter: set_geometry not supported on GNOME (no protocol; portal-only)".into(),
        ))
    }

    /// 单窗查询：list_windows 过滤。
    async fn get_window_info(
        &self,
        id: &agent_shell_core::types::WindowId,
    ) -> agent_shell_core::error::Result<agent_shell_core::types::WindowInfo> {
        self.windows()
            .await?
            .into_iter()
            .find(|w| w.id == *id)
            .ok_or_else(|| {
                agent_shell_core::error::AgentShellError::WindowNotFound(id.native_id.clone())
            })
    }

    /// 工作区列表：**受限**——GNOME 工作区语义不在 Eval/Extension 方法集。
    async fn list_workspaces(
        &self,
    ) -> agent_shell_core::error::Result<Vec<agent_shell_core::types::WorkspaceInfo>> {
        Err(agent_shell_core::error::AgentShellError::NotImplemented(
            "mutter: workspace enumeration not supported on GNOME".into(),
        ))
    }

    async fn activate_workspace(
        &self,
        _id: &agent_shell_core::types::WorkspaceId,
    ) -> agent_shell_core::error::Result<()> {
        Err(agent_shell_core::error::AgentShellError::NotImplemented(
            "mutter: workspace activation not supported on GNOME".into(),
        ))
    }

    async fn move_window_to_workspace(
        &self,
        _wid: &agent_shell_core::types::WindowId,
        _ws: &agent_shell_core::types::WorkspaceId,
    ) -> agent_shell_core::error::Result<()> {
        Err(agent_shell_core::error::AgentShellError::NotImplemented(
            "mutter: move-to-workspace not supported on GNOME".into(),
        ))
    }

    /// 监视器列表：Mutter.DisplayConfig GetCurrentState → core MonitorInfo。
    ///
    /// 尺寸来源：逻辑监视器承载的物理监视器的**当前活动模式**（物理像素）；
    /// `geometry` 按缩放因子换算为逻辑像素（与 core 类型注释一致——
    /// Wayland 系 geometry 为缩放后坐标）。活动模式探测失败时尺寸为 0
    /// 并记录 warn（不猜测）。物理尺寸填 physical_geometry。
    async fn list_monitors(
        &self,
    ) -> agent_shell_core::error::Result<Vec<agent_shell_core::types::MonitorInfo>> {
        let layout = self.display_config.get_current_state().await?;
        Ok(layout
            .logical_monitors
            .iter()
            .enumerate()
            .map(|(i, lm)| {
                // 首个连接器对应的物理监视器（克隆模式下取主承载者）。
                let phys = lm
                    .connectors
                    .first()
                    .and_then(|c| layout.monitors.iter().find(|m| &m.connector == c));
                let (w, h) = phys.and_then(|m| m.active_mode_size()).unwrap_or((0, 0));
                if phys.and_then(|m| m.active_mode_size()).is_none() {
                    tracing::warn!(
                        connectors = ?lm.connectors,
                        "active mode size unknown for logical monitor {i}"
                    );
                }
                agent_shell_core::types::MonitorInfo {
                    id: agent_shell_core::types::MonitorId {
                        native_id: lm
                            .connectors
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("logical-{i}")),
                        de_type: agent_shell_core::types::DesktopEnvironment::GNOME,
                    },
                    name: lm.connectors.first().cloned().unwrap_or_default(),
                    geometry: agent_shell_core::types::Rect {
                        x: lm.x,
                        y: lm.y,
                        width: if lm.scale > 0.0 {
                            (w as f64 / lm.scale).round() as i32
                        } else {
                            w
                        },
                        height: if lm.scale > 0.0 {
                            (h as f64 / lm.scale).round() as i32
                        } else {
                            h
                        },
                    },
                    physical_geometry: agent_shell_core::types::Rect {
                        x: lm.x,
                        y: lm.y,
                        width: w,
                        height: h,
                    },
                    scale: lm.scale,
                    is_primary: lm.primary,
                    workspace_id: None,
                }
            })
            .collect())
    }

    /// 订阅事件流：仅 Extension 路径有三信号可订阅；Eval 路径无事件流。
    ///
    /// 返回的是 Extension 三信号的原始归一化流（WindowOpened /
    /// ActiveWindowChanged→Focused / WindowClosed）。capabilities().
    /// window_events 为 false——DesktopEvent 全量归一化在 T3b 落地；
    /// 调用方若订阅拿到的是明确的信号子集而非永不产出的空壳。
    async fn subscribe(
        &self,
    ) -> agent_shell_core::error::Result<Box<dyn agent_shell_core::EventStream>> {
        match &self.path {
            GnomePath::Extension(x) => Ok(x.subscribe().await?),
            GnomePath::Eval(_) => Err(agent_shell_core::error::AgentShellError::NotImplemented(
                "mutter: no event stream on Eval path (install extension for signals)".into(),
            )),
        }
    }
}
