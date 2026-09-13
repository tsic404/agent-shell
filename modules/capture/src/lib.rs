//! 截图与捕获组件（设计文档 §13 截图与捕获子系统）。
//!
//! 三级降级的 [`CaptureDispatcher`]——为上层提供单帧捕获与流式帧：
//!
//! 1. portal ScreenCast → PipeWire 流式（各 Wayland DE 公共首选，§3.5.3）
//! 2. portal Screenshot → PNG 单帧
//! 3. X11 原生 MIT-SHM（X11Generic 会话直用）
//!
//! 另含跨帧变化检测缓存 [`cache::CaptureCache`]（§13.4）。
//!
//! TTY 环境：无任何后端时组件不可用——`CaptureDispatcher::assemble`
//! 返回 `None`，所有方法返回结构化 `BackendUnavailable`，不 panic。

pub mod cache;
pub mod portal_common;
pub mod portal_screencast;
pub mod portal_screenshot;
pub mod x11;

use agent_shell_core::component::{
    CaptureComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use async_trait::async_trait;
use std::future::Future;

pub use cache::CaptureCache;
pub use portal_screencast::{
    CaptureTarget, Frame, PersistMode, PixelFormat, ScreenCastCapture, ScreenCastOptions,
    ScreenCastSession,
};
pub use portal_screenshot::{ScreenshotPortal, SCREENSHOT_MAX_ATTEMPTS, SCREENSHOT_TIMEOUT};
pub use x11::X11Capture;

/// portal restore_token 持久化抽象——daemon 侧 `PortalSessionManager` 实现。
///
/// capture 组件不直接依赖 daemon 的 `PortalSessionManager` 类型，经此 trait
/// 解耦：daemon 装配时注入实现，capture 负责存/取 token。
pub trait TokenStore: Send + Sync {
    /// 取上次持久化的 restore_token（无则 None）。
    fn get_restore_token(&self) -> Option<String>;
    /// 保存新 restore_token（覆盖旧值；None 清除）。
    fn save_restore_token(&self, token: Option<String>);
}

/// 组件名（doctor 报告用）。
pub const COMPONENT_NAME: &str = "capture";

/// portal 授权探测预算（TSI-3054）：portal 段（ScreenCast Start 弹窗等待 +
/// Screenshot 交互超时）单独封顶 6s，为 x11 兜底与进程启动/RPC 往返留出
/// 余量。
///
/// 无 portal 授权时，ScreenCast Start 弹窗无人应答会吃满
/// [`portal_screencast::SCREENCAST_TIMEOUT`]（10s）+
/// [`portal_screenshot::SCREENSHOT_TIMEOUT_INTERACTIVE`]（5s）= 15s。QA 验收
/// （TC-301/303）以 8s 为命令超时上限（`timeout 8` → exit 124），故 portal
/// 段封顶 6s、端到端（portal + x11 兜底）由 [`CAPTURE_END_TO_END_BUDGET`]
/// 封顶 7s——保证无授权时 8s 内必出结果（x11 帧）或快速失败。
pub const PORTAL_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);

/// 端到端捕获截止（TSI-3054）：portal 探测 + x11 兜底整体封顶 7s，低于 QA
/// 验收 `timeout 8` 上限（留 1s 余量给进程启动与 RPC 往返）。
///
/// 单独约束 portal 段（[`PORTAL_PROBE_BUDGET`]）不足以防 x11 兜底自身阻塞；
/// 此截止把「portal 探测 + x11 抓帧」作为整体兜底，超时即快速失败。
pub const CAPTURE_END_TO_END_BUDGET: std::time::Duration = std::time::Duration::from_secs(7);

/// 捕获结果：帧或已落盘的 PNG 路径。
#[derive(Clone, Debug)]
pub enum CapturedFrame {
    /// 原始像素帧。
    Pixels(Frame),
    /// PNG 文件路径（portal Screenshot 落盘）。
    Png(std::path::PathBuf),
}

impl CapturedFrame {
    /// 原始像素访问；PNG 路径变体返回 None（由调用方按需解码）。
    pub fn pixels(&self) -> Option<&Frame> {
        match self {
            Self::Pixels(f) => Some(f),
            Self::Png(_) => None,
        }
    }
}

/// 实际选中的后端名（doctor / `active_backend` 输出）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveBackend {
    ScreenCast,
    ScreenshotPortal,
    X11,
}

impl ActiveBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::ScreenCast => "portal-screencast",
            Self::ScreenshotPortal => "portal-screenshot",
            Self::X11 => "x11-mit-shm",
        }
    }
}

/// 三级降级捕获组件（装配结果持有者）。
///
/// 装配探测链（design/02 §4.2）：`capture: portal ScreenCast →
/// Screenshot → X11`。ScreenCast 会话建立需用户弹窗确认，构造期只做
/// 无副作用探测（bus 上 portal 是否可达、原生 X11 会话是否存在）；实际
/// 后端由首次 [`Self::capture`] 或 [`Self::probe`] 惰性建立并记录到 `active`。
///
/// `token_store` 注入后，ScreenCast 优先尝试用持久化的 `restore_token`
/// 静默恢复会话——恢复成功则无弹窗（无交互授权路径）。
pub struct CaptureDispatcher {
    conn: zbus::Connection,
    screencast_ok: bool,
    screenshot_ok: bool,
    screenshot: ScreenshotPortal,
    x11_present: bool,
    /// 当前是否 Wayland 会话（`WAYLAND_DISPLAY`/`WAYLAND_SOCKET` 存在）。
    ///
    /// Wayland 下 portal 是唯一授权闸门：无授权时降级 x11 会静默抓取
    /// XWayland root（越权），须快速失败而非抓屏（TSI-3054 审查 #2）。
    wayland_session: bool,
    active: std::sync::Mutex<Option<ActiveBackend>>,
    /// restore_token 持久化（daemon 的 PortalSessionManager；无则 None）。
    token_store: Option<std::sync::Arc<dyn TokenStore>>,
    /// 已建立的 ScreenCast 流会话（daemon 复用，避免反复弹窗 §21.22）。
    session: tokio::sync::Mutex<Option<std::sync::Arc<ScreenCastCapture>>>,
    /// X11 捕获器（惰性建连，daemon 复用连接——审查项 #5）。
    x11: tokio::sync::OnceCell<X11Capture>,
}

impl CaptureDispatcher {
    /// 按探测链装配（无 token 持久化）。全部后端不可用返回 `None`（TTY 场景）。
    pub async fn assemble() -> Option<Self> {
        Self::with_token_store(None).await
    }

    /// 按探测链装配，注入 `restore_token` 持久化后端。
    ///
    /// `token_store` 为 daemon 的 `PortalSessionManager`；注入后 ScreenCast
    /// 优先尝试 `restore_token` 静默恢复，避免交互弹窗（§22.7 D5）。
    pub async fn with_token_store(
        token_store: Option<std::sync::Arc<dyn TokenStore>>,
    ) -> Option<Self> {
        let conn = zbus::Connection::session().await.ok()?;
        let screencast_ok = ScreenCastCapture::available(&conn).await;
        let screenshot = ScreenshotPortal::with_connection(conn.clone());
        let screenshot_ok = screenshot.available().await;
        let x11_present = X11Capture::display_present();
        let wayland_session = is_wayland_session();
        if !screencast_ok && !screenshot_ok && !x11_present {
            return None;
        }
        Some(Self {
            conn,
            screencast_ok,
            screenshot_ok,
            screenshot,
            x11_present,
            wayland_session,
            active: std::sync::Mutex::new(None),
            token_store,
            session: tokio::sync::Mutex::new(None),
            x11: tokio::sync::OnceCell::new(),
        })
    }

    /// 窗口直捕（X11-only；portal 无法定位 native window id）。
    /// 连接经 `OnceCell` 惰性建立并复用。
    ///
    /// 无 `DISPLAY`（无 X server 可达）返回 `BackendUnavailable`。
    pub async fn capture_window(&self, window: u32) -> Result<Frame> {
        if !self.x11_present {
            return Err(AgentShellError::BackendUnavailable(
                "x11 capture unavailable: no X server reachable".into(),
            ));
        }
        let x = self
            .x11
            .get_or_try_init(|| async { X11Capture::connect() })
            .await?;
        x.capture_window(window).await
    }

    /// 当前实际选中的后端（未捕获过则返回 None）。
    pub fn selected_backend(&self) -> Option<ActiveBackend> {
        *self.active.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 可用后端名列表（doctor 输出）。
    pub fn available_backends(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.screencast_ok {
            v.push(ActiveBackend::ScreenCast.name());
        }
        if self.screenshot_ok {
            v.push(ActiveBackend::ScreenshotPortal.name());
        }
        if self.x11_present {
            v.push(ActiveBackend::X11.name());
        }
        v
    }

    /// 记录当前激活后端（doctor / active_backend 报告源）。
    fn set_active(&self, backend: Option<ActiveBackend>) {
        *self.active.lock().unwrap_or_else(|p| p.into_inner()) = backend;
    }

    /// 单帧捕获：portal（ScreenCast → Screenshot）→ 失败/超时按会话降级 X11 或快速失败。
    ///
    /// ScreenCast 会话优先尝试 `restore_token` 静默恢复（无弹窗）；
    /// `interactive=true` 时允许弹窗授权（无 token 或恢复失败时）；
    /// `interactive=false` 时仅当 `token_store` 含 `restore_token` 才尝试
    /// ScreenCast——无 token 则直接降级到 Screenshot/X11（避免弹窗）。
    ///
    /// 交互路径端到端（portal 探测 + x11 兜底）受 [`CAPTURE_END_TO_END_BUDGET`]
    /// 封顶：无 portal 授权时弹窗无人应答，portal 段先被 [`PORTAL_PROBE_BUDGET`]
    /// 截断，随后降级 x11（原生 X11 会话）或快速失败（Wayland/无 x11），
    /// 全程保证 8s 内出结果（TSI-3054）。
    pub async fn capture(&self, target: CaptureTarget, interactive: bool) -> Result<CapturedFrame> {
        if interactive {
            capture_with_budget(
                CAPTURE_END_TO_END_BUDGET,
                self.capture_portal_then_fallback(target, true),
            )
            .await
        } else {
            self.capture_portal_then_fallback(target, false).await
        }
    }

    /// portal 段（受 [`PORTAL_PROBE_BUDGET`] 子预算）→ 失败/超时降级 x11 或快速失败。
    ///
    /// portal 失败后的去向由 [`portal_fallback`] 决定：原生 X11 会话降级 x11；
    /// Wayland 会话拒绝静默抓 XWayland root（越权）、无 x11 兜底时快速失败
    /// 并给出明确报错（TSI-3054 审查 #2/#3）。
    async fn capture_portal_then_fallback(
        &self,
        target: CaptureTarget,
        interactive: bool,
    ) -> Result<CapturedFrame> {
        let portal = if interactive {
            capture_with_budget(PORTAL_PROBE_BUDGET, self.capture_portal(target, true)).await
        } else {
            self.capture_portal(target, false).await
        };
        match portal {
            Ok(frame) => Ok(frame),
            Err(e) => {
                tracing::warn!("portal capture unavailable: {e}");
                match portal_fallback(self.wayland_session, self.x11_present) {
                    PortalFallback::X11 => self.capture_x11().await,
                    PortalFallback::Fail(err) => Err(err),
                }
            }
        }
    }

    /// [`Self::capture`] 的 portal 段主体：ScreenCast → Screenshot。
    ///
    /// `interactive=true` 时 Screenshot 先做非交互预探测（§13.1 建议 2）——
    /// 已授权/免弹窗即时成功、无会话/自动化即时 Permission，两种情况都不
    /// 弹窗、不吃交互超时预算；预探测失败才升级到交互弹窗。全部失败返回
    /// `BackendUnavailable`，由调用方降级 x11。
    async fn capture_portal(
        &self,
        target: CaptureTarget,
        interactive: bool,
    ) -> Result<CapturedFrame> {
        // L1: portal ScreenCast（流式，daemon 复用会话）。
        if let Some(s) = self.ensure_screencast_session(target, interactive).await {
            match s.capture_frame().await {
                Ok(frame) => {
                    self.set_active(Some(ActiveBackend::ScreenCast));
                    return Ok(CapturedFrame::Pixels(frame));
                }
                Err(e) => {
                    tracing::warn!("screencast frame failed, degrade: {e}");
                    // 会话失效即丢弃，下次重新走五步流程。
                    *self.session.lock().await = None;
                    self.set_active(None);
                }
            }
        }

        // L2: portal Screenshot。
        if self.screenshot.available().await {
            // 交互先非交互预探测（§13.1 建议 2 / Radian 审查）：已授权/免弹窗
            // 即时成功、无会话即时 Permission，均不弹窗；黑/无效帧与超时视为
            // 预探测失败，升级到交互弹窗。预探测受交互预算约束（5s/单次口径），
            // 不单独吃满 8s×2 超时（否则 ScreenCast 弹窗耗尽 10s 后会被外层
            // 总预算截断，授权弹窗永不弹出）。
            if interactive {
                if let Some(frame) = self
                    .screenshot_preprobe(portal_screenshot::SCREENSHOT_TIMEOUT_INTERACTIVE)
                    .await
                {
                    self.set_active(Some(ActiveBackend::ScreenshotPortal));
                    return Ok(frame);
                }
            }
            match self.screenshot.capture(interactive).await {
                Ok(path) => {
                    // 校验产物确为可解码的非黑 PNG：DDE 的 xdg-desktop-portal-dde
                    // 委托 KWin 落盘 JPEG（实测 /tmp/kwin_screenshot_*.jpg），
                    // portal 却把「成功」返回成 CapturedFrame::Png，默认流程不会
                    // 触发 x11 兜底，daemon 按 PNG 解析报「not a PNG file」。
                    // 非 PNG / 损坏 / 全黑一律视为 portal 不可用，降级 x11
                    // （原生 X11 会话）或快速失败（Wayland / 无 x11）。
                    match validated_screenshot_frame(path) {
                        Some(p) => {
                            self.set_active(Some(ActiveBackend::ScreenshotPortal));
                            return Ok(CapturedFrame::Png(p));
                        }
                        None => {
                            tracing::warn!(
                                "portal Screenshot returned a non-PNG/invalid artifact \
                                 (e.g. JPEG from xdg-desktop-portal-dde); degrading to next backend"
                            );
                            self.set_active(None);
                        }
                    }
                }
                Err(e) => tracing::warn!("screenshot portal failed, degrade: {e}"),
            }
        }

        Err(AgentShellError::BackendUnavailable(
            "portal capture unavailable (ScreenCast/Screenshot unreachable or denied)".into(),
        ))
    }

    /// X11 原生兜底（portal 不可用/超时/拒绝后）。
    ///
    /// 仅在 [`portal_fallback`] 判定「原生 X11 会话可兜底」时被调用，故此处
    /// 不再复核 `x11_present`（决策与最终错误由纯函数集中，可单测）。全黑帧
    /// （XWayland root 无合成器内容）保留并告警——上层据 [`Self::probe`] /
    /// doctor 报告降级链真实状态。
    async fn capture_x11(&self) -> Result<CapturedFrame> {
        let cap = X11Capture::connect().map_err(|e| {
            tracing::warn!("x11 capture unavailable: {e}");
            // 失败即清 active——否则先前记录的 stale 后端会残留，
            // doctor/selected_backend 持续报假「选中」状态。
            self.set_active(None);
            e
        })?;
        let frame = cap.capture_frame().await.map_err(|e| {
            tracing::warn!("x11 capture frame failed: {e}");
            self.set_active(None);
            e
        })?;
        if is_all_black(&frame) {
            tracing::warn!(
                "X11 fallback captured an all-black frame (likely XWayland root without \
                 compositor content); portal ScreenCast/Screenshot is the correct backend \
                 for this session"
            );
        }
        self.set_active(Some(ActiveBackend::X11));
        Ok(CapturedFrame::Pixels(frame))
    }

    /// 非交互预探测（§13.1 建议 2 / Radian 审查）：已授权/免弹窗即时成功；
    /// 无会话/自动化即时 Permission。黑/无效帧、超时、Permission 均视为
    /// 「未授权」跳过（返回 `None`），由调用方升级到交互弹窗。
    ///
    /// 受 `budget` 约束（交互预算内的短探测）：超时视为跳过而非失败——避免
    /// 预探测的 8s×2 超时在 ScreenCast 弹窗耗尽预算后被外层总预算截断，致
    /// 授权弹窗永不弹出（Radian 审查 #1）。
    async fn screenshot_preprobe(&self, budget: std::time::Duration) -> Option<CapturedFrame> {
        match tokio::time::timeout(budget, self.screenshot.capture(false)).await {
            Ok(Ok(path)) => validated_screenshot_frame(path).map(CapturedFrame::Png),
            Ok(Err(e)) => {
                tracing::debug!("screenshot non-interactive pre-probe failed: {e}");
                None
            }
            Err(_) => {
                tracing::debug!("screenshot non-interactive pre-probe timed out; escalate");
                None
            }
        }
    }

    /// 建立（或复用）ScreenCast 流会话，返回可复用的会话句柄。
    ///
    /// `interactive=false` 且无 `restore_token` 时直接返回 `None`（portal 无
    /// 免弹窗选项，建立会话会弹授权窗）；有 token 则尝试静默恢复。建立失败
    /// （token 过期 / portal 不可用）返回 `None`，由调用方降级。
    async fn ensure_screencast_session(
        &self,
        target: CaptureTarget,
        interactive: bool,
    ) -> Option<std::sync::Arc<ScreenCastCapture>> {
        if !self.screencast_ok {
            return None;
        }
        let has_token = self
            .token_store
            .as_ref()
            .and_then(|s| s.get_restore_token())
            .is_some();
        if !(interactive || has_token) {
            return None;
        }
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            let opts = self.build_screencast_options();
            match ScreenCastCapture::start_with_options(self.conn.clone(), target, &opts).await {
                Ok(session) => {
                    // Start 返回新 restore_token 时持久化（覆盖旧值，
                    // token 单次有效）。persist_mode 未授权则无 token。
                    if let Some(store) = &self.token_store {
                        if let Some(new_token) = &session.restore_token {
                            store.save_restore_token(Some(new_token.clone()));
                        }
                    }
                    *guard = Some(std::sync::Arc::new(session.capture));
                }
                Err(e) => {
                    // 静默恢复失败（token 过期/会话不可用）且非交互时静默降级；
                    // 交互时也继续尝试其它后端。
                    if interactive {
                        tracing::warn!("screencast start failed, degrade: {e}");
                    } else {
                        tracing::debug!("screencast restore/start failed (non-interactive): {e}");
                    }
                }
            }
        }
        guard.as_ref().cloned()
    }

    /// 非交互会话探测（doctor 用）：逐级真实验证/建立降级链中首个可用后端，
    /// 记录到 `active` 并返回。
    ///
    /// 不短路复用已记录的 `active`——已记录后端也可能已失效（ScreenCast 会话
    /// 可在两次 capture 间失效、X11 连接可断开），必须逐级重验，失败即清状态
    /// 并继续降级链。区别于 [`Self::capture`]：不产出帧/文件，全程
    /// `interactive=false`（不弹授权窗）。ScreenCast 仅在持有 `restore_token`
    /// 时尝试静默恢复；Screenshot 走无对话框全屏路径；X11 直捕一帧验证
    /// （全黑帧视为非可用——纯 Wayland 会话 XWayland root 无合成器内容）。
    ///
    /// 返回 `None` 表示无任何后端可非交互建立，并将 `active` 清空。
    pub async fn probe(&self) -> Option<ActiveBackend> {
        // L1: ScreenCast 静默恢复（无 token 会弹窗，跳过）。
        if let Some(s) = self
            .ensure_screencast_session(CaptureTarget::Monitor, false)
            .await
        {
            match s.capture_frame().await {
                Ok(_) => {
                    self.set_active(Some(ActiveBackend::ScreenCast));
                    return Some(ActiveBackend::ScreenCast);
                }
                Err(e) => {
                    tracing::debug!("screencast frame probe failed: {e}");
                    *self.session.lock().await = None;
                    self.set_active(None);
                }
            }
        }

        // L2: portal Screenshot（interactive=false 全屏、无对话框）。
        if self.screenshot_ok {
            match self.screenshot.capture(false).await {
                Ok(path) => {
                    // 探测产生的临时截图落盘（可能含敏感画面），验证后立即删除，
                    // 避免每次 doctor 运行累积残留文件。全黑/无效帧判非可用
                    // （与 X11 的 is_all_black 同口径）：portal Screenshot 返回
                    // 全空帧时不能据此报「选中 portal-screenshot」。
                    let usable = !is_png_black_or_invalid(&path);
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::debug!("probe screenshot cleanup failed {}: {e}", path.display());
                    }
                    if usable {
                        self.set_active(Some(ActiveBackend::ScreenshotPortal));
                        return Some(ActiveBackend::ScreenshotPortal);
                    }
                    tracing::warn!(
                        "portal Screenshot probe captured an all-black/invalid frame; \
                         degrading to next backend"
                    );
                }
                Err(e) => tracing::debug!("screenshot portal probe failed (non-interactive): {e}"),
            }
        }

        // L3: X11 原生（全黑帧判非可用——XWayland root 无内容）。
        if self.x11_present {
            match self
                .x11
                .get_or_try_init(|| async { X11Capture::connect() })
                .await
            {
                Ok(cap) => match cap.capture_frame().await {
                    Ok(frame) if !is_all_black(&frame) => {
                        self.set_active(Some(ActiveBackend::X11));
                        return Some(ActiveBackend::X11);
                    }
                    Ok(_) => tracing::warn!(
                        "X11 probe captured an all-black frame (XWayland root without compositor \
                         content); portal authorization required"
                    ),
                    Err(e) => tracing::debug!("x11 capture frame probe failed: {e}"),
                },
                Err(e) => tracing::debug!("x11 capture connect probe failed: {e}"),
            }
        }

        // 逐级验证均失败：清空 stale active，doctor 据 None 渲染不可用/需授权。
        self.set_active(None);
        None
    }

    /// 构造 ScreenCast 建会话选项：尝试 restore_token 恢复 + persist_mode 持久化。
    ///
    /// 委托 [`screencast_options_for`]——纯函数，可单测。
    fn build_screencast_options(&self) -> ScreenCastOptions {
        screencast_options_for(self.token_store.as_deref())
    }

    /// 关闭持有的 ScreenCast 会话（daemon 退出前调用）。
    pub async fn shutdown(&self) {
        if let Some(s) = self.session.lock().await.take() {
            let _ = s.close_session().await;
        }
    }
}

/// 从 `TokenStore` 构造 ScreenCast 建会话选项（纯函数，可单测）。
///
/// - 有 `token_store` 且存有 `restore_token`：传入以尝试静默恢复。
/// - 有 `token_store`：设 `persist_mode = UntilRevoked` 使 Start 返回新 token。
/// - 无 `token_store`：不设 persist_mode（portal 默认 0），每次弹窗。
pub fn screencast_options_for(token_store: Option<&dyn TokenStore>) -> ScreenCastOptions {
    let mut opts = ScreenCastOptions::default();
    if let Some(store) = token_store {
        opts.restore_token = store.get_restore_token();
        opts.persist_mode = Some(PersistMode::UntilRevoked);
    }
    opts
}

#[async_trait]
impl DesktopComponent for CaptureDispatcher {
    fn name(&self) -> &'static str {
        "capture-dispatcher"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Capture
    }

    fn is_available(&self) -> bool {
        self.screencast_ok || self.screenshot_ok || self.x11_present
    }

    async fn health(&self) -> ComponentHealth {
        if !self.screencast_ok && !self.screenshot_ok && !self.x11_present {
            return ComponentHealth::Unavailable;
        }
        match self.selected_backend() {
            Some(_b) => ComponentHealth::Healthy,
            None => ComponentHealth::Degraded(format!(
                "no session yet; backends probe: {:?}",
                self.available_backends()
            )),
        }
    }
}

#[async_trait]
impl CaptureComponent for CaptureDispatcher {
    async fn active_backend(&self) -> Result<&'static str> {
        self.selected_backend()
            .map(ActiveBackend::name)
            .ok_or_else(|| {
                AgentShellError::BackendUnavailable("no capture backend activated yet".into())
            })
    }
}

/// capture 组件 doctor 行（daemon doctor 用）。
///
/// 先触发一次非交互会话探测（[`CaptureDispatcher::probe`]）——门户会话
/// 惰性建立，未探测前 `selected_backend()` 恒 `None`，doctor 只能报
/// 「候选」而无真实可用状态。探测后据实际选中的后端渲染；探测失败时复用
/// [`portal_fallback`] 决策，Wayland 无授权明示「已拒绝 x11 兜底」而非
/// 笼统「探测未就绪」（TSI-3075）。
pub async fn doctor_line(dispatcher: Option<&CaptureDispatcher>) -> String {
    const LABEL: &str = "截图捕获";
    match dispatcher {
        None => format!("✗ {LABEL:<12}: 不可用（无 portal 且无原生 X11 会话，TTY？）"),
        Some(d) => {
            let active = d.probe().await;
            let fallback = portal_fallback(d.wayland_session, d.x11_present);
            render_capture_doctor_line(LABEL, &d.available_backends(), active, fallback)
        }
    }
}

/// 渲染 capture doctor 行（纯函数，可单测）。
///
/// `active` 为 [`CaptureDispatcher::probe`] 的探测结果：`Some` = 已建立
/// 真实会话；`None` = 非交互探测失败。失败去向复用 [`portal_fallback`]
/// 决策：Wayland 下 portal 是唯一授权闸门，降级 x11 会静默抓 XWayland
/// root（越权），故「已拒绝 x11 兜底」——但 [`CaptureDispatcher::probe`]
/// 不携带具体失败原因（未授权、传输错误、黑帧皆可能），故不武断「无授权」，
/// 仅保留「已拒绝 x11 兜底」信号（TSI-3075）。候选链无 `x11-mit-shm`（纯
/// Wayland，无 XWayland）时无兜底可拒，如实报无可用后端。
fn render_capture_doctor_line(
    label: &str,
    backends: &[&'static str],
    active: Option<ActiveBackend>,
    fallback: PortalFallback,
) -> String {
    let chain = backends.join(" → ");
    match active {
        Some(b) => format!("✓ {label:<12}: {chain}（选中 {}）", b.name()),
        None => match fallback {
            // Wayland + XWayland 候选链含 x11：portal 是唯一授权闸门，降级
            // x11 会静默抓 XWayland root（越权）——portal_fallback 已判定拒绝。
            // probe() 不携带失败原因，不武断「无授权」，仅保留「已拒绝 x11
            // 兜底」信号（TSI-3075 + 审查）。
            PortalFallback::Fail(AgentShellError::Permission(_))
                if backends.contains(&ActiveBackend::X11.name()) =>
            {
                format!(
                    "⚠ {label:<12}: 候选 {chain}（Wayland 下 portal 未就绪/未授权，已拒绝 x11 兜底）"
                )
            }
            // 纯 Wayland（无 XWayland）候选链无 x11：无兜底可拒，如实报无可用后端。
            PortalFallback::Fail(AgentShellError::Permission(_)) => {
                format!(
                    "⚠ {label:<12}: 候选 {chain}（Wayland 下 portal 未就绪/未授权，无可用后端）"
                )
            }
            // 原生 X11：x11 兜底可用，含 portal 候选时非交互探测未就绪
            // 多半仍是 portal 未授权；仅 X11 候选则是「无可用后端」。
            _ if backends.iter().any(|b| b.starts_with("portal-")) => {
                format!("⚠ {label:<12}: 候选 {chain}（非交互探测未就绪，需 portal 交互授权）")
            }
            // 无任何可用后端（原生 X11 探测失败 / 无 x11 无 portal）。
            _ => format!("⚠ {label:<12}: 候选 {chain}（非交互探测失败，无可用后端）"),
        },
    }
}

/// 判定帧是否全黑——纯 Wayland + XWayland 会话下 X11 兜底直捕 root 的典型产物。
///
/// 仅检查 RGB 通道（忽略 Bgrx 的填充字节 / Rgba 的 alpha），避免把
/// 「alpha=255 的透明黑」误判为非黑。Rgb565/Clut8 无填充，逐字节比较。
fn is_all_black(frame: &Frame) -> bool {
    match frame.format {
        PixelFormat::Bgra | PixelFormat::Bgrx | PixelFormat::Rgba => frame
            .data
            .chunks(4)
            .all(|px| px.len() < 4 || px[..3].iter().all(|&b| b == 0)),
        _ => frame.data.iter().all(|&b| b == 0),
    }
}

/// 解码输出缓冲上限。portal Screenshot 单帧全屏 PNG：8K RGBA ≈ 127 MiB，
/// 留 ~2× 余量。伪造/损坏 PNG 的 IHDR 可声明超大宽高，`output_buffer_size()`
/// 仅按 isize::MAX 封顶；分配前据此封顶，超限判非可用，避免数 GB memset
/// 使 daemon 在 doctor 探测时 OOM。
const PNG_DECODE_MAX_BYTES: usize = 256 * 1024 * 1024;

/// 判定 Screenshot 落盘 PNG 是否「全黑或无效」——与 [`is_all_black`] 同口径：
/// 解码失败（空文件 / 损坏 / 非 PNG / 截断 / bad CRC）或所有像素 RGB 通道
/// 全 0 均视为非可用。
///
/// 仅判 RGB 通道（忽略 alpha），避免把「透明黑」误判为非黑。portal Screenshot
/// 可用性必须以帧内容为准，而非仅「文件生成成功」——本地 portal 全空帧
/// （全 0 字节）能落盘却不可用，须据此降级。
fn is_png_black_or_invalid(path: &std::path::Path) -> bool {
    use png::{Decoder, Transformations};

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return true,
    };
    let mut decoder = Decoder::new(std::io::BufReader::new(file));
    // 归一化为 8-bit 颜色（palette/tRNS/低 bit 深度统一展开），简化通道判定。
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = match decoder.read_info() {
        Ok(r) => r,
        Err(_) => return true,
    };
    let Some(buf_size) = reader.output_buffer_size() else {
        return true;
    };
    // 分配前封顶：伪造 IHDR 可声明超大宽高，`output_buffer_size()` 仅以
    // isize::MAX 封顶，直接 memset 会 OOM。超限判非可用。
    if buf_size > PNG_DECODE_MAX_BYTES {
        tracing::warn!(
            buf_size,
            "portal Screenshot PNG decode buffer exceeds cap; treating as invalid"
        );
        return true;
    }
    // 逐行流式解码，不物化整帧缓冲（8K 单帧 25MB+，debug 下解压/逐像素
    // 遍历无优化，全帧物化会放大每次 doctor 探测的开销）。命中非黑行只
    // 标记、不提前返回——必须读尽全部行再 `finish()` 校验 trailer，否则
    // 首行非黑但截断/损坏（bad CRC、IDAT 截断、缺 IEND）的 PNG 会漏过
    // 校验，以 `CapturedFrame::Png` 交给 daemon。
    let (color_type, _depth) = reader.output_color_type();
    let mut has_non_black = false;
    loop {
        match reader.next_row() {
            Ok(Some(row)) => {
                if row_has_non_black(row.data(), color_type) {
                    has_non_black = true;
                }
            }
            Ok(None) => break,
            Err(_) => return true,
        }
    }
    // 读尽 IDAT 后 IEND 仍未消费：`finish()` 读至输入末尾并校验 trailer
    // CRC，截断（缺 IEND / IDAT 不完整）或坏 CRC 在此返回 Err，判不可用。
    if reader.finish().is_err() {
        return true;
    }
    !has_non_black
}

/// 单行是否含非黑像素（忽略 alpha 通道，与 [`is_all_black`] 同口径）。
///
/// `normalize_to_color8` 展开后每个像素为 1/2/3/4 字节：Rgb(3)/Rgba(4)/
/// Grayscale(1)/GrayscaleAlpha(2)。`Indexed` 经 EXPAND 后不会出现，
/// 保留防御分支（1 字节/像素）。
fn row_has_non_black(data: &[u8], color_type: png::ColorType) -> bool {
    use png::ColorType;
    let (bpp, color_channels) = match color_type {
        ColorType::Rgb => (3, 3),
        ColorType::Rgba => (4, 3),
        ColorType::Grayscale => (1, 1),
        ColorType::GrayscaleAlpha => (2, 1),
        ColorType::Indexed => (1, 1),
    };
    data.chunks(bpp)
        .any(|px| px.iter().take(color_channels).any(|&b| b != 0))
}

/// 预算封顶：在 `budget` 内执行 `fut`，超时返回 `Timeout`。
///
/// 抽成可注入预算的辅助函数以便单测（用极小 `budget` 确定性触发超时分支）——
/// 与 [`CaptureDispatcher::capture_portal`] / 端到端截止解耦，测试不依赖真实
/// portal/X11（Radian 审查 #3）。超时后的去向（降级 x11 或快速失败）由调用方
/// 的 [`portal_fallback`] 决策（TSI-3054）。
async fn capture_with_budget<F>(budget: std::time::Duration, fut: F) -> Result<CapturedFrame>
where
    F: Future<Output = Result<CapturedFrame>>,
{
    match tokio::time::timeout(budget, fut).await {
        Ok(res) => res,
        Err(_) => Err(AgentShellError::Timeout(format!(
            "capture exceeded {}s budget",
            budget.as_secs()
        ))),
    }
}
/// portal 失败后的降级去向（纯函数决策结果）。
#[derive(Debug)]
enum PortalFallback {
    /// 原生 X11 会话：降级 x11-mit-shm 抓屏。
    X11,
    /// 快速失败：Wayland 下拒绝静默抓 XWayland root，或没有任何兜底后端。
    Fail(AgentShellError),
}

/// portal 失败后的降级决策（纯函数，可单测）。
///
/// - Wayland 会话：portal 是唯一授权闸门，无授权降级 x11 会静默抓取
///   XWayland root（越权），必须快速失败并给出明确报错（TSI-3054 审查 #2）。
/// - 原生 X11 会话且 `x11_present`：降级 x11-mit-shm。
/// - 无任何兜底：快速失败。
fn portal_fallback(wayland: bool, x11_present: bool) -> PortalFallback {
    if wayland {
        PortalFallback::Fail(AgentShellError::Permission(
            "portal authorization unavailable on Wayland; refusing to capture the screen \
             (X11 fallback would grab XWayland root without authorization)"
                .into(),
        ))
    } else if x11_present {
        PortalFallback::X11
    } else {
        PortalFallback::Fail(AgentShellError::BackendUnavailable(
            "no capture backend available (portal unavailable/denied, no native X11 session)"
                .into(),
        ))
    }
}

/// 是否 Wayland 会话：`WAYLAND_DISPLAY` 或 `WAYLAND_SOCKET` 存在。
///
/// 与 `components/clipboard`、`daemon` 的 `is_wayland_session` 同口径
/// （`var_os().is_some()`）；抽出纯函数版 [`is_wayland_session_with`] 以便单测。
fn is_wayland_session() -> bool {
    is_wayland_session_with(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_SOCKET").as_deref(),
    )
}

/// 纯函数版 Wayland 会话判定（可单测，不触碰进程环境变量）。
fn is_wayland_session_with(
    wayland_display: Option<&std::ffi::OsStr>,
    wayland_socket: Option<&std::ffi::OsStr>,
) -> bool {
    wayland_display.is_some() || wayland_socket.is_some()
}

/// 校验 portal Screenshot 落盘产物是否可用（纯函数，可单测）：非 PNG（如
/// xdg-desktop-portal-dde 委托 KWin 落盘的 JPEG）、损坏或全黑（与
/// [`is_png_black_or_invalid`] 同口径）一律判不可用并清理临时文件，返回
/// `None`；可用则返回 `Some(path)`。调用方据 `None` 降级 x11 / 快速失败。
fn validated_screenshot_frame(path: std::path::PathBuf) -> Option<std::path::PathBuf> {
    if is_png_black_or_invalid(&path) {
        let _ = std::fs::remove_file(&path);
        None
    } else {
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// 内存 TokenStore——验证 CaptureDispatcher 的 token 注入/读取逻辑。
    struct InMemoryTokenStore {
        token: Mutex<Option<String>>,
    }

    impl TokenStore for InMemoryTokenStore {
        fn get_restore_token(&self) -> Option<String> {
            self.token.lock().clone()
        }

        fn save_restore_token(&self, token: Option<String>) {
            *self.token.lock() = token;
        }
    }

    #[test]
    fn token_store_save_and_get() {
        let store = InMemoryTokenStore {
            token: Mutex::new(None),
        };
        assert!(store.get_restore_token().is_none());
        store.save_restore_token(Some("tok1".into()));
        assert_eq!(store.get_restore_token().as_deref(), Some("tok1"));
        store.save_restore_token(None);
        assert!(store.get_restore_token().is_none());
    }

    /// portal 探测预算 = 6s、端到端截止 = 7s：两者都必须小于 QA 验收的 8s
    /// 上限与旧行为叠加的 2×SCREENSHOT_TIMEOUT + SCREENCAST_TIMEOUT（≈27.8s）
    /// 及旧的 15s 交互预算——锚定「无 portal 授权 8s 内降级 x11 或快速失败」
    /// （TSI-3054 审查 #1）。
    #[test]
    fn capture_budgets_leave_margin_for_x11_fallback() {
        assert_eq!(PORTAL_PROBE_BUDGET, std::time::Duration::from_secs(6));
        assert_eq!(CAPTURE_END_TO_END_BUDGET, std::time::Duration::from_secs(7));
        assert!(
            CAPTURE_END_TO_END_BUDGET < std::time::Duration::from_secs(8),
            "end-to-end deadline must stay under the QA 8s timeout (leave margin)"
        );
        assert!(
            PORTAL_PROBE_BUDGET < CAPTURE_END_TO_END_BUDGET,
            "portal probe must leave room for the x11 fallback within the end-to-end deadline"
        );
        let old_stacked =
            portal_screenshot::SCREENSHOT_TIMEOUT * 2 + portal_screencast::SCREENCAST_TIMEOUT;
        assert!(PORTAL_PROBE_BUDGET < old_stacked);
        assert!(
            PORTAL_PROBE_BUDGET
                < portal_screencast::SCREENCAST_TIMEOUT
                    + portal_screenshot::SCREENSHOT_TIMEOUT_INTERACTIVE
        );
    }

    /// portal 失败后的降级决策（TSI-3054 审查 #2/#3，纯函数确定性测）：
    /// 原生 X11 会话降级 x11；Wayland 会话快速失败（拒绝静默抓 XWayland
    /// root）；无 x11 兜底时快速失败并给出明确报错。
    #[test]
    fn portal_fallback_decides_x11_or_fail() {
        // 原生 X11 + DISPLAY 可达 → 降级 x11。
        assert!(matches!(portal_fallback(false, true), PortalFallback::X11));
        // Wayland（即使 XWayland DISPLAY 存在）→ 快速失败，不静默抓屏。
        match portal_fallback(true, true) {
            PortalFallback::Fail(AgentShellError::Permission(msg)) => {
                assert!(msg.contains("Wayland"), "unexpected msg: {msg}");
            }
            other => panic!("expected Permission fail on Wayland, got {other:?}"),
        }
        // 无任何兜底（x11_present=false）→ 快速失败。
        match portal_fallback(false, false) {
            PortalFallback::Fail(AgentShellError::BackendUnavailable(msg)) => {
                assert!(
                    msg.contains("no native X11 session"),
                    "unexpected msg: {msg}"
                );
            }
            other => panic!("expected BackendUnavailable, got {other:?}"),
        }
    }

    /// Wayland 会话判定：任一 env（`WAYLAND_DISPLAY` / `WAYLAND_SOCKET`）存在即真。
    #[test]
    fn wayland_session_detection() {
        use std::ffi::OsStr;
        assert!(!is_wayland_session_with(None, None));
        assert!(is_wayland_session_with(Some(OsStr::new("wayland-0")), None));
        assert!(is_wayland_session_with(None, Some(OsStr::new("12"))));
        assert!(is_wayland_session_with(
            Some(OsStr::new("wayland-0")),
            Some(OsStr::new("12"))
        ));
    }

    /// 预算封顶行为：快速 future 原样通过；慢 future 被中止并归一到 `Timeout`
    /// ——覆盖「timeout 中止 → Timeout」分支（Radian 审查 #3，可注入预算）。
    #[tokio::test]
    async fn capture_with_budget_passes_fast_and_times_out_slow() {
        let fast = capture_with_budget(
            std::time::Duration::from_secs(1),
            std::future::ready(Ok(CapturedFrame::Png("fast.png".into()))),
        )
        .await;
        assert!(fast.is_ok(), "fast future must pass through: {fast:?}");

        let slow = capture_with_budget(std::time::Duration::from_millis(10), async {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Ok(CapturedFrame::Png("slow.png".into()))
        })
        .await;
        assert!(
            matches!(slow, Err(AgentShellError::Timeout(_))),
            "slow future must be aborted into Timeout: {slow:?}"
        );
    }

    /// 产物校验：黑/无效帧 → `None` 并清理临时文件；有效帧 → `Some`
    /// ——覆盖「预探测失败 → 升级弹窗」与「非 PNG 产物 → 降级」的判定分支。
    #[test]
    fn validated_screenshot_frame_rejects_black_and_keeps_valid() {
        let dir = tempfile::tempdir().unwrap();
        // 全黑 PNG → 不可用并清理。
        let black = write_png(
            dir.path(),
            "black.png",
            png::ColorType::Rgb,
            1,
            1,
            &[0, 0, 0],
        );
        assert!(validated_screenshot_frame(black.clone()).is_none());
        assert!(
            !black.exists(),
            "black frame must be removed after rejection"
        );
        // 非黑 PNG → 可用保留。
        let red = write_png(
            dir.path(),
            "red.png",
            png::ColorType::Rgb,
            1,
            1,
            &[255, 0, 0],
        );
        assert!(validated_screenshot_frame(red.clone()).is_some());
        assert!(red.exists(), "valid frame must be kept");
    }

    /// JPEG 产物回归锚定：DDE 的 xdg-desktop-portal-dde 委托 KWin 落盘
    /// JPEG（实测 /tmp/kwin_screenshot_*.jpg），portal 却返回「成功」。
    /// 非 PNG 产物必须判为不可用并清理，由调用方降级 x11，而非把 JPEG 当
    /// PNG 解析报「not a PNG file」（TSI-3084）。
    #[test]
    fn validated_screenshot_frame_rejects_jpeg_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg = dir.path().join("kwin_screenshot.jpg");
        // JPEG SOI（FF D8 FF E0）+ 非全零填充：非 PNG 魔数、非「空/全 0」。
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.extend(std::iter::repeat(0x11).take(1024));
        std::fs::write(&jpeg, &bytes).unwrap();
        assert!(is_png_black_or_invalid(&jpeg));
        assert!(validated_screenshot_frame(jpeg.clone()).is_none());
        assert!(
            !jpeg.exists(),
            "JPEG artifact must be removed after rejection"
        );
    }

    #[test]
    fn screencast_options_with_token_and_stored_restore_token() {
        // 有 token_store + 已存 token → options 应携带 restore_token
        // 和 persist_mode=UntilRevoked。
        let store = InMemoryTokenStore {
            token: Mutex::new(Some("saved_token".into())),
        };
        let opts = screencast_options_for(Some(&store));
        assert_eq!(opts.restore_token.as_deref(), Some("saved_token"));
        assert_eq!(opts.persist_mode, Some(PersistMode::UntilRevoked));
    }

    #[test]
    fn screencast_options_with_token_store_but_no_token() {
        // 有 token_store 但无已存 token → persist_mode 设但 restore_token=None。
        // portal 收到 persist_mode 无 restore_token → 首次授权弹窗，返回新 token。
        let store = InMemoryTokenStore {
            token: Mutex::new(None),
        };
        let opts = screencast_options_for(Some(&store));
        assert!(opts.restore_token.is_none());
        assert_eq!(opts.persist_mode, Some(PersistMode::UntilRevoked));
    }

    #[test]
    fn screencast_options_without_token_store() {
        // 无 token_store → options 应为默认（无 restore_token、无 persist_mode）。
        // 这锚定无持久化时回退到弹窗路径的行为。
        let opts = screencast_options_for(None);
        assert!(opts.restore_token.is_none());
        assert!(opts.persist_mode.is_none());
    }

    #[test]
    fn is_all_black_detects_black_frames() {
        // Bgrx 全黑（RGB=0）应为黑，即使填充字节非 0。
        let black_bgrx = Frame {
            data: vec![0, 0, 0, 0, 0, 0, 0, 0],
            width: 2,
            height: 1,
            stride: 8,
            format: PixelFormat::Bgrx,
        };
        assert!(is_all_black(&black_bgrx));
        // Rgba 透明黑（RGB=0、alpha=255）仍应判黑。
        let transparent_black_rgba = Frame {
            data: vec![0, 0, 0, 255, 0, 0, 0, 255],
            width: 2,
            height: 1,
            stride: 8,
            format: PixelFormat::Rgba,
        };
        assert!(is_all_black(&transparent_black_rgba));
        // 非黑（任一 RGB 通道非 0）应判非黑。
        let red_bgrx = Frame {
            data: vec![0, 0, 1, 0],
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Bgrx,
        };
        assert!(!is_all_black(&red_bgrx));
        // Rgb565 全零为黑。
        let black_rgb565 = Frame {
            data: vec![0, 0, 0, 0],
            width: 2,
            height: 1,
            stride: 4,
            format: PixelFormat::Rgb565,
        };
        assert!(is_all_black(&black_rgb565));
    }

    /// 写一个 8-bit PNG 到 `dir` 下，返回文件路径。
    fn write_png(
        dir: &std::path::Path,
        name: &str,
        color: png::ColorType,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        path
    }

    /// 构造 IHDR 声明超大宽高的最小 PNG（含合法 CRC），用于验证解码前封顶。
    fn write_oversized_png(
        dir: &std::path::Path,
        name: &str,
        width: u32,
        height: u32,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut bytes = vec![137, 80, 78, 71, 13, 10, 26, 10];
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        // depth 8 / RGB / compression / filter / interlace
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        push_png_chunk(&mut bytes, b"IHDR", &ihdr);
        push_png_chunk(&mut bytes, b"IDAT", &[]);
        push_png_chunk(&mut bytes, b"IEND", &[]);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// 追加一个 PNG chunk：length + type + data + CRC32(type || data)。
    fn push_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_input = Vec::with_capacity(4 + data.len());
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc32fast::hash(&crc_input).to_be_bytes());
    }

    #[test]
    fn is_png_black_or_invalid_detects_black_frames() {
        let dir = tempfile::tempdir().unwrap();
        // 全黑 RGB。
        let rgb_black = write_png(
            dir.path(),
            "rgb_black.png",
            png::ColorType::Rgb,
            2,
            2,
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        assert!(is_png_black_or_invalid(&rgb_black));
        // 不透明黑 RGBA（RGB=0、alpha=255）→ 忽略 alpha 仍判黑。
        let opaque_black = write_png(
            dir.path(),
            "opaque_black.png",
            png::ColorType::Rgba,
            1,
            2,
            &[0, 0, 0, 255, 0, 0, 0, 255],
        );
        assert!(is_png_black_or_invalid(&opaque_black));
        // 透明黑 RGBA（RGB=0、alpha=0）→ 忽略 alpha 仍判黑。
        let transparent_black = write_png(
            dir.path(),
            "transparent_black.png",
            png::ColorType::Rgba,
            1,
            2,
            &[0, 0, 0, 0, 0, 0, 0, 0],
        );
        assert!(is_png_black_or_invalid(&transparent_black));
        // 全黑灰度。
        let gray_black = write_png(
            dir.path(),
            "gray_black.png",
            png::ColorType::Grayscale,
            2,
            1,
            &[0, 0],
        );
        assert!(is_png_black_or_invalid(&gray_black));
        // 全黑灰度+alpha（gray=0、alpha=255）→ 忽略 alpha 仍判黑。
        let gray_alpha_black = write_png(
            dir.path(),
            "gray_alpha_black.png",
            png::ColorType::GrayscaleAlpha,
            2,
            1,
            &[0, 255, 0, 255],
        );
        assert!(is_png_black_or_invalid(&gray_alpha_black));
    }

    #[test]
    fn is_png_black_or_invalid_rejects_non_black_frames() {
        let dir = tempfile::tempdir().unwrap();
        // 非黑 RGB。
        let red = write_png(
            dir.path(),
            "red.png",
            png::ColorType::Rgb,
            1,
            1,
            &[255, 0, 0],
        );
        assert!(!is_png_black_or_invalid(&red));
        // 非黑灰度。
        let gray = write_png(
            dir.path(),
            "gray.png",
            png::ColorType::Grayscale,
            1,
            1,
            &[128],
        );
        assert!(!is_png_black_or_invalid(&gray));
        // 非黑灰度+alpha（gray=128、alpha=0）→ 灰度通道非 0，判非黑。
        let gray_alpha = write_png(
            dir.path(),
            "gray_alpha.png",
            png::ColorType::GrayscaleAlpha,
            1,
            1,
            &[128, 0],
        );
        assert!(!is_png_black_or_invalid(&gray_alpha));
    }

    #[test]
    fn is_png_black_or_invalid_detects_non_black_after_black_rows() {
        // 逐行流式解码的跨行继续扫描路径：前两行全黑、第三行出现非黑像素，
        // 应短路判非黑（`row_has_non_black == false` 后继续读下一行直至命中）。
        let dir = tempfile::tempdir().unwrap();
        let top_black_bottom_red = write_png(
            dir.path(),
            "top_black_bottom_red.png",
            png::ColorType::Rgb,
            2,
            3,
            &[
                0, 0, 0, 0, 0, 0, // 第 1 行：黑
                0, 0, 0, 0, 0, 0, // 第 2 行：黑
                255, 0, 0, 255, 0, 0, // 第 3 行：红
            ],
        );
        assert!(!is_png_black_or_invalid(&top_black_bottom_red));
    }

    #[test]
    fn is_png_black_or_invalid_detects_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        // 全 0 字节（非 PNG）。
        let raw = dir.path().join("raw.bin");
        std::fs::write(&raw, vec![0u8; 1024]).unwrap();
        assert!(is_png_black_or_invalid(&raw));
        // 不存在。
        assert!(is_png_black_or_invalid(&dir.path().join("missing.png")));
    }

    #[test]
    fn is_png_black_or_invalid_rejects_oversized_png() {
        // IHDR 声明 100000×100000 RGB（≈ 28 GB），解码前封顶应判非可用，
        // 而非触发数 GB memset 导致 OOM。
        let dir = tempfile::tempdir().unwrap();
        let huge = write_oversized_png(dir.path(), "huge.png", 100_000, 100_000);
        assert!(is_png_black_or_invalid(&huge));
    }

    /// 首行非黑但尾部截断/损坏的 PNG 必须判不可用——首个非黑行即短路返回
    /// 「可用」会让 bad CRC / IDAT 截断 / 缺 IEND 的产物漏过校验，以
    /// `CapturedFrame::Png` 交给 daemon（Radian 审查 / sourcery-ai 反馈）。
    #[test]
    fn is_png_black_or_invalid_rejects_corrupt_non_black_png() {
        let dir = tempfile::tempdir().unwrap();
        // 2×2 RGB：首行红（非黑）、次行黑。IDAT 完整可解出首行，损坏在尾部。
        let src = write_png(
            dir.path(),
            "src.png",
            png::ColorType::Rgb,
            2,
            2,
            &[255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        let valid = std::fs::read(&src).unwrap();

        // (1) 缺 IEND：截掉末尾 12 字节（IEND chunk）。
        let missing_iend = dir.path().join("missing_iend.png");
        std::fs::write(&missing_iend, &valid[..valid.len() - 12]).unwrap();
        assert!(is_png_black_or_invalid(&missing_iend));

        // (2) IDAT 截断：再截深一层，砍掉 IEND + 一段 IDAT 数据。
        let truncated_idat = dir.path().join("truncated_idat.png");
        std::fs::write(&truncated_idat, &valid[..valid.len() - 24]).unwrap();
        assert!(is_png_black_or_invalid(&truncated_idat));

        // (3) 坏 trailer CRC：翻转 IEND 的 CRC 末字节（文件末尾 4 字节）。
        let bad_crc = dir.path().join("bad_crc.png");
        let mut bytes = valid.clone();
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF;
        std::fs::write(&bad_crc, &bytes).unwrap();
        assert!(is_png_black_or_invalid(&bad_crc));
    }

    #[test]
    fn render_capture_doctor_line_with_active_backend() {
        // 已建立真实会话（probe 返回 Some）→ ✓ + 选中后端名。
        let line = render_capture_doctor_line(
            "截图捕获",
            &["portal-screencast", "portal-screenshot", "x11-mit-shm"],
            Some(ActiveBackend::ScreenCast),
            PortalFallback::X11,
        );
        assert!(
            line.starts_with("✓ 截图捕获"),
            "selected backend must render ✓: {line}"
        );
        assert!(line.contains("portal-screencast → portal-screenshot → x11-mit-shm"));
        assert!(line.contains("选中 portal-screencast"), "{line}");
    }

    #[test]
    fn render_capture_doctor_line_without_active_backend_native_x11() {
        // 原生 X11 会话、含 portal 候选（probe 返回 None）→ ⚠ 提示需 portal
        // 交互授权，不再用旧的「尚未实际建立会话」（探测已真实执行过）。
        let line = render_capture_doctor_line(
            "截图捕获",
            &["portal-screencast", "portal-screenshot"],
            None,
            PortalFallback::X11,
        );
        assert!(line.starts_with("⚠ 截图捕获"), "{line}");
        assert!(line.contains("候选 portal-screencast → portal-screenshot"));
        assert!(line.contains("需 portal 交互授权"), "{line}");
    }

    #[test]
    fn render_capture_doctor_line_without_active_backend_wayland_refuses_x11() {
        // Wayland + XWayland 候选链含 x11（probe 返回 None、portal_fallback
        // 拒绝 x11 兜底）→ 明示「已拒绝 x11 兜底」；probe() 不携带失败原因，
        // 故不武断「无授权」，只报「portal 未就绪/未授权」（TSI-3075 + 审查）。
        let line = render_capture_doctor_line(
            "截图捕获",
            &["portal-screencast", "portal-screenshot", "x11-mit-shm"],
            None,
            PortalFallback::Fail(AgentShellError::Permission(
                "portal authorization unavailable on Wayland".into(),
            )),
        );
        assert!(line.starts_with("⚠ 截图捕获"), "{line}");
        assert!(line.contains("候选 portal-screencast → portal-screenshot → x11-mit-shm"));
        assert!(line.contains("Wayland 下 portal 未就绪/未授权"), "{line}");
        assert!(line.contains("已拒绝 x11 兜底"), "{line}");
    }

    #[test]
    fn render_capture_doctor_line_without_active_backend_pure_wayland_no_x11() {
        // 纯 Wayland（无 XWayland，候选链无 x11-mit-shm）→ 无兜底可拒，
        // 如实报「无可用后端」而非「已拒绝 x11 兜底」（Radian 审查）。
        let line = render_capture_doctor_line(
            "截图捕获",
            &["portal-screencast", "portal-screenshot"],
            None,
            PortalFallback::Fail(AgentShellError::Permission(
                "portal authorization unavailable on Wayland".into(),
            )),
        );
        assert!(line.starts_with("⚠ 截图捕获"), "{line}");
        assert!(line.contains("候选 portal-screencast → portal-screenshot"));
        assert!(line.contains("Wayland 下 portal 未就绪/未授权"), "{line}");
        assert!(line.contains("无可用后端"), "{line}");
        assert!(!line.contains("已拒绝 x11 兜底"), "{line}");
    }

    #[test]
    fn render_capture_doctor_line_without_active_backend_x11_only() {
        // 候选集仅含 X11（无 portal 后端）→ 探测失败与授权无关，须渲染
        // 「无可用后端」而非「需 portal 交互授权」（Radian 建议 3）。
        let line =
            render_capture_doctor_line("截图捕获", &["x11-mit-shm"], None, PortalFallback::X11);
        assert!(line.starts_with("⚠ 截图捕获"), "{line}");
        assert!(line.contains("候选 x11-mit-shm"), "{line}");
        assert!(line.contains("无可用后端"), "{line}");
        assert!(!line.contains("portal"), "{line}");
    }
}
