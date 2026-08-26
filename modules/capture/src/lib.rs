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

pub use cache::CaptureCache;
pub use portal_screencast::{CaptureTarget, Frame, PixelFormat, ScreenCastCapture};
pub use portal_screenshot::{ScreenshotPortal, SCREENSHOT_MAX_ATTEMPTS, SCREENSHOT_TIMEOUT};
pub use x11::X11Capture;

/// 组件名（doctor 报告用）。
pub const COMPONENT_NAME: &str = "capture";

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
/// 无副作用探测（bus 上 portal 是否可达、DISPLAY 是否存在）；实际
/// 后端在首次 `capture` 时惰性建立并记录到 `active`。
pub struct CaptureDispatcher {
    conn: zbus::Connection,
    screencast_ok: bool,
    screenshot_ok: bool,
    screenshot: ScreenshotPortal,
    x11_present: bool,
    active: std::sync::Mutex<Option<ActiveBackend>>,
    /// 已建立的 ScreenCast 流会话（daemon 复用，避免反复弹窗 §21.22）。
    session: tokio::sync::Mutex<Option<std::sync::Arc<ScreenCastCapture>>>,
    /// X11 捕获器（惰性建连，daemon 复用连接——审查项 #5）。
    x11: tokio::sync::OnceCell<X11Capture>,
}

impl CaptureDispatcher {
    /// 按探测链装配。全部后端不可用返回 `None`（TTY 场景）。
    pub async fn assemble() -> Option<Self> {
        let conn = zbus::Connection::session().await.ok()?;
        let screencast_ok = ScreenCastCapture::available(&conn).await;
        let screenshot = ScreenshotPortal::with_connection(conn.clone());
        let screenshot_ok = screenshot.available().await;
        let x11_present = X11Capture::display_present();
        if !screencast_ok && !screenshot_ok && !x11_present {
            return None;
        }
        Some(Self {
            conn,
            screencast_ok,
            screenshot_ok,
            screenshot,
            x11_present,
            active: std::sync::Mutex::new(None),
            session: tokio::sync::Mutex::new(None),
            x11: tokio::sync::OnceCell::new(),
        })
    }

    /// 窗口直捕（X11-only；portal 无法定位 native window id）。
    /// 连接经 `OnceCell` 惰性建立并复用。
    pub async fn capture_window(&self, window: u32) -> Result<Frame> {
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

    /// 单帧捕获：ScreenCast 流式取最新帧 → 失败/不可用降级 Screenshot → 再降级 X11。
    ///
    /// ScreenCast 需用户弹窗授权且会话由本组件持有复用；`interactive=false`
    /// 时跳过需要弹窗的后端（无持久化会话时直接走 Screenshot/X11）。
    pub async fn capture(&self, target: CaptureTarget, interactive: bool) -> Result<CapturedFrame> {
        // L1: portal ScreenCast（流式，daemon 复用会话）。
        if interactive && self.screencast_ok {
            let mut guard = self.session.lock().await;
            if guard.is_none() {
                match ScreenCastCapture::start(self.conn.clone(), target).await {
                    Ok(s) => *guard = Some(std::sync::Arc::new(s)),
                    Err(e) => tracing::warn!("screencast start failed, degrade: {e}"),
                }
            }
            if let Some(s) = guard.as_ref() {
                match s.capture_frame().await {
                    Ok(frame) => {
                        self.set_active(Some(ActiveBackend::ScreenCast));
                        return Ok(CapturedFrame::Pixels(frame));
                    }
                    Err(e) => {
                        tracing::warn!("screencast frame failed, degrade: {e}");
                        // 会话失效即丢弃，下次重新走五步流程。
                        *guard = None;
                        self.set_active(None);
                    }
                }
            }
        }

        // L2: portal Screenshot。
        if self.screenshot.available().await {
            match self.screenshot.capture(interactive).await {
                Ok(path) => {
                    self.set_active(Some(ActiveBackend::ScreenshotPortal));
                    return Ok(CapturedFrame::Png(path));
                }
                Err(e) => tracing::warn!("screenshot portal failed, degrade: {e}"),
            }
        }

        // L3: X11 原生。
        if self.x11_present {
            let cap = X11Capture::connect().map_err(|e| {
                tracing::warn!("x11 capture unavailable: {e}");
                e
            })?;
            let frame = cap.capture_frame().await?;
            self.set_active(Some(ActiveBackend::X11));
            return Ok(CapturedFrame::Pixels(frame));
        }

        Err(AgentShellError::BackendUnavailable(
            "no capture backend available (portal ScreenCast/Screenshot unreachable, no DISPLAY)"
                .into(),
        ))
    }

    /// 关闭持有的 ScreenCast 会话（daemon 退出前调用）。
    pub async fn shutdown(&self) {
        if let Some(s) = self.session.lock().await.take() {
            let _ = s.close_session().await;
        }
    }
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

/// capture 组件 doctor 行（CLI collect_doctor 用）。
pub async fn doctor_line(dispatcher: Option<&CaptureDispatcher>) -> String {
    const LABEL: &str = "截图捕获";
    match dispatcher {
        None => format!("✗ {LABEL:<12}: 不可用（无 portal 且无 DISPLAY，TTY？）"),
        Some(d) => {
            let backends = d.available_backends().join(" → ");
            match d.selected_backend() {
                Some(b) => format!("✓ {LABEL:<12}: {}（选中 {}）", backends, b.name()),
                None => format!("⚠ {LABEL:<12}: 候选 {backends}（尚未实际建立会话）"),
            }
        }
    }
}
