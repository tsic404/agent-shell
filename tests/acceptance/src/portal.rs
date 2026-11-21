//! portal Screenshot 夹具：私有 session bus 上模拟授权门户的成功/拒绝。
//!
//! 截图验收的判定面是「daemon 与 portal 的协议交互 + 落盘编码」，真实屏幕
//! 内容不可在 CI 复现；本夹具让 `CaptureDispatcher` 走真实 portal 客户端、
//! 真实 PNG 校验与真实落盘路径。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::zvariant::OwnedValue;

use crate::bus::{claim, PrivateBus};

/// portal service / path / interface 坐标（与 `modules/capture` 同源）。
pub const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub const SCREENSHOT_IFACE: &str = "org.freedesktop.portal.Screenshot";
pub const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";

/// 一次 portal Screenshot 调用的行为。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalBehaviour {
    /// 返回 0 并给出 PNG 文件 URI。
    Success,
    /// 返回非零响应码（1 = 用户取消 / 2 = 其它），客户端归一到权限错误。
    Cancel,
    /// 方法调用永不返回（模拟 KDE 截图授权弹窗无人应答）。
    Hang,
}

/// portal Screenshot mock：返回指定 PNG 字节与响应码。
#[derive(Clone)]
pub struct ScreenshotMock {
    behaviour: Arc<Mutex<PortalBehaviour>>,
    interactive_behaviour: Arc<Mutex<Option<PortalBehaviour>>>,
    png: Arc<Mutex<Vec<u8>>>,
    calls: Arc<AtomicUsize>,
    interactive_calls: Arc<AtomicUsize>,
    /// PNG 落盘目录（每个请求生成独立文件）。
    root: PathBuf,
}

impl ScreenshotMock {
    /// 成功返回的 mock（PNG 字节由测试给定）。
    pub fn new(root: PathBuf, png: Vec<u8>) -> Self {
        Self {
            behaviour: Arc::new(Mutex::new(PortalBehaviour::Success)),
            interactive_behaviour: Arc::new(Mutex::new(None)),
            png: Arc::new(Mutex::new(png)),
            calls: Arc::new(AtomicUsize::new(0)),
            interactive_calls: Arc::new(AtomicUsize::new(0)),
            root,
        }
    }

    /// 切换后续调用的行为（测试分阶段驱动）。
    pub fn set_behaviour(&self, behaviour: PortalBehaviour) {
        *self.behaviour.lock().expect("portal behaviour") = behaviour;
    }

    /// 为 `interactive=true` 的调用单独设定行为（None = 与静默行为一致）。
    pub fn set_interactive_behaviour(&self, behaviour: PortalBehaviour) {
        *self.interactive_behaviour.lock().expect("portal behaviour") = Some(behaviour);
    }

    /// 替换后续成功响应落盘的 PNG 字节（分阶段驱动）。
    pub fn set_png(&self, png: Vec<u8>) {
        *self.png.lock().expect("portal png") = png;
    }

    /// 已收到的 Screenshot 方法调用数。
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    /// 已收到的 `interactive=true` Screenshot 方法调用数。
    pub fn interactive_calls(&self) -> usize {
        self.interactive_calls.load(Ordering::Relaxed)
    }

    fn next_png(&self) -> PathBuf {
        let path = self.root.join(format!(
            "portal-shot-{}.png",
            self.calls.load(Ordering::Relaxed)
        ));
        std::fs::write(&path, self.png.lock().expect("portal png").clone())
            .expect("write mock portal png");
        path
    }
}

#[zbus::interface(name = "org.freedesktop.portal.Screenshot")]
impl ScreenshotMock {
    /// `Screenshot(s parent_window, a{sv} options) -> o handle`。
    ///
    /// 返回按 handle_token 规则预算的 Request 路径，并在返回前发出 `Response`
    /// ——客户端在调用前已订阅该路径，信号不会丢失。
    async fn screenshot(
        &self,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let token = options
            .get("handle_token")
            .and_then(|v| String::try_from(v.clone()).ok())
            .unwrap_or_else(|| "agent-shell-acceptance".into());
        let sender = header
            .sender()
            .map(|s| s.as_str().trim_start_matches(':').replace('.', "_"))
            .unwrap_or_else(|| "unknown".into());
        let request_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

        let interactive = options
            .get("interactive")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);
        if interactive {
            self.interactive_calls.fetch_add(1, Ordering::Relaxed);
        }
        let behaviour = if interactive {
            self.interactive_behaviour
                .lock()
                .expect("portal behaviour")
                .unwrap_or_else(|| *self.behaviour.lock().expect("portal behaviour"))
        } else {
            *self.behaviour.lock().expect("portal behaviour")
        };
        let code: u32 = match behaviour {
            PortalBehaviour::Success => 0,
            PortalBehaviour::Cancel => 1,
            PortalBehaviour::Hang => {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                return Err(zbus::fdo::Error::Failed("portal hang released".into()));
            }
        };
        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        if behaviour == PortalBehaviour::Success {
            let path = self.next_png();
            let uri = format!("file://{}", path.display());
            results.insert(
                "uri".into(),
                OwnedValue::try_from(zvariant_string(&uri)).expect("uri value"),
            );
        }
        conn.emit_signal(
            None::<&str>,
            request_path.as_str(),
            "org.freedesktop.portal.Request",
            "Response",
            &(code, results),
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("emit Response: {e}")))?;

        zbus::zvariant::OwnedObjectPath::try_from(request_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("request path: {e}")))
    }
}

fn zvariant_string(value: &str) -> zbus::zvariant::Value<'static> {
    zbus::zvariant::Value::from(value.to_string())
}

/// portal ScreenCast mock 的共享生命周期状态。
#[derive(Default)]
struct ScreenCastState {
    calls: AtomicUsize,
    start_calls: AtomicUsize,
    start_responds: AtomicBool,
    sessions_created: AtomicUsize,
    session_closes: AtomicUsize,
    session_close_attempts: AtomicUsize,
    requests_cancelled: AtomicUsize,
    open_sessions: Mutex<HashSet<String>>,
    pending_requests: Mutex<HashSet<String>>,
    close_delay: Mutex<Duration>,
    mismatch_step: Mutex<Option<ScreenCastStep>>,
}

/// ScreenCast 流程中可注入 Request 路径失配的步骤。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenCastStep {
    CreateSession,
    SelectSources,
    Start,
}

/// portal ScreenCast mock：走完 CreateSession / SelectSources，`Start`
/// 永不发 Response，用于验证默认特性下 ScreenCast 级受独立子预算约束并
/// 让位给交互 Screenshot。
///
/// 每次 CreateSession 都在独立路径上注册真实的
/// `org.freedesktop.portal.Session` 对象；每个 Request 路径注册
/// `org.freedesktop.portal.Request`。测试据此断言超时后远端会话被 Close、
/// 未完成的授权请求被取消，而不是只看本地方法计数。
#[derive(Clone)]
pub struct ScreenCastMock {
    state: Arc<ScreenCastState>,
}

impl ScreenCastMock {
    /// 构造一个 `Start` 永不发 Response 的 ScreenCast portal。
    pub fn new() -> Self {
        Self {
            state: Arc::new(ScreenCastState::default()),
        }
    }

    /// 已收到的 ScreenCast 方法调用总数。
    pub fn calls(&self) -> usize {
        self.state.calls.load(Ordering::Relaxed)
    }

    /// 已收到的 Start 调用数（Response 信号永不发出）。
    pub fn start_calls(&self) -> usize {
        self.state.start_calls.load(Ordering::Relaxed)
    }

    /// 让 `Start` 立即返回成功响应（用于已建捕获器 Drop 路径回归）。
    pub fn respond_to_start(&self) {
        self.state.start_responds.store(true, Ordering::SeqCst);
    }

    /// 让指定步骤返回非预期的 Request 路径，模拟后端不按 handle_token 回复。
    pub fn set_request_path_mismatch(&self, step: ScreenCastStep) {
        *self.state.mismatch_step.lock().expect("mismatch step") = Some(step);
    }

    /// 延迟 `org.freedesktop.portal.Session.Close` 的响应。
    pub fn set_close_delay(&self, delay: Duration) {
        *self.state.close_delay.lock().expect("close delay") = delay;
    }

    /// 已创建的 portal 会话数。
    pub fn sessions_created(&self) -> usize {
        self.state.sessions_created.load(Ordering::Relaxed)
    }

    /// 已收到 `org.freedesktop.portal.Session.Close` 的会话数。
    pub fn session_closes(&self) -> usize {
        self.state.session_closes.load(Ordering::Relaxed)
    }

    /// 已发起的 `Session.Close` 调用次数（含尚未完成的调用）。
    pub fn session_close_attempts(&self) -> usize {
        self.state.session_close_attempts.load(Ordering::Relaxed)
    }

    /// 已收到 `org.freedesktop.portal.Request.Close` 的未完成请求数。
    pub fn requests_cancelled(&self) -> usize {
        self.state.requests_cancelled.load(Ordering::Relaxed)
    }

    /// 仍存活（尚未 Close）的 portal 会话数。
    pub fn open_sessions(&self) -> usize {
        self.state
            .open_sessions
            .lock()
            .expect("open sessions")
            .len()
    }

    /// 仍未完成（未发 Response 且未取消）的请求数。
    pub fn pending_requests(&self) -> usize {
        self.state
            .pending_requests
            .lock()
            .expect("pending requests")
            .len()
    }

    fn request_path_for(&self, step: ScreenCastStep, canonical: String) -> String {
        let mismatch = *self.state.mismatch_step.lock().expect("mismatch step");
        if mismatch == Some(step) {
            format!("{canonical}_unexpected")
        } else {
            canonical
        }
    }
}

impl Default for ScreenCastMock {
    fn default() -> Self {
        Self::new()
    }
}

#[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
impl ScreenCastMock {
    /// `CreateSession(a{sv} options) -> o handle`：返回会话句柄。
    async fn create_session(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        self.state.calls.fetch_add(1, Ordering::Relaxed);
        let request_path = request_path(&options, &header);
        let request_path = self.request_path_for(ScreenCastStep::CreateSession, request_path);
        let sender = sender_segment(&header);
        let index = self.state.sessions_created.fetch_add(1, Ordering::Relaxed) + 1;
        let session_path =
            format!("/org/freedesktop/portal/desktop/session/{sender}/agent_shell_mock_{index}");
        self.state
            .open_sessions
            .lock()
            .expect("open sessions")
            .insert(session_path.clone());
        register_session(conn, &session_path, self.state.clone()).await?;
        register_request(conn, &request_path, self.state.clone()).await?;
        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert(
            "session_handle".into(),
            OwnedValue::try_from(zvariant_string(&session_path)).expect("session handle value"),
        );
        emit_response(conn, &request_path, results).await?;
        finish_request(conn, &request_path, &self.state).await;
        zbus::zvariant::OwnedObjectPath::try_from(request_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("request path: {e}")))
    }

    /// `SelectSources(o session, a{sv} options) -> o handle`：接受选择。
    async fn select_sources(
        &self,
        _session_handle: zbus::zvariant::ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        self.state.calls.fetch_add(1, Ordering::Relaxed);
        let request_path = request_path(&options, &header);
        let request_path = self.request_path_for(ScreenCastStep::SelectSources, request_path);
        register_request(conn, &request_path, self.state.clone()).await?;
        emit_response(conn, &request_path, HashMap::new()).await?;
        finish_request(conn, &request_path, &self.state).await;
        zbus::zvariant::OwnedObjectPath::try_from(request_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("request path: {e}")))
    }

    /// `Start(o session, s parent, a{sv} options) -> o handle`：返回请求路径
    /// 但永不发 Response，模拟授权窗无人应答。
    async fn start(
        &self,
        _session_handle: zbus::zvariant::ObjectPath<'_>,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        self.state.calls.fetch_add(1, Ordering::Relaxed);
        self.state.start_calls.fetch_add(1, Ordering::Relaxed);
        let request_path = request_path(&options, &header);
        let request_path = self.request_path_for(ScreenCastStep::Start, request_path);
        register_request(conn, &request_path, self.state.clone()).await?;
        if self.state.start_responds.load(Ordering::SeqCst) {
            // 成功响应：给出一条 stream，供捕获器建立后验证 Drop 清理。
            let streams: Vec<(u32, HashMap<String, OwnedValue>)> = vec![(42, HashMap::new())];
            let mut results: HashMap<String, OwnedValue> = HashMap::new();
            results.insert(
                "streams".into(),
                OwnedValue::try_from(zvariant::Value::from(streams))
                    .map_err(|e| zbus::fdo::Error::Failed(format!("streams value: {e}")))?,
            );
            emit_response(conn, &request_path, results).await?;
            finish_request(conn, &request_path, &self.state).await;
        } else {
            self.state
                .pending_requests
                .lock()
                .expect("pending requests")
                .insert(request_path.clone());
        }
        zbus::zvariant::OwnedObjectPath::try_from(request_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("request path: {e}")))
    }

    /// `OpenPipeWireRemote(o session, a{sv} options) -> h fd`：返回一个
    /// 非 PipeWire socket 的 fd，worker 会快速失败退出；捕获器对象仍成立，
    /// 用于验证 Drop 的会话清理。
    #[zbus(name = "OpenPipeWireRemote")]
    async fn open_pipewire_remote(
        &self,
        _session_handle: zbus::zvariant::ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        use std::os::fd::OwnedFd;
        let (socket, _peer) = std::os::unix::net::UnixStream::pair()
            .map_err(|e| zbus::fdo::Error::Failed(format!("socketpair: {e}")))?;
        let fd: OwnedFd = socket.into();
        Ok(zbus::zvariant::OwnedFd::from(fd))
    }
}

/// 真实的 portal Session 对象：只在测试里记录 Close 并注销自己。
struct MockSession {
    path: String,
    state: Arc<ScreenCastState>,
}

#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl MockSession {
    /// `Close()`：释放会话；重复调用不再计数。
    async fn close(&self, #[zbus(object_server)] server: &zbus::object_server::ObjectServer) {
        self.state
            .session_close_attempts
            .fetch_add(1, Ordering::Relaxed);
        let delay = *self.state.close_delay.lock().expect("close delay");
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let was_open = self
            .state
            .open_sessions
            .lock()
            .expect("open sessions")
            .remove(&self.path);
        if was_open {
            self.state.session_closes.fetch_add(1, Ordering::Relaxed);
        }
        let _ = server.remove::<MockSession, _>(self.path.as_str()).await;
    }
}

/// 真实的 portal Request 对象：Close 取消未完成请求并注销自己。
struct MockRequest {
    path: String,
    state: Arc<ScreenCastState>,
}

#[zbus::interface(name = "org.freedesktop.portal.Request")]
impl MockRequest {
    /// `Close()`：取消尚未回 Response 的请求。
    async fn close(&self, #[zbus(object_server)] server: &zbus::object_server::ObjectServer) {
        let was_pending = self
            .state
            .pending_requests
            .lock()
            .expect("pending requests")
            .remove(&self.path);
        if was_pending {
            self.state
                .requests_cancelled
                .fetch_add(1, Ordering::Relaxed);
        }
        let _ = server.remove::<MockRequest, _>(self.path.as_str()).await;
    }
}

async fn register_session(
    conn: &zbus::Connection,
    path: &str,
    state: Arc<ScreenCastState>,
) -> zbus::fdo::Result<()> {
    conn.object_server()
        .at(
            path,
            MockSession {
                path: path.to_owned(),
                state,
            },
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("register session: {e}")))?;
    Ok(())
}

async fn register_request(
    conn: &zbus::Connection,
    path: &str,
    state: Arc<ScreenCastState>,
) -> zbus::fdo::Result<()> {
    conn.object_server()
        .at(
            path,
            MockRequest {
                path: path.to_owned(),
                state,
            },
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("register request: {e}")))?;
    Ok(())
}

async fn finish_request(conn: &zbus::Connection, path: &str, state: &ScreenCastState) {
    state
        .pending_requests
        .lock()
        .expect("pending requests")
        .remove(path);
    let _ = conn.object_server().remove::<MockRequest, _>(path).await;
}

fn sender_segment(header: &zbus::message::Header<'_>) -> String {
    header
        .sender()
        .map(|s| s.as_str().trim_start_matches(':').replace('.', "_"))
        .unwrap_or_else(|| "unknown".into())
}

fn request_path(
    options: &HashMap<String, OwnedValue>,
    header: &zbus::message::Header<'_>,
) -> String {
    let token = options
        .get("handle_token")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_else(|| "agent-shell-acceptance".into());
    format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sender_segment(header),
        token
    )
}

async fn emit_response(
    conn: &zbus::Connection,
    request_path: &str,
    results: HashMap<String, OwnedValue>,
) -> zbus::fdo::Result<()> {
    conn.emit_signal(
        None::<&str>,
        request_path,
        "org.freedesktop.portal.Request",
        "Response",
        &(0u32, results),
    )
    .await
    .map_err(|e| zbus::fdo::Error::Failed(format!("emit Response: {e}")))
}

/// 在私有 bus 上启动成功/拒绝可控的 Screenshot portal。
pub async fn serve_screenshot_portal(bus: &PrivateBus, mock: ScreenshotMock) -> zbus::Connection {
    let server = bus.serve(PORTAL_PATH, mock).await;
    claim(&server, PORTAL_SERVICE).await;
    server
}

/// 在同一 portal 对象路径上同时提供 Screenshot 与 ScreenCast 接口。
///
/// 默认特性回归需要先让 ScreenCast 级尝试建会话，再让交互 Screenshot 成功；
/// 两个接口必须由同一服务名对应的同一连接提供，才能被捕获组件依次调用。
pub async fn serve_portal(
    bus: &PrivateBus,
    screenshot: ScreenshotMock,
    screencast: ScreenCastMock,
) -> zbus::Connection {
    let server = bus.connect().await;
    server
        .object_server()
        .at(PORTAL_PATH, screenshot)
        .await
        .expect("register Screenshot mock");
    server
        .object_server()
        .at(PORTAL_PATH, screencast)
        .await
        .expect("register ScreenCast mock");
    claim(&server, PORTAL_SERVICE).await;
    server
}
