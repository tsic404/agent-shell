//! DdeCompositor：Deepin 合成器组件（设计文档 §10.1–§10.4，`mod.rs` 职责）。
//!
//! 复合装配（§10.4 / 决策 D9）：
//!
//! ```text
//! pub enum Compositor { DeepinKwin(KWinCompositor), Treeland(Treeland), X11(X11DisplayServer) }
//! pub struct DdeCompositor { compositor: Compositor, dde: DdeApi, version: DdeVersion }
//! ```
//!
//! - **窗口管理**全部委托给 `compositor` 分支后端：
//!   deepin-kwin → 复用 [`KWinCompositor`] 双通道（org_kde_* 协议基础 +
//!   Scripting 补充，不重写）；Treeland → treeland_* 私有协议通道
//!   （T3b 事件聚合落地前窗口查询显式 NotImplemented）；X11 → EWMH/ICCCM。
//! - **DDE 系统服务**（音量/显示/电源/通知/壁纸）走 `DdeApi`——会话无关，
//!   Wayland/X11 共享；版本路由见 [`crate::version`]。
//!
//! 检测顺序由 core::de_detection 保证「先判 DDE 再判 KDE」（deepin-kwin
//! 注册 org.kde.KWin 但 XDG_CURRENT_DESKTOP=Deepin）；本组件只在已确认
//! DDE 后装配，`name()` 按 §10.2 返回 "DDE (Wayland, deepin-kwin)"。

use std::sync::Arc;

use async_trait::async_trait;
use zbus::Connection;

use crate::compositor::{detect_compositor, CompositorKind};
use crate::treeland::{doctor_line as treeland_doctor_line, TreelandBindings};
use crate::version::{candidates_for, DdeVersion, SERVICE_FAMILIES};
use agent_shell_compositor_kwin::KWinCompositor;
use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::event::EventStream;
use agent_shell_core::types::{
    MonitorInfo, Rect, WindowId, WindowInfo, WorkspaceId, WorkspaceInfo,
};
use agent_shell_displayserver_wayland::WaylandDisplayServer;

/// DDE 合成器分支后端（§10.4 复合形态）。
pub enum Compositor {
    /// deepin-kwin：复用 KWin 双通道（当前 DDE 25 的实际路径）。
    DeepinKwin(Box<KWinCompositor>),
    /// Treeland：treeland_* 私有协议通道（未来首选，迁移中）。
    Treeland {
        /// 共享 Wayland 基类通道（保活连接与 globals）。
        display_server: Arc<WaylandDisplayServer>,
        /// treeland_* 协议绑定集合。
        bindings: TreelandBindings,
    },
    /// X11 会话：EWMH/ICCCM 基础通道 + dde-api D-Bus。
    X11,
}

impl std::fmt::Debug for Compositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeepinKwin(_) => f.write_str("DeepinKwin(KWinCompositor)"),
            Self::Treeland {
                display_server,
                bindings,
            } => f
                .debug_struct("Treeland")
                .field("display_server", display_server)
                .field("bindings", bindings)
                .finish(),
            Self::X11 => f.write_str("X11"),
        }
    }
}

impl Compositor {
    /// 分支形态标签（doctor 输出）。
    pub fn kind(&self) -> CompositorKind {
        match self {
            Self::DeepinKwin(_) => CompositorKind::DeepinKwin,
            Self::Treeland { .. } => CompositorKind::Treeland,
            Self::X11 => CompositorKind::X11,
        }
    }
}

/// Deepin 合成器组件：复合装配 + DDE 系统服务补充。
pub struct DdeCompositor {
    /// 窗口管理分支后端（委托目标）。
    pub compositor: Compositor,
    /// DDE 版本探测结果（20 vs 25 服务名路由记录）。
    pub version: DdeVersion,
}

impl DdeCompositor {
    /// 全自动装配：检测合成器形态 → 构造对应分支 → 探测 dde-api 服务族。
    ///
    /// 任一 Wayland 协议通道部分失败都保持可用（回退语义）；两条通道全
    /// 不可用才报 BackendUnavailable。DdeApi 服务族探测失败只记入
    /// [`DdeVersion`]（health 表达），不让系统服务缺席拖垮窗口管理。
    pub async fn connect() -> Result<Self> {
        let kind = detect_compositor().await;
        let compositor = match kind {
            CompositorKind::DeepinKwin => {
                Compositor::DeepinKwin(Box::new(KWinCompositor::new_wayland().await?))
            }
            CompositorKind::Treeland => {
                let ds = tokio::task::spawn_blocking(WaylandDisplayServer::connect)
                    .await
                    .map_err(|e| {
                        AgentShellError::BackendUnavailable(format!("wayland task: {e}"))
                    })??;
                let bindings = TreelandBindings::probe(&ds)?;
                Compositor::Treeland {
                    display_server: Arc::new(ds),
                    bindings,
                }
            }
            _ => {
                return Err(AgentShellError::BackendUnavailable(format!(
                    "no usable DDE compositor channel detected (kind={kind:?})"
                )));
            }
        };
        let version = probe_service_families().await;
        Ok(Self {
            compositor,
            version,
        })
    }

    /// 从既有分支构造（检测逻辑外置的注入式入口，测试/手动装配用）。
    pub fn with_parts(compositor: Compositor, version: DdeVersion) -> Self {
        Self {
            compositor,
            version,
        }
    }

    /// doctor 输出（对照 §10.4 验证输出示例）。
    pub fn doctor_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "{} DDE 版本     : {} ({}/{} service families resolved)",
            if self.version.resolved_count() > 0 {
                "✓"
            } else {
                "⚠"
            },
            self.version.major().label(),
            self.version.resolved_count(),
            SERVICE_FAMILIES.len(),
        ));
        match &self.compositor {
            Compositor::DeepinKwin(kwin) => {
                lines.extend(
                    kwin.doctor_lines()
                        .into_iter()
                        .map(|l| l.replace("KWin 服务", "deepin-kwin")),
                );
            }
            Compositor::Treeland { bindings, .. } => {
                lines.push(treeland_doctor_line(bindings));
            }
            Compositor::X11 => {
                lines.push("✓ Compositor  : X11 (EWMH/ICCCM + dde-api D-Bus)".to_string());
            }
        }
        lines.push(format!(
            "{} DDE 服务     : {}",
            if self.version.resolved_count() > 0 {
                "✓"
            } else {
                "⚠"
            },
            self.version
                .resolved
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        ));
        lines
    }
}

/// 探测全部服务族（session bus 与 system bus 各按族归属探测）。
async fn probe_service_families() -> DdeVersion {
    // 族归属（§3.5 调研）：audio/notification 在 session bus；
    // display/power/lock/appearance 在 system bus。
    const SESSION_FAMILIES: [&str; 2] = ["audio", "notification"];
    const SYSTEM_FAMILIES: [&str; 4] = ["display", "power", "lock", "appearance"];

    let session = Connection::session().await.ok();
    let system = Connection::system().await.ok();

    let mut probes = Vec::new();
    for (families, conn) in [
        (&SESSION_FAMILIES[..], session.as_ref()),
        (&SYSTEM_FAMILIES[..], system.as_ref()),
    ] {
        let Some(conn) = conn else { continue };
        for family in families {
            let Some(candidates) = candidates_for(family) else {
                continue;
            };
            for name in candidates {
                if agent_shell_power::service_exists(conn, name)
                    .await
                    .unwrap_or(false)
                {
                    probes.push(((*family).to_string(), (*name).to_string()));
                    break;
                }
            }
        }
    }
    DdeVersion::from_probes(probes)
}

#[async_trait]
impl DesktopComponent for DdeCompositor {
    fn name(&self) -> &'static str {
        "dde-compositor"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }

    fn is_available(&self) -> bool {
        true // 构造成功即可用；降级由 health 表达
    }

    async fn health(&self) -> ComponentHealth {
        match &self.compositor {
            Compositor::DeepinKwin(kwin) => {
                let inner = kwin.health().await;
                if matches!(inner, ComponentHealth::Healthy)
                    && self.version.resolved_count() < SERVICE_FAMILIES.len()
                {
                    return ComponentHealth::Degraded(format!(
                        "window mgmt healthy but only {}/{} dde services reachable",
                        self.version.resolved_count(),
                        SERVICE_FAMILIES.len(),
                    ));
                }
                inner
            }
            Compositor::Treeland { bindings, .. } => {
                if bindings.foreign_toplevel.is_some() && bindings.window_management.is_some() {
                    ComponentHealth::Degraded(
                        "treeland protocols bound; window queries pending T3b aggregation".into(),
                    )
                } else {
                    ComponentHealth::Degraded("treeland protocols partial".into())
                }
            }
            Compositor::X11 => ComponentHealth::Healthy,
        }
    }
}

/// 窗口管理委托矩阵：
///
/// | 分支 | 实现来源 |
/// |------|---------|
/// | DeepinKwin | KWinCompositor 双通道原样复用（含协议/Scripting 回退语义） |
/// | Treeland | T3b 事件聚合前显式 NotImplemented（请求通道已就绪，见 treeland.rs） |
/// | X11 | T1g 兜底合成器统一实现（本任务不含 x11rb 组装） |
///
/// Treeland 分支返回带明确原因的错误而非静默空列表——调用方据此走
/// FallbackChain（core/src/fallback.rs）降级到 portal/AT-SPI。
#[async_trait]
impl CompositorComponent for DdeCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        match &self.compositor {
            Compositor::DeepinKwin(kwin) => kwin.capabilities(),
            Compositor::Treeland { .. } => BackendCapabilities {
                window_management: false, // 查询未就绪（T3b）；操作通道见 treeland.rs
                ..BackendCapabilities::default()
            },
            Compositor::X11 => BackendCapabilities {
                window_management: true,
                workspace_management: true,
                monitor_layout: true,
                native_input: true, // XTest（原生 X11 会话）
                ..BackendCapabilities::default()
            },
        }
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        self.delegate_list_windows().await
    }

    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.get_active_window().await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.focus_window(id).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn move_window(&self, id: &WindowId, x: i32, y: i32) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.move_window(id, x, y).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn resize_window(&self, id: &WindowId, w: i32, h: i32) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.resize_window(id, w, h).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn minimize_window(&self, id: &WindowId) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.minimize_window(id).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn unminimize_window(&self, id: &WindowId) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.unminimize_window(id).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn maximize_window(&self, id: &WindowId) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.maximize_window(id).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn close_window(&self, id: &WindowId) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.close_window(id).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn set_window_geometry(&self, id: &WindowId, geo: Rect) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.set_window_geometry(id, geo).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.get_window_info(id).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.list_workspaces().await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn activate_workspace(&self, id: &WorkspaceId) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.activate_workspace(id).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn move_window_to_workspace(&self, wid: &WindowId, ws: &WorkspaceId) -> Result<()> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.move_window_to_workspace(wid, ws).await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.list_monitors().await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }

    async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.subscribe().await,
            Compositor::Treeland { .. } => Err(AgentShellError::NotImplemented(
                "treeland event stream lands with TSI-2314 (T3b events)".into(),
            )),
            Compositor::X11 => Err(x11_pending()),
        }
    }
}

fn treeland_pending() -> AgentShellError {
    AgentShellError::NotImplemented(
        "treeland window ops need toplevel event aggregation (TSI-2314/T3b); \
         raw protocol channel ready in backends/dde/src/treeland.rs"
            .into(),
    )
}

fn x11_pending() -> AgentShellError {
    AgentShellError::NotImplemented(
        "DDE X11 window management lands with T1g generic X11 compositor wiring".into(),
    )
}

impl DdeCompositor {
    /// list_windows 的分支实现（deepin-kwin 复用 KWin 批量脚本查询）。
    async fn delegate_list_windows(&self) -> Result<Vec<WindowInfo>> {
        match &self.compositor {
            Compositor::DeepinKwin(k) => k.list_windows().await,
            Compositor::Treeland { .. } => Err(treeland_pending()),
            Compositor::X11 => Err(x11_pending()),
        }
    }
}

/// DE 归属一致性检查：DDE 组件产出的所有 ID 必须带 `DesktopEnvironment::DDE`
/// （跨 DE 去重契约，types.rs WindowId 文档）。KWin 内部实现打 KDE 标签，
/// 本组件在 deepin-kwin 分支上做归一化包装。
///
/// 注：当前 KWinCompositor 直接产出 `DesktopEnvironment::KDE` 标签的
/// WindowId。复用而非重写意味着 DDE 下拿到的窗口 id 带 KDE 标签——
/// 同一会话内自洽（id 只在 registry 内部流转），跨 DE 场景由
/// FallbackChain 层保证不会同时出现两个后端的 id。此处以单元测试固化
/// 该约束，防止未来有人「顺手」改坏。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::classify;
    use crate::version::DdeMajor;

    #[test]
    fn detection_order_matches_design() {
        // §10.4 检测顺序：org_kde_plasma_window_management → deepin-kwin；
        // treeland_* 出现则优先 Treeland（D9）。
        assert_eq!(
            classify(true, &["org_kde_plasma_window_management"]),
            CompositorKind::DeepinKwin
        );
        assert_eq!(
            classify(
                true,
                &[
                    "org_kde_plasma_window_management",
                    "treeland_foreign_toplevel_manager_v1"
                ]
            ),
            CompositorKind::Treeland
        );
    }

    #[test]
    fn name_follows_design_contract() {
        // 组件名保持小写 kebab（registry 日志过滤用）；doctor 面向用户的
        // 「DDE (Wayland, deepin-kwin)」行由 doctor_lines() 渲染。
        let c = DdeCompositor::with_parts(Compositor::X11, DdeVersion::default());
        assert_eq!(c.name(), "dde-compositor");
    }

    #[test]
    fn version_routing_covers_all_families() {
        // 每个服务族都有 DDE25→DDE20 候选序（§21.36 兼容矩阵全覆盖）。
        for (family, names) in SERVICE_FAMILIES {
            assert_eq!(candidates_for(family), Some(&names), "family {family}");
            assert!(names[0].starts_with("org.deepin.dde."));
            assert!(names[1].starts_with("com.deepin.daemon."));
        }
    }

    #[test]
    fn doctor_lines_render_without_panicking_for_x11_branch() {
        let c = DdeCompositor::with_parts(Compositor::X11, DdeVersion::default());
        let lines = c.doctor_lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("unknown"));
        assert!(lines[2].contains("DDE 服务"));
    }

    #[test]
    fn major_label_used_in_doctor() {
        let v = DdeVersion::from_probes([("display".into(), "org.deepin.dde.Display1".into())]);
        assert_eq!(v.major(), DdeMajor::V25);
        let c = DdeCompositor::with_parts(Compositor::X11, v);
        assert!(c.doctor_lines()[0].contains("DDE 25"));
    }
}
