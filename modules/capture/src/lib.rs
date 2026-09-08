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
/// 后端在首次 `capture` 时惰性建立并记录到 `active`。
///
/// `token_store` 注入后，ScreenCast 优先尝试用持久化的 `restore_token`
/// 静默恢复会话——恢复成功则无弹窗（无交互授权路径）。
pub struct CaptureDispatcher {
    conn: zbus::Connection,
    screencast_ok: bool,
    screenshot_ok: bool,
    screenshot: ScreenshotPortal,
    x11_present: bool,
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

    /// 单帧捕获：ScreenCast 流式取最新帧 → 失败/不可用降级 Screenshot → 再降级 X11。
    ///
    /// ScreenCast 会话优先尝试 `restore_token` 静默恢复（无弹窗）；
    /// `interactive=true` 时允许弹窗授权（无 token 或恢复失败时）；
    /// `interactive=false` 时仅当 `token_store` 含 `restore_token` 才尝试
    /// ScreenCast——无 token 则直接降级到 Screenshot/X11（避免弹窗）。
    pub async fn capture(&self, target: CaptureTarget, interactive: bool) -> Result<CapturedFrame> {
        // L1: portal ScreenCast（流式，daemon 复用会话）。
        if self.screencast_ok {
            // interactive=false 且无 restore_token 时跳过——portal 无免弹窗选项。
            let has_token = self
                .token_store
                .as_ref()
                .and_then(|s| s.get_restore_token())
                .is_some();
            if interactive || has_token {
                let mut guard = self.session.lock().await;
                if guard.is_none() {
                    let opts = self.build_screencast_options();
                    match ScreenCastCapture::start_with_options(self.conn.clone(), target, &opts)
                        .await
                    {
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
                            // 静默恢复失败（token 过期/会话不可用）且非交互
                            // 时静默降级；交互时也继续尝试其它后端。
                            if interactive {
                                tracing::warn!("screencast start failed, degrade: {e}");
                            } else {
                                tracing::debug!(
                                    "screencast restore/start failed (non-interactive): {e}"
                                );
                            }
                        }
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
            if is_all_black(&frame) {
                tracing::warn!(
                    "X11 fallback captured an all-black frame (likely XWayland root without \
                     compositor content); portal ScreenCast/Screenshot is the correct backend \
                     for this session"
                );
            }
            self.set_active(Some(ActiveBackend::X11));
            return Ok(CapturedFrame::Pixels(frame));
        }

        Err(AgentShellError::BackendUnavailable(
            "no capture backend available (portal ScreenCast/Screenshot unreachable, no native \
             X11 session)"
                .into(),
        ))
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

/// capture 组件 doctor 行（CLI collect_doctor 用）。
pub async fn doctor_line(dispatcher: Option<&CaptureDispatcher>) -> String {
    const LABEL: &str = "截图捕获";
    match dispatcher {
        None => format!("✗ {LABEL:<12}: 不可用（无 portal 且无原生 X11 会话，TTY？）"),
        Some(d) => {
            let backends = d.available_backends().join(" → ");
            match d.selected_backend() {
                Some(b) => format!("✓ {LABEL:<12}: {}（选中 {}）", backends, b.name()),
                None => format!("⚠ {LABEL:<12}: 候选 {backends}（尚未实际建立会话）"),
            }
        }
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
}
