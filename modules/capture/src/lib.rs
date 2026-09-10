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
            return Ok(CapturedFrame::Pixels(frame));
        }

        Err(AgentShellError::BackendUnavailable(
            "no capture backend available (portal ScreenCast/Screenshot unreachable, no native \
             X11 session)"
                .into(),
        ))
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
/// 「候选」而无真实可用状态。探测后据实际选中的后端渲染。
pub async fn doctor_line(dispatcher: Option<&CaptureDispatcher>) -> String {
    const LABEL: &str = "截图捕获";
    match dispatcher {
        None => format!("✗ {LABEL:<12}: 不可用（无 portal 且无原生 X11 会话，TTY？）"),
        Some(d) => {
            let active = d.probe().await;
            render_capture_doctor_line(LABEL, &d.available_backends(), active)
        }
    }
}

/// 渲染 capture doctor 行（纯函数，可单测）。
///
/// `active` 为 [`CaptureDispatcher::probe`] 的探测结果：`Some` = 已建立
/// 真实会话；`None` = 非交互探测失败。`None` 的原因据候选集区分：含 portal
/// 后端则多半是「需交互授权」；仅 X11 则是「无可用后端」——两者排查方向不同，
/// 不混为一谈（X11 连接失败与授权无关）。
fn render_capture_doctor_line(
    label: &str,
    backends: &[&'static str],
    active: Option<ActiveBackend>,
) -> String {
    let chain = backends.join(" → ");
    match active {
        Some(b) => format!("✓ {label:<12}: {chain}（选中 {}）", b.name()),
        None if backends.iter().any(|b| b.starts_with("portal-")) => {
            format!("⚠ {label:<12}: 候选 {chain}（非交互探测未就绪，需 portal 交互授权）")
        }
        None => format!("⚠ {label:<12}: 候选 {chain}（非交互探测失败，无可用后端）"),
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
/// 解码失败（空文件 / 损坏 / 非 PNG）或所有像素 RGB 通道全 0 均视为非可用。
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
    // 逐行流式解码 + 短路：非黑像素（常见情形）在第一行即返回 false，
    // 无需像 `next_frame` 那样先物化整帧 25MB+ 缓冲再全量扫描。debug 构建
    // 下 PNG 解压/逐像素遍历无优化，全帧物化会放大每次 doctor 探测的开销。
    let (color_type, _depth) = reader.output_color_type();
    loop {
        match reader.next_row() {
            Ok(Some(row)) => {
                if row_has_non_black(row.data(), color_type) {
                    return false;
                }
            }
            Ok(None) => return true,
            Err(_) => return true,
        }
    }
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

    #[test]
    fn render_capture_doctor_line_with_active_backend() {
        // 已建立真实会话（probe 返回 Some）→ ✓ + 选中后端名。
        let line = render_capture_doctor_line(
            "截图捕获",
            &["portal-screencast", "portal-screenshot", "x11-mit-shm"],
            Some(ActiveBackend::ScreenCast),
        );
        assert!(
            line.starts_with("✓ 截图捕获"),
            "selected backend must render ✓: {line}"
        );
        assert!(line.contains("portal-screencast → portal-screenshot → x11-mit-shm"));
        assert!(line.contains("选中 portal-screencast"), "{line}");
    }

    #[test]
    fn render_capture_doctor_line_without_active_backend() {
        // 非交互探测未就绪（probe 返回 None）→ ⚠ 提示需 portal 交互授权，
        // 不再用旧的「尚未实际建立会话」（探测已真实执行过）。
        let line = render_capture_doctor_line(
            "截图捕获",
            &["portal-screencast", "portal-screenshot"],
            None,
        );
        assert!(line.starts_with("⚠ 截图捕获"), "{line}");
        assert!(line.contains("候选 portal-screencast → portal-screenshot"));
        assert!(line.contains("需 portal 交互授权"), "{line}");
    }

    #[test]
    fn render_capture_doctor_line_without_active_backend_x11_only() {
        // 候选集仅含 X11（无 portal 后端）→ 探测失败与授权无关，须渲染
        // 「无可用后端」而非「需 portal 交互授权」（Radian 建议 3）。
        let line = render_capture_doctor_line("截图捕获", &["x11-mit-shm"], None);
        assert!(line.starts_with("⚠ 截图捕获"), "{line}");
        assert!(line.contains("候选 x11-mit-shm"), "{line}");
        assert!(line.contains("无可用后端"), "{line}");
        assert!(!line.contains("portal"), "{line}");
    }
}
