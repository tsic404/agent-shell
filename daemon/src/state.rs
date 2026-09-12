//! daemon 核心状态：全部持久化连接的持有者（设计文档 §22.2 D1 / §23.2）。
//!
//! daemon 常驻用户会话，持有：
//! - 合成器通道（KDE → KWin；DDE → DdeCompositor；GNOME → MutterCompositor，
//!   D-Bus Eval/Extension 双路径，doctor 输出经 daemon 呈现）
//! - X11 通道（EWMH/ICCCM/XTest；XWayland 会话下输入注入降级用）
//! - WindowStateCache（查询走缓存；T3b 事件归一化落地后改为事件驱动刷新，
//!   当前以 TTL 短缓存近似——如实标注 `from_cache` 语义）

use agent_shell_a11y::{A11yOps, AtSpiComponent};
use agent_shell_backend_dde::{CompositorKind, DdeCompositor};
use agent_shell_capture::CaptureDispatcher;
use agent_shell_compositor_kwin::KWinCompositor;
use agent_shell_compositor_mutter::{GnomePathKind, MutterCompositor};
use agent_shell_core::component::{BackendCapabilities, CompositorComponent, DesktopComponent};
use agent_shell_core::types::WindowInfo;
use event::{EventHub, EventRing};
use std::time::{Duration, Instant};
/// daemon 持有的合成器后端。KDE 会话装 KWin，DDE 会话装 DdeCompositor，
/// GNOME 会话装 MutterCompositor（D-Bus Eval/Extension 双路径）。
enum CompositorBackend {
    Kwin(Box<KWinCompositor>),
    Dde(DdeCompositor),
    Mutter(Box<MutterCompositor>),
}

/// 原始事件源装配结果（区分「就绪 / 无原生流 / 失败」三态）。
enum RawSourceOutcome {
    /// 原始事件源就绪（KWin 事件脚本启动成功且接收端已取到）。
    Ready(Box<dyn event::RawSource>),
    /// 该后端无原生事件流（DDE Treeland / Mutter 未接线 / TTY）——查询差分兜底。
    Unavailable,
    /// 原始事件源装配失败（KWin 脚本启动失败 / 一次性队列已被占用）——可重试。
    Failed(String),
}

impl CompositorBackend {
    fn as_dyn(&self) -> &dyn CompositorComponent {
        match self {
            Self::Kwin(c) => c.as_ref(),
            Self::Dde(c) => c,
            Self::Mutter(c) => c.as_ref(),
        }
    }

    /// doctor 报告用的后端展示名（KWin 保留历史值，DDE/Mutter 委托 trait 方法）。
    fn name(&self) -> &'static str {
        match self {
            Self::Kwin(_) => "kwin-compositor",
            Self::Dde(c) => c.name(),
            Self::Mutter(c) => c.name(),
        }
    }

    /// 事件源标注：KWin 会话按 Wayland/X11 细分；DDE 会话按合成器形态
    /// 细分（deepin-kwin→KWinWayland / Treeland→Treeland / X11→X11Generic）；
    /// Mutter 会话按窗口语义路径细分（Eval→MutterEval / Extension→MutterExtension）。
    fn event_source_kind(&self) -> event::EventSource {
        match self {
            Self::Kwin(c) => match c.session_kind() {
                agent_shell_compositor_kwin::SessionKind::X11 => event::EventSource::KWinX11,
                _ => event::EventSource::KWinWayland,
            },
            Self::Dde(c) => dde_event_source(c.compositor.kind()),
            Self::Mutter(c) => mutter_event_source(c.path_kind()),
        }
    }

    /// doctor 输出（与 `require_compositor` 一致的错误码映射在此不做——
    /// 返回 Vec 由 dispatch 层渲染；分支内各自补齐懒探测证据）。
    async fn doctor_lines_async(&self) -> Vec<String> {
        match self {
            Self::Kwin(c) => c.doctor_lines_async().await,
            Self::Dde(c) => c.doctor_lines_async().await,
            Self::Mutter(c) => c.doctor_lines_async().await,
        }
    }

    /// 会话后端对应的 `WindowId` 环境标签。KWin 内部打 KDE 标签
    /// （deepin-kwin 分支复用 KWin 亦同口径）；DDE 其余分支标 DDE；
    /// Mutter 标 GNOME。
    fn de_type(&self) -> agent_shell_core::types::DesktopEnvironment {
        match self {
            Self::Kwin(_) => agent_shell_core::types::DesktopEnvironment::KDE,
            Self::Dde(c) => dde_de_type(c.compositor.kind()),
            Self::Mutter(_) => agent_shell_core::types::DesktopEnvironment::GNOME,
        }
    }
    /// 取原始事件源（§18.2），区分三态：
    /// - KWin 会话：事件脚本启动成功 → [`RawSourceOutcome::Ready`]；失败
    ///   （/Scripting 未就绪、一次性队列已被占用）→ [`RawSourceOutcome::Failed`]。
    /// - DDE Treeland / Mutter 未接线（T3b 范围外）→ [`RawSourceOutcome::Unavailable`]，
    ///   daemon 保持查询差分兜底。
    async fn raw_source_outcome(&self) -> RawSourceOutcome {
        match self {
            Self::Kwin(c) => match c.subscribe_raw().await {
                Ok(src) => RawSourceOutcome::Ready(src),
                Err(e) => RawSourceOutcome::Failed(e.to_string()),
            },
            Self::Dde(_) | Self::Mutter(_) => RawSourceOutcome::Unavailable,
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

/// Mutter 窗口语义路径 → 事件源标签（§18.1 映射：Eval 走 polling 桥接，
/// Extension 走 Shell Extension 信号）。
fn mutter_event_source(kind: GnomePathKind) -> event::EventSource {
    match kind {
        GnomePathKind::Eval => event::EventSource::MutterEval,
        GnomePathKind::Extension => event::EventSource::MutterExtension,
    }
}

/// 构造窗口信息解析器（§18.3 `resolve_window_info_by_id`）：归一化 open/focus
/// 原始事件时按 id 回查完整 [`WindowInfo`]。捕获合成器的 `Arc` 克隆，返回
/// `'static` future——归一化 task 的生命周期与 daemon 的 `&mut` 借用解耦。
fn window_resolver(
    compositor: std::sync::Arc<CompositorBackend>,
) -> event::normalize::WindowResolver {
    std::sync::Arc::new(move |native_id: &str| {
        let comp = std::sync::Arc::clone(&compositor);
        let native_id = native_id.to_string();
        Box::pin(async move {
            let id = agent_shell_core::types::WindowId {
                native_id,
                de_type: comp.de_type(),
            };
            comp.as_dyn().get_window_info(&id).await.ok()
        })
    })
}

/// 缓存条目有效期。T3b（EventHub 归一化）落地后由事件失效替代。
const CACHE_TTL: Duration = Duration::from_secs(2);

/// daemon 会话状态。
pub struct Daemon {
    compositor: Option<std::sync::Arc<CompositorBackend>>,
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
    pub a11y: Option<std::sync::Arc<dyn A11yOps>>,
    /// 事件枢纽（§22.5 D4：订阅者 fan-out 中心）。
    pub hub: EventHub,
    /// 事件环形缓冲（§22.5 D4：CLI `events --replay`）。
    pub ring: EventRing,
    /// 待 serve_connection 取走的订阅句柄（转发任务消费）。
    pub subscriptions: Vec<event::EventSubscription>,
    /// 事件归一化管线是否已启动（首次 `events subscribe` 时惰性装配；
    /// compositor 原始事件队列仅可消费一次，二次订阅不重复取流）。
    event_pipeline_started: bool,
}

impl Daemon {
    /// 装配：连接当前会话合成器。失败不 panic——doctor 场景需要 daemon
    /// 存活以报告诊断细节，各方法在 `compositor=None` 时返回明确错误。
    pub async fn connect(idle_timeout: Duration) -> Self {
        let compositor = match session_kind().as_str() {
            "kde" => if is_wayland_session() {
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
            "gnome" => if is_wayland_session() {
                MutterCompositor::new_wayland().await
            } else {
                MutterCompositor::new_x11().await
            }
            .map(Box::new)
            .map(CompositorBackend::Mutter)
            .map_err(|e| tracing::warn!("mutter compositor assemble failed: {e}"))
            .ok(),
            other => {
                tracing::warn!(
                    "no compositor component for {other:?} (implemented: KDE, DDE, GNOME)"
                );
                None
            }
        }
        .map(std::sync::Arc::new);
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
            a11y: AtSpiComponent::probe()
                .await
                .map(|a| std::sync::Arc::new(a) as std::sync::Arc<dyn A11yOps>),
            rootd_connect: crate::rootd_client::connector(),
            hub: EventHub::new(),
            ring: EventRing::default(),
            subscriptions: Vec::new(),
            event_pipeline_started: false,
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
        // 窗口差异事件（查询驱动轮询差分）：把 list_windows 的刷新接回
        // `ring` 与 `hub.publish`，使订阅者能收到事件、replay 有真实数据。
        // 管线激活后（`event_pipeline_started`）窗口事件由 EventNormalizer
        // 持续推送（raw → 归一化 → hub+ring），此处差分不再发布——否则同一
        // 窗口变化会被两条路径重复发布。差分仅作无管线（TTY / DDE Treeland /
        // Mutter 未接线）时的兜底数据源。
        let now = Instant::now();
        if !self.event_pipeline_started {
            let source_kind = self.event_source_kind();
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
    /// 惰性装配事件归一化管线（首次 `events subscribe` 时调用，幂等）。
    ///
    /// 取 compositor 原始事件源 → 注入 [`EventNormalizer`](event::EventNormalizer)
    /// （共享 hub 与 ring）→ `run()`。返回：
    /// - `Ok(())`：管线已装配，或此前已装配，或无原生流后端（差分兜底）；
    /// - `Err(..)`：装配失败（KWin 脚本启动失败等），**未**置位启动标志，可重试。
    pub async fn ensure_event_pipeline(
        &mut self,
    ) -> Result<(), (agent_shell_rpc::RpcErrorCode, String)> {
        if self.event_pipeline_started {
            return Ok(());
        }
        let Some(compositor) = self.compositor.as_ref().map(std::sync::Arc::clone) else {
            // 无合成器（TTY/headless）：无原生事件流，订阅仍走查询差分兜底。
            return Ok(());
        };
        let resolver = window_resolver(std::sync::Arc::clone(&compositor));
        let outcome = compositor.raw_source_outcome().await;
        self.apply_assembly_outcome(resolver, outcome)
    }

    /// 根据装配结果更新管线状态（纯逻辑，可单测注入任意 outcome）。
    ///
    /// 仅 [`RawSourceOutcome::Ready`] 置位启动标志；`Failed` 返回 Err 且不置位，
    /// 使下次订阅可重试；`Unavailable`（无原生流后端）保持差分兜底。
    fn apply_assembly_outcome(
        &mut self,
        resolver: event::normalize::WindowResolver,
        outcome: RawSourceOutcome,
    ) -> Result<(), (agent_shell_rpc::RpcErrorCode, String)> {
        match outcome {
            RawSourceOutcome::Ready(source) => {
                Self::install_pipeline(&self.hub, &self.ring, resolver, source);
                self.event_pipeline_started = true;
                Ok(())
            }
            RawSourceOutcome::Unavailable => Ok(()),
            RawSourceOutcome::Failed(msg) => {
                Err((agent_shell_rpc::RpcErrorCode::BackendUnavailable, msg))
            }
        }
    }

    /// 装配归一化器并启动（可注入 source 的纯装配核心，便于单测）。
    fn install_pipeline(
        hub: &EventHub,
        ring: &EventRing,
        resolver: event::normalize::WindowResolver,
        source: Box<dyn event::RawSource>,
    ) {
        let mut normalizer = event::EventNormalizer::new(hub.clone())
            .with_ring(ring.clone())
            .with_resolver(resolver);
        normalizer.add_source(source);
        normalizer.run();
    }

    /// 测试辅助：剥离合成器（模拟 headless/无显示服务器会话），使事件管线
    /// 装配走「无原生流 → 查询差分兜底」的确定性成功路径，测试不依赖真实
    /// KWin 运行状态。
    #[cfg(test)]
    pub(crate) fn with_compositor_none(mut self) -> Self {
        self.compositor = None;
        self
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

/// 校验 fd 是否为真实 socket（纯函数，供单测注入）。
///
/// `/proc/self/fd/{fd}` symlink 指向进程已打开文件；`metadata` 跟随 symlink，
/// socket 报 `is_socket() == true`，非法/已关闭 fd 报 ENOENT → false。
fn fd_is_socket(fd: i32) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(format!("/proc/self/fd/{fd}"))
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// `WAYLAND_SOCKET` 是否指向真实 socket fd。
///
/// socket 激活形态（systemd user service / fd 传递）下 `WAYLAND_SOCKET` 是
/// 数字 fd；非法/非 socket fd 会经 wayland-client 传入 tokio I/O driver，在
/// 后台 worker 线程 panic（`Bad file descriptor`）致 daemon abort——该 panic
/// 不在 `degrade_wayland_failure` 降级路径上，须在装配前拦截。
fn wayland_socket_is_socket() -> bool {
    let Some(fd) = std::env::var_os("WAYLAND_SOCKET")
        .and_then(|v| v.to_str().and_then(|s| s.parse::<i32>().ok()))
    else {
        return false;
    };
    fd_is_socket(fd)
}

/// 会话是否为 Wayland（与 `backends/{gnome,dde,kde}/src/assemble.rs`、
/// `clipboard`、`displayserver/wayland/registry.rs` 同 `var_os().is_some()`
/// 口径；`WAYLAND_SOCKET` 额外校验为真实 socket fd，见 [`wayland_socket_is_socket`]）。
fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || wayland_socket_is_socket()
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
    fn mutter_event_source_maps_all_kinds() {
        use agent_shell_compositor_mutter::GnomePathKind as K;
        assert_eq!(mutter_event_source(K::Eval), event::EventSource::MutterEval);
        assert_eq!(
            mutter_event_source(K::Extension),
            event::EventSource::MutterExtension
        );
    }

    #[test]
    fn fd_is_socket_rejects_bad_fds() {
        // 非法/越界 fd 必须判 false——否则会传入 tokio I/O driver 致 worker
        // 线程 panic、daemon abort（QA_FAILED 根因 ①）。
        assert!(!fd_is_socket(-1));
        assert!(!fd_is_socket(i32::MAX));
    }

    #[test]
    fn fd_is_socket_accepts_real_socket() {
        use std::os::fd::AsRawFd;
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        assert!(fd_is_socket(a.as_raw_fd()));
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
            event_pipeline_started: false,
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

    /// 假原始事件源：发一条 [`event::RawEvent::KWinWindowRemoved`]（无需 resolver
    /// 即可归一化为 `WindowClosed`），用于装配链路的可注入单测。
    struct FakeRawSource;

    impl event::RawSource for FakeRawSource {
        fn source_name(&self) -> &'static str {
            "fake-kwin"
        }
        fn source_kind(&self) -> event::EventSource {
            event::EventSource::KWinWayland
        }
        fn events(&self) -> futures::stream::BoxStream<'static, event::RawEvent> {
            use futures::{stream, StreamExt};
            stream::iter(std::iter::once(event::RawEvent::KWinWindowRemoved {
                id: "42".into(),
            }))
            .boxed()
        }
    }

    /// 空解析器：归一化 open/focus 事件时返回 `None`（本测试只走 close 路径）。
    fn noop_resolver() -> event::normalize::WindowResolver {
        std::sync::Arc::new(|_id: &str| {
            Box::pin(async { None::<agent_shell_core::types::WindowInfo> })
        })
    }

    /// 装配成功 → 归一化事件入 daemon ring（验收标准「subscribe 后 replay 返回
    /// 真实归一化事件」的装配链路覆盖，TSI-3033 审查 #5）。
    #[tokio::test]
    async fn install_pipeline_feeds_normalized_events_into_ring() {
        let d = Daemon::connect(Duration::from_secs(1)).await;
        Daemon::install_pipeline(&d.hub, &d.ring, noop_resolver(), Box::new(FakeRawSource));
        // 等归一化 task flush。
        tokio::time::sleep(Duration::from_millis(100)).await;
        let events = d.ring.snapshot();
        assert_eq!(events.len(), 1, "装配成功应把归一化事件入 ring");
        assert!(matches!(
            events.first(),
            Some(event::DesktopEvent::WindowClosed { id, .. }) if id.native_id == "42"
        ));
    }

    /// 首启失败不置位启动标志（可重试）；成功后才置位（审查 #2 回归）。
    #[tokio::test]
    async fn failed_assembly_keeps_pipeline_retryable() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resolver = noop_resolver();
        let err = d.apply_assembly_outcome(
            resolver.clone(),
            RawSourceOutcome::Failed("scripting not ready".into()),
        );
        assert!(err.is_err(), "装配失败必须返回错误");
        assert!(!d.event_pipeline_started, "失败不得置位启动标志");
        let ok =
            d.apply_assembly_outcome(resolver, RawSourceOutcome::Ready(Box::new(FakeRawSource)));
        assert!(ok.is_ok());
        assert!(d.event_pipeline_started, "成功应置位启动标志");
    }
}
