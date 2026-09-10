//! daemon 核心状态：全部持久化连接的持有者（设计文档 §22.2 D1 / §23.2）。
//!
//! daemon 常驻用户会话，持有：
//! - 合成器通道（KDE → KWin；DDE → DdeCompositor，deepin-kwin 分支复用 KWin
//!   双通道，doctor 输出经 daemon 呈现）
//! - X11 通道（EWMH/ICCCM/XTest；XWayland 会话下输入注入降级用）
//! - WindowStateCache（查询走缓存；T3b 事件归一化落地后改为事件驱动刷新，
//!   当前以 TTL 短缓存近似——如实标注 `from_cache` 语义）

use agent_shell_a11y::AtSpiComponent;
use agent_shell_backend_dde::{CompositorKind, DdeCompositor};
use agent_shell_capture::CaptureDispatcher;
use agent_shell_compositor_kwin::KWinCompositor;
use agent_shell_core::component::{BackendCapabilities, CompositorComponent, DesktopComponent};
use agent_shell_core::types::WindowInfo;
use event::{EventHub, EventRing};
use std::time::{Duration, Instant};

/// daemon 持有的合成器后端。KDE 会话装 KWin，DDE 会话装 DdeCompositor
/// （其 deepin-kwin 分支复用 KWin 双通道，doctor 输出经 daemon 呈现）。
enum CompositorBackend {
    Kwin(Box<KWinCompositor>),
    Dde(DdeCompositor),
}

impl CompositorBackend {
    fn as_dyn(&self) -> &dyn CompositorComponent {
        match self {
            Self::Kwin(c) => c.as_ref(),
            Self::Dde(c) => c,
        }
    }

    /// doctor 报告用的后端展示名（KWin 保留历史值，DDE 委托 trait 方法）。
    fn name(&self) -> &'static str {
        match self {
            Self::Kwin(_) => "kwin-compositor",
            Self::Dde(c) => c.name(),
        }
    }

    /// 事件源标注：KWin 会话按 Wayland/X11 细分；DDE 会话按合成器形态
    /// 细分（deepin-kwin→KWinWayland / Treeland→Treeland / X11→X11Generic）。
    fn event_source_kind(&self) -> event::EventSource {
        match self {
            Self::Kwin(c) => match c.session_kind() {
                agent_shell_compositor_kwin::SessionKind::X11 => event::EventSource::KWinX11,
                _ => event::EventSource::KWinWayland,
            },
            Self::Dde(c) => dde_event_source(c.compositor.kind()),
        }
    }

    /// doctor 输出（与 `require_compositor` 一致的错误码映射在此不做——
    /// 返回 Vec 由 dispatch 层渲染；分支内各自补齐懒探测证据）。
    async fn doctor_lines_async(&self) -> Vec<String> {
        match self {
            Self::Kwin(c) => c.doctor_lines_async().await,
            Self::Dde(c) => c.doctor_lines_async().await,
        }
    }

    /// 会话后端对应的 `WindowId` 环境标签。KWin 内部打 KDE 标签
    /// （deepin-kwin 分支复用 KWin 亦同口径）；DDE 其余分支标 DDE。
    fn de_type(&self) -> agent_shell_core::types::DesktopEnvironment {
        match self {
            Self::Kwin(_) => agent_shell_core::types::DesktopEnvironment::KDE,
            Self::Dde(c) => dde_de_type(c.compositor.kind()),
        }
    }
}

/// DDE 合成器形态 → 事件源标签（§18.1 映射；deepin-kwin 复用 org_kde
/// 协议，与 KWinWayland 同源，不得误标 Treeland）。
fn dde_event_source(kind: CompositorKind) -> event::EventSource {
    match kind {
        CompositorKind::DeepinKwin => event::EventSource::KWinWayland,
        CompositorKind::Treeland => event::EventSource::Treeland,
        CompositorKind::X11 => event::EventSource::X11Generic,
    }
}

/// DDE 合成器形态 → `WindowId` 环境标签。deepin-kwin 与 X11 分支都复用
/// KWin 实现（org_kde_* 协议 / EWMH-ICCCM + D-Bus 桥），内部 ID 一律打
/// KDE 标签（同会话自洽）；Treeland 标 DDE。
fn dde_de_type(kind: CompositorKind) -> agent_shell_core::types::DesktopEnvironment {
    match kind {
        CompositorKind::DeepinKwin | CompositorKind::X11 => {
            agent_shell_core::types::DesktopEnvironment::KDE
        }
        CompositorKind::Treeland => agent_shell_core::types::DesktopEnvironment::DDE,
    }
}

/// 缓存条目有效期。T3b（EventHub 归一化）落地后由事件失效替代。
const CACHE_TTL: Duration = Duration::from_secs(2);

/// daemon 会话状态。
pub struct Daemon {
    compositor: Option<CompositorBackend>,
    cache: Vec<WindowInfo>,
    cached_at: Option<Instant>,
    /// 空闲退出时限（§22.2：默认 30min，可配置）。
    pub idle_timeout: Duration,
    /// capture 组件（三级降级链；None = 全后端探测失败，TTY 场景）。
    pub capture: Option<CaptureDispatcher>,
    /// input 组件（libei → ydotool → XTest 降级链；None = 全后端探测失败，TTY 场景）。
    pub input: Option<agent_shell_input::InputComponentHandle>,
    /// Portal 会话管理器（§22.6 D5）。
    pub portal_sessions: std::sync::Arc<crate::portal_sessions::PortalSessionManager>,
    /// IME 会话（§22.8 D7）。
    pub ime_session: crate::ime_session::ImeSession,
    /// 安全判定（§22.7 D6）：daemon 层唯一权限入口。
    pub security: agent_shell_core::security::SecurityManager,
    /// 发起调用的 agent 身份。由宿主编排层在启动 CLI/MCP 前经
    /// `AGENT_SHELL_AGENT_ID` 注入，子进程经 fork/exec 继承；未设置回落 `"*"`。
    pub caller_id: String,
    /// rootd 连接工厂（§23.4）。生产绑定真实 system bus 探测；测试注入
    /// 恒 `None` 的失败工厂，使无 rootd 降级路径在所有主机确定性可测。
    pub rootd_connect: crate::rootd_client::RootdConnector,
    /// AT-SPI 组件（None = a11y bus 不可达，a11y.query 返回 BackendUnavailable）。
    pub a11y: Option<AtSpiComponent>,
    /// 事件枢纽（§22.5 D4：订阅者 fan-out 中心）。
    pub hub: EventHub,
    /// 事件环形缓冲（§22.5 D4：CLI `events --replay`）。
    pub ring: EventRing,
    /// 待 serve_connection 取走的订阅句柄（转发任务消费）。
    pub subscriptions: Vec<event::EventSubscription>,
}

impl Daemon {
    /// 装配：连接当前会话合成器。失败不 panic——doctor 场景需要 daemon
    /// 存活以报告诊断细节，各方法在 `compositor=None` 时返回明确错误。
    pub async fn connect(idle_timeout: Duration) -> Self {
        let compositor = match session_kind().as_str() {
            "kde" => if std::env::var("WAYLAND_DISPLAY").is_ok() {
                KWinCompositor::new_wayland().await
            } else {
                KWinCompositor::new_x11().await
            }
            .map(Box::new)
            .map(CompositorBackend::Kwin)
            .map_err(|e| tracing::warn!("compositor assemble failed: {e}"))
            .ok(),
            "dde" => DdeCompositor::connect()
                .await
                .map(CompositorBackend::Dde)
                .map_err(|e| tracing::warn!("dde compositor assemble failed: {e}"))
                .ok(),
            other => {
                tracing::warn!("no compositor component for {other:?} (implemented: KDE, DDE)");
                None
            }
        };
        // 输入降级链（libei → ydotool → XTest）：与 compositor 独立装配，
        // TTY/无后端会话探测失败返回 None，input.send 报 BackendUnavailable。
        let de_type = agent_shell_core::de_detection::detect_desktop_environment();
        let input = agent_shell_input::detect(de_type).await.ok();

        // PortalSessionManager 先建——注入 CaptureDispatcher 作 TokenStore，
        // 使 ScreenCast 能 restore_token 静默恢复（§22.7 D5）。
        let portal_sessions = std::sync::Arc::new(
            crate::portal_sessions::PortalSessionManager::new(crate::single_instance::state_dir()),
        );
        let token_store: std::sync::Arc<dyn agent_shell_capture::TokenStore> =
            std::sync::Arc::clone(&portal_sessions) as _;
        let caller_id = std::env::var("AGENT_SHELL_AGENT_ID").unwrap_or_else(|_| "*".to_string());
        let security =
            agent_shell_core::security::SecurityManager::load_default().unwrap_or_else(|e| {
                tracing::warn!("security config load failed, falling back to defaults: {e}");
                agent_shell_core::security::SecurityManager::with_config(
                    agent_shell_core::security::AgentShellConfig::default(),
                )
            });
        Self {
            compositor,
            capture: CaptureDispatcher::with_token_store(Some(token_store)).await,
            input,
            cache: Vec::new(),
            cached_at: None,
            idle_timeout,
            portal_sessions,
            ime_session: crate::ime_session::ImeSession::new(),
            security,
            caller_id,
            a11y: AtSpiComponent::probe().await,
            rootd_connect: crate::rootd_client::connector(),
            hub: EventHub::new(),
            ring: EventRing::default(),
            subscriptions: Vec::new(),
        }
    }

    fn require_compositor(
        &self,
    ) -> Result<&dyn CompositorComponent, (agent_shell_rpc::RpcErrorCode, String)> {
        self.compositor.as_ref().map(|c| c.as_dyn()).ok_or_else(|| {
            (
                agent_shell_rpc::RpcErrorCode::BackendUnavailable,
                "compositor channel unavailable in this session".into(),
            )
        })
    }

    /// 当前会话对应的 [`event::EventSource`]（审查建议 4：X11 会话不得错标
    /// 为 KWinWayland；DDE 会话标注 Treeland 源）。
    fn event_source_kind(&self) -> event::EventSource {
        match self.compositor.as_ref() {
            Some(c) => c.event_source_kind(),
            None => event::EventSource::KWinWayland,
        }
    }

    ///
    /// 返回 `(列表, 是否来自缓存)`。
    pub async fn list_windows(
        &mut self,
    ) -> Result<(Vec<WindowInfo>, bool), (agent_shell_rpc::RpcErrorCode, String)> {
        if let Some(at) = self.cached_at {
            if at.elapsed() < CACHE_TTL {
                return Ok((self.cache.clone(), true));
            }
        }
        let wins = self
            .require_compositor()?
            .list_windows()
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))?;
        // 窗口差异事件：把 list_windows 的刷新接回 `ring` 与 `hub.publish`，
        // 使订阅者能收到事件、replay 有真实数据。首个快照（空缓存）会把
        // 全部当前窗口计为 WindowOpened——符合事件语义（订阅/回放前窗口
        // 即已存在）。后续刷新按 id 差分。事件源按会话类型标注（审查建议 4）。
        // 注意：这是「查询驱动的轮询差分」，不是 §22.5 D4 的持续真实推送
        // （EventNormalizer 装配留待 Phase 2/TSI-2317，见 docs 偏差记录）。
        let source_kind = self.event_source_kind();
        let now = Instant::now();
        let opened = wins
            .iter()
            .filter(|w| {
                !self
                    .cache
                    .iter()
                    .any(|old| old.id.native_id == w.id.native_id)
            })
            .map(|w| event::DesktopEvent::WindowOpened {
                info: w.clone(),
                source: source_kind.clone(),
                occurred_at: now,
            });
        let closed = self
            .cache
            .iter()
            .filter(|old| !wins.iter().any(|w| w.id.native_id == old.id.native_id))
            .map(|old| event::DesktopEvent::WindowClosed {
                id: old.id.clone(),
                source: source_kind.clone(),
                occurred_at: now,
            });
        for evt in opened.chain(closed) {
            self.ring.push(evt.clone());
            self.hub.publish(evt).await;
        }
        self.cache = wins;
        self.cached_at = Some(now);
        Ok((self.cache.clone(), false))
    }

    /// 单窗查询（复用窗口列表缓存）。
    pub async fn window_info(
        &mut self,
        native_id: &str,
    ) -> Result<WindowInfo, (agent_shell_rpc::RpcErrorCode, String)> {
        let (wins, _) = self.list_windows().await?;
        wins.into_iter()
            .find(|w| w.id.native_id == native_id)
            .ok_or_else(|| {
                (
                    agent_shell_rpc::RpcErrorCode::NotFound,
                    format!("window not found: {native_id:?}"),
                )
            })
    }

    /// 窗口写操作（写操作直通持久化连接，不经缓存）。
    pub async fn window_op(
        &self,
        native_id: &str,
        op: agent_shell_rpc::WindowOpKind,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), (agent_shell_rpc::RpcErrorCode, String)> {
        use agent_shell_rpc::WindowOpKind as K;
        let comp = self.require_compositor()?;
        let de_type = self
            .compositor
            .as_ref()
            .map(|c| c.de_type())
            .unwrap_or(agent_shell_core::types::DesktopEnvironment::KDE);
        let id = agent_shell_core::types::WindowId {
            native_id: native_id.to_string(),
            de_type,
        };
        // 写前校验目标存在（避免协议通道乐观发送吞掉无效 uuid）。
        let windows = comp
            .list_windows()
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))?;
        if !windows.iter().any(|win| win.id.native_id == native_id) {
            return Err((
                agent_shell_rpc::RpcErrorCode::NotFound,
                format!("window not found: {native_id:?}"),
            ));
        }
        let r = match op {
            K::Focus => comp.focus_window(&id).await,
            K::Move => comp.move_window(&id, x, y).await,
            K::Resize => comp.resize_window(&id, w, h).await,
            K::Minimize => comp.minimize_window(&id).await,
            K::Close => comp.close_window(&id).await,
        };
        r.map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))
    }

    /// doctor 输出行的异步版本：先补齐 `/Scripting` 探测证据再渲染
    /// （TSI-2486：doctor 路径此前从不触发懒探测，桥接行恒为「未探测」）。
    pub async fn doctor_lines_async(&self) -> Vec<String> {
        match self.compositor.as_ref() {
            Some(c) => c.doctor_lines_async().await,
            None => Vec::new(),
        }
    }

    /// 工作区切换。
    pub async fn activate_workspace(
        &self,
        ws: &str,
    ) -> Result<(), (agent_shell_rpc::RpcErrorCode, String)> {
        let comp = self.require_compositor()?;
        let list = comp
            .list_workspaces()
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))?;
        let target = list
            .iter()
            .find(|w| w.id.native_id == ws || w.number.to_string() == ws)
            .ok_or_else(|| {
                (
                    agent_shell_rpc::RpcErrorCode::NotFound,
                    format!("workspace not found: {ws:?}"),
                )
            })?;
        comp.activate_workspace(&target.id)
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))
    }

    /// 工作区列表（无缓存——低频操作）。
    pub async fn list_workspaces(
        &self,
    ) -> Result<Vec<String>, (agent_shell_rpc::RpcErrorCode, String)> {
        let comp = self.require_compositor()?;
        let list = comp
            .list_workspaces()
            .await
            .map_err(|e| (agent_shell_rpc::RpcErrorCode::BackendError, e.to_string()))?;
        Ok(list
            .into_iter()
            .map(|w| format!("{} {} active={}", w.number, w.name, w.is_active))
            .collect())
    }

    /// 组件健康摘要（doctor 数据源之一）。
    pub fn compositor_health(&self) -> Result<agent_shell_core::component::ComponentHealth, ()> {
        // health() 是 async；此处仅暴露可用性。完整 doctor 行在 dispatch 层异步收集。
        match &self.compositor {
            Some(_) => Ok(agent_shell_core::component::ComponentHealth::Healthy),
            None => Err(()),
        }
    }

    pub fn has_compositor(&self) -> bool {
        self.compositor.is_some()
    }

    /// 合成器能力真值（`info` 能力位表数据源）。无合成器时返回全 false
    /// ——能力位表与后端声明一致，不硬编码任何字段。
    pub fn compositor_capabilities(&self) -> BackendCapabilities {
        self.compositor
            .as_ref()
            .map(|c| c.as_dyn().capabilities())
            .unwrap_or_default()
    }

    /// 懒启动能力名集（事件流首次 `subscribe()` 才建立，激活前 false 非
    /// 永久不可用）。无合成器时为空——`info` 据此把对应行标 `lazy` 而非 `✗`。
    pub fn lazy_capabilities(&self) -> &'static [&'static str] {
        self.compositor
            .as_ref()
            .map(|c| c.as_dyn().lazy_capabilities())
            .unwrap_or(&[])
    }

    /// doctor 报告用的合成器后端名。
    pub fn compositor_name(&self) -> &'static str {
        self.compositor
            .as_ref()
            .map(|c| c.name())
            .unwrap_or("unavailable")
    }

    /// 当前窗口缓存条目数（daemon_status 报告用）。
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// 当前事件订阅者数量。
    pub fn subscriber_count(&self) -> usize {
        self.hub.subscriber_count()
    }
}

/// 当前会话的 DE 归类（与 core 检测同口径）。
fn session_kind() -> String {
    use agent_shell_core::de_detection::detect_desktop_environment;
    use agent_shell_core::types::DesktopEnvironment::*;
    match detect_desktop_environment() {
        KDE => "kde".into(),
        DDE => "dde".into(),
        GNOME => "gnome".into(),
        Tty | Unknown => "none".into(),
        _ => "other".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_survives_without_compositor() {
        // daemon 契约：装配失败不 panic（doctor 场景需要 daemon 存活报告诊断）。
        let d = Daemon::connect(Duration::from_secs(1)).await;
        // 在无合成器会话/CI 环境下 has_compositor 可能为 false，但结构必须完整。
        assert_eq!(d.has_compositor(), d.compositor.is_some());
    }

    #[tokio::test]
    async fn require_compositor_errors_gracefully() {
        let d = Daemon::connect(Duration::from_secs(1)).await;
        if !d.has_compositor() {
            let (code, msg) = match d.require_compositor() {
                Err(e) => e,
                Ok(_) => panic!("expected error without compositor"),
            };
            assert_eq!(code, agent_shell_rpc::RpcErrorCode::BackendUnavailable);
            assert!(msg.contains("unavailable"));
        }
    }

    #[test]
    fn dde_event_source_maps_all_kinds() {
        use agent_shell_backend_dde::CompositorKind as K;
        assert_eq!(
            dde_event_source(K::DeepinKwin),
            event::EventSource::KWinWayland
        );
        assert_eq!(dde_event_source(K::Treeland), event::EventSource::Treeland);
        assert_eq!(dde_event_source(K::X11), event::EventSource::X11Generic);
    }

    #[test]
    fn dde_de_type_maps_all_kinds() {
        use agent_shell_backend_dde::CompositorKind as K;
        use agent_shell_core::types::DesktopEnvironment as DE;
        assert_eq!(dde_de_type(K::DeepinKwin), DE::KDE);
        assert_eq!(dde_de_type(K::X11), DE::KDE);
        assert_eq!(dde_de_type(K::Treeland), DE::DDE);
    }

    #[test]
    fn compositor_name_reports_unavailable_without_backend() {
        // Compositor 变体在 A1 下均需 I/O 装配（KWinCompositor/Treeland），
        // 单测无法离线构造；名称委托的 Dde 路径由 dde crate 的 name() 契约
        // 覆盖。此处仅固化 None 路径（doctor 兜底展示名）。
        let d = Daemon {
            compositor: None,
            cache: Vec::new(),
            cached_at: None,
            idle_timeout: Duration::from_secs(1),
            capture: None,
            input: None,
            portal_sessions: std::sync::Arc::new(
                crate::portal_sessions::PortalSessionManager::new(
                    crate::single_instance::state_dir(),
                ),
            ),
            ime_session: crate::ime_session::ImeSession::new(),
            security: agent_shell_core::security::SecurityManager::load_default().unwrap_or_else(
                |_| {
                    agent_shell_core::security::SecurityManager::with_config(
                        agent_shell_core::security::AgentShellConfig::default(),
                    )
                },
            ),
            caller_id: "*".into(),
            rootd_connect: crate::rootd_client::connector(),
            a11y: None,
            hub: EventHub::new(),
            ring: EventRing::default(),
            subscriptions: Vec::new(),
        };
        assert_eq!(d.compositor_name(), "unavailable");
    }

    #[tokio::test]
    async fn event_ring_replay_after_push() {
        // events --replay 数据源：push 一个真实 DesktopEvent 后 snapshot 非空。
        let d = Daemon::connect(Duration::from_secs(1)).await;
        assert!(d.ring.snapshot().is_empty());
        d.ring.push(event::DesktopEvent::Noop);
        let events = d.ring.snapshot();
        assert!(!events.is_empty());
        assert_eq!(events.len(), 1);
    }
}
