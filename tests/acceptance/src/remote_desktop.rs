//! 私有 session bus 上的 RemoteDesktop portal + 可控 EIS peer。
//!
//! 授权恢复的判定面是「Start Response 晚于命令预算到达后，第二条命令是否
//! 接续同一 portal 会话完成 ConnectToEIS 并注入」。这里用真实 D-Bus 协议
//! 复现 portal 的 Request/Response 与 fd 交付，再用 reis 的 EIS 服务端完成
//! 真实 EI 握手，避免只在假后端上模拟生命周期。

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

use crate::bus::{claim, PrivateBus};

/// portal 服务 / 对象路径（与生产客户端同源）。
pub const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// 一次待发出的 `Start` 应答。
struct HeldStart {
    connection: zbus::Connection,
    request_path: String,
    /// 发起请求的客户端 unique name：客户端进程退出后请求随会话一起作废。
    sender: String,
    /// 请求所属 portal 会话：会话被 `Close` 即撤下对应界面。
    session: String,
}

/// RemoteDesktop mock 的可观测计数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortalCalls {
    pub creates: usize,
    pub selects: usize,
    pub starts: usize,
    pub connects: usize,
    /// 客户端对 `org.freedesktop.portal.Request.Close` 的调用次数
    /// （撤下待确认的授权界面）。
    pub request_closes: usize,
    /// 客户端对 `org.freedesktop.portal.Session.Close` 的调用次数
    /// （预算耗尽或建立失败时撤下授权界面）。
    pub closes: usize,
}

#[derive(Default)]
struct PortalState {
    eis_fd: Option<OwnedFd>,
    defer_start: bool,
    /// `Start` 方法应答本身也推迟：复刻方法回复晚于外层命令预算的路径。
    defer_start_reply: bool,
    held_starts: Vec<HeldStart>,
    calls: PortalCalls,
    last_session: Option<String>,
    /// 已创建会话与其属主（客户端 unique name）。
    sessions: Vec<(String, String)>,
}

/// RemoteDesktop portal mock。
#[derive(Clone)]
pub struct RemoteDesktopMock {
    state: Arc<Mutex<PortalState>>,
    /// 推迟中的 `Start` 方法应答的释放闸；`new` 形态不使用。
    start_reply: Arc<tokio::sync::Notify>,
}

impl RemoteDesktopMock {
    /// `eis_fd` 是 `ConnectToEIS` 交给客户端的 socket 端；`defer_start` 为真时
    /// `Start` 只登记请求，响应由测试显式释放（复刻用户晚确认）。
    pub fn new(eis_fd: OwnedFd, defer_start: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(PortalState {
                eis_fd: Some(eis_fd),
                defer_start,
                ..PortalState::default()
            })),
            start_reply: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// 与 [`Self::new`] 相同，但 `Start` 的方法应答也挂起，直到
    /// [`Self::release_start_reply`]——用于验证「方法应答晚于命令预算」时
    /// 会话仍被持有。
    pub fn new_with_delayed_start_reply(eis_fd: OwnedFd, defer_start: bool) -> Self {
        let mock = Self::new(eis_fd, defer_start);
        mock.state.lock().expect("portal state").defer_start_reply = true;
        mock
    }

    /// 释放被推迟的 `Start` 方法应答。
    pub fn release_start_reply(&self) {
        self.start_reply.notify_one();
    }

    /// 已发生的 portal 调用计数。
    pub fn calls(&self) -> PortalCalls {
        self.state.lock().expect("portal state").calls
    }

    /// 最近一次 CreateSession 的会话路径。
    pub fn last_session(&self) -> Option<String> {
        self.state
            .lock()
            .expect("portal state")
            .last_session
            .clone()
    }

    /// 尚未确认的 `Start` 请求数（授权界面仍在等待用户）。
    pub fn held_starts(&self) -> usize {
        self.state.lock().expect("portal state").held_starts.len()
    }

    /// 释放被推迟的 `Start` 响应（返回释放条数）。
    pub async fn release_start(&self) -> usize {
        let held: Vec<HeldStart> = {
            let mut g = self.state.lock().expect("portal state");
            std::mem::take(&mut g.held_starts)
        };
        let n = held.len();
        for h in held {
            h.connection
                .emit_signal(
                    None::<&str>,
                    h.request_path.as_str(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(0u32, HashMap::<String, OwnedValue>::new()),
                )
                .await
                .expect("emit Start Response");
        }
        n
    }
}

fn request_path(sender: &str, token: &str) -> String {
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

fn sender_part(header: &zbus::message::Header<'_>) -> String {
    header
        .sender()
        .map(|s| s.as_str().trim_start_matches(':').replace('.', "_"))
        .unwrap_or_else(|| "unknown".into())
}

fn handle_token(options: &HashMap<String, OwnedValue>, fallback: &str) -> String {
    options
        .get("handle_token")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_else(|| fallback.into())
}

async fn emit_response(
    conn: &zbus::Connection,
    path: &str,
    results: HashMap<String, OwnedValue>,
) -> zbus::fdo::Result<()> {
    conn.emit_signal(
        None::<&str>,
        path,
        "org.freedesktop.portal.Request",
        "Response",
        &(0u32, results),
    )
    .await
    .map_err(|e| zbus::fdo::Error::Failed(format!("emit Response: {e}")))
}

#[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop")]
impl RemoteDesktopMock {
    async fn create_session(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let sender = sender_part(&header);
        let token = handle_token(&options, "create");
        let session_token = options
            .get("session_handle_token")
            .and_then(|v| String::try_from(v.clone()).ok())
            .unwrap_or_else(|| "session".into());
        let session = format!("/org/freedesktop/portal/desktop/session/{sender}/{session_token}");
        let path = request_path(&sender, &token);
        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert(
            "session_handle".into(),
            OwnedValue::try_from(Value::from(session.clone())).expect("session_handle value"),
        );
        {
            let mut g = self.state.lock().expect("portal state");
            g.calls.creates += 1;
            g.last_session = Some(session.clone());
            g.sessions.push((
                session.clone(),
                header.sender().map(|s| s.to_string()).unwrap_or_default(),
            ));
        }
        // 会话对象在真实 portal 里可被 `Close` 撤销（撤下授权界面）。
        conn.object_server()
            .at(
                session.as_str(),
                PortalSessionMock {
                    state: Arc::clone(&self.state),
                    path: session.clone(),
                },
            )
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("serve session object: {e}")))?;
        emit_response(conn, &path, results).await?;
        OwnedObjectPath::try_from(path).map_err(|e| zbus::fdo::Error::Failed(format!("path: {e}")))
    }

    async fn select_devices(
        &self,
        _session_handle: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let path = request_path(&sender_part(&header), &handle_token(&options, "select"));
        self.state.lock().expect("portal state").calls.selects += 1;
        emit_response(conn, &path, HashMap::new()).await?;
        OwnedObjectPath::try_from(path).map_err(|e| zbus::fdo::Error::Failed(format!("path: {e}")))
    }

    async fn start(
        &self,
        session_handle: OwnedObjectPath,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let path = request_path(&sender_part(&header), &handle_token(&options, "start"));
        // 授权界面属于请求对象：真实后端在 Request.Close 时撤下对话框。
        conn.object_server()
            .at(
                path.as_str(),
                PortalRequestMock {
                    state: Arc::clone(&self.state),
                    path: path.clone(),
                },
            )
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("serve request object: {e}")))?;
        let (defer, defer_reply) = {
            let mut g = self.state.lock().expect("portal state");
            g.calls.starts += 1;
            if g.defer_start {
                g.held_starts.push(HeldStart {
                    connection: conn.clone(),
                    request_path: path.clone(),
                    sender: header.sender().map(|s| s.to_string()).unwrap_or_default(),
                    session: session_handle.as_str().to_string(),
                });
            }
            (g.defer_start, g.defer_start_reply)
        };
        if defer_reply {
            self.start_reply.notified().await;
        }
        if !defer {
            emit_response(conn, &path, HashMap::new()).await?;
        }
        OwnedObjectPath::try_from(path).map_err(|e| zbus::fdo::Error::Failed(format!("path: {e}")))
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        _session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<OwnedFd> {
        let mut g = self.state.lock().expect("portal state");
        g.calls.connects += 1;
        g.eis_fd
            .take()
            .ok_or_else(|| zbus::fdo::Error::Failed("fixture EIS fd already delivered".into()))
    }
}

/// `Start` 请求对象：`Close` 撤下授权界面并作废尚未发出的应答。
#[derive(Clone)]
struct PortalRequestMock {
    state: Arc<Mutex<PortalState>>,
    path: String,
}

#[zbus::interface(name = "org.freedesktop.portal.Request")]
impl PortalRequestMock {
    async fn close(&self) {
        let mut g = self.state.lock().expect("portal state");
        g.calls.request_closes += 1;
        g.held_starts.retain(|held| held.request_path != self.path);
    }
}

/// 会话对象：只记录 `Close`。真实 GNOME portal 收到它即撤下授权界面。
#[derive(Clone)]
struct PortalSessionMock {
    state: Arc<Mutex<PortalState>>,
    path: String,
}

#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl PortalSessionMock {
    async fn close(&self) {
        let mut g = self.state.lock().expect("portal state");
        g.calls.closes += 1;
        g.held_starts.retain(|held| held.session != self.path);
    }
}

/// 在私有 bus 上启动 RemoteDesktop mock 并占用服务名。
pub async fn serve_remote_desktop(bus: &PrivateBus, mock: RemoteDesktopMock) -> zbus::Connection {
    let server = bus.serve(PORTAL_PATH, mock.clone()).await;
    claim(&server, PORTAL_SERVICE).await;
    watch_client_disconnects(&server, Arc::clone(&mock.state));
    server
}

/// 复刻 portal 后端对客户端断连的处理：会话属主离开总线即撤销会话。
///
/// 瞬态服务随客户端退出时不会显式 `Close`；真实 portal 观察到发起方
/// unique name 消失后撤下授权界面。这里订阅 `NameOwnerChanged` 得到同一
/// 可观察事实，供 TC-input-08 判定「命令退出后界面撤下」。
fn watch_client_disconnects(conn: &zbus::Connection, state: Arc<Mutex<PortalState>>) {
    let conn = conn.clone();
    tokio::spawn(async move {
        let Ok(rule) = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")
            .and_then(|builder| builder.interface("org.freedesktop.DBus"))
            .and_then(|builder| builder.member("NameOwnerChanged"))
            .map(|builder| builder.build())
        else {
            return;
        };
        let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, Some(16)).await
        else {
            return;
        };
        use futures::StreamExt;
        while let Some(Ok(msg)) = stream.next().await {
            let Ok((_name, old_owner, new_owner)): Result<(String, String, String), _> =
                msg.body().deserialize()
            else {
                continue;
            };
            if old_owner.is_empty() || !new_owner.is_empty() {
                continue;
            }
            let mut g = state.lock().expect("portal state");
            let gone = g
                .sessions
                .iter()
                .filter(|(_, owner)| owner == &old_owner)
                .count();
            let withdrawn = g
                .held_starts
                .iter()
                .filter(|held| held.sender == old_owner)
                .count();
            g.sessions.retain(|(_, owner)| owner != &old_owner);
            g.held_starts.retain(|held| held.sender != old_owner);
            g.calls.request_closes += withdrawn;
            g.calls.closes += gone;
        }
    });
}

/// EIS 服务端观测结果。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EisLog {
    pub handshakes: usize,
    pub binds: usize,
    /// 服务端实际创建并广告的 EI 设备数。
    pub devices: usize,
    pub keys: usize,
    pub motion_absolute: usize,
    pub last_absolute: Option<(f32, f32)>,
    pub button_presses: usize,
    pub button_releases: usize,
    pub scrolls: usize,
    pub last_scroll: Option<(f32, f32)>,
}

/// 最小 EIS 服务端：完成 EI 握手、广告设备并记录注入请求。
pub struct FakeEis {
    stop: Arc<AtomicBool>,
    log: Arc<Mutex<EisLog>>,
    thread: Option<JoinHandle<()>>,
}

impl FakeEis {
    /// 在 `stream` 上启动服务端线程，广告完整键鼠能力。
    pub fn start(stream: UnixStream) -> Self {
        Self::start_with_capabilities(stream, full_capabilities())
    }

    /// 在 `stream` 上启动服务端线程，只广告给定能力（判定缺能力时的健康）。
    pub fn start_with_capabilities(
        stream: UnixStream,
        capabilities: reis::enumflags2::BitFlags<reis::event::DeviceCapability>,
    ) -> Self {
        Self::start_with_device_split(stream, capabilities, false)
    }

    /// 按 GNOME 的真实形态广告能力：指针（相对+按键+滚动）、绝对指针和
    /// 键盘分别是独立的 `ei_device`，客户端必须聚合而不是只留首个设备。
    pub fn start_split_devices(stream: UnixStream) -> Self {
        Self::start_with_device_split(stream, full_capabilities(), true)
    }

    fn start_with_device_split(
        stream: UnixStream,
        capabilities: reis::enumflags2::BitFlags<reis::event::DeviceCapability>,
        split_devices: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(EisLog::default()));
        let thread = {
            let stop = Arc::clone(&stop);
            let log = Arc::clone(&log);
            std::thread::spawn(move || serve_eis(stream, stop, log, capabilities, split_devices))
        };
        Self {
            stop,
            log,
            thread: Some(thread),
        }
    }

    /// 当前观测结果。
    pub fn log(&self) -> EisLog {
        *self.log.lock().expect("eis log")
    }

    /// 等到至少 `keys` 个按键请求被服务端解析。
    pub fn wait_for_keys(&self, keys: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.log().keys >= keys {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// 等到日志满足 `predicate`（判定异步注入事件到达 peer）。
    pub fn wait_until(&self, predicate: impl Fn(&EisLog) -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate(&self.log()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

impl Drop for FakeEis {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn full_capabilities() -> reis::enumflags2::BitFlags<reis::event::DeviceCapability> {
    reis::event::DeviceCapability::Keyboard
        | reis::event::DeviceCapability::Pointer
        | reis::event::DeviceCapability::PointerAbsolute
        | reis::event::DeviceCapability::Button
        | reis::event::DeviceCapability::Scroll
}

fn serve_eis(
    stream: UnixStream,
    stop: Arc<AtomicBool>,
    log: Arc<Mutex<EisLog>>,
    capabilities: reis::enumflags2::BitFlags<reis::event::DeviceCapability>,
    split_devices: bool,
) {
    let poll_fd = stream.try_clone().expect("clone EIS socket");
    let ctx = match reis::eis::Context::new(stream) {
        Ok(ctx) => ctx,
        Err(_) => return,
    };
    let mut handshaker = reis::handshake::EisHandshaker::new(&ctx, 1);
    let mut converter: Option<reis::request::EisRequestConverter> = None;
    let mut sequence = 0u32;

    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd: poll_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll 只读单个已初始化的 pollfd。
        let ready = unsafe { libc::poll(&mut pfd, 1, 5) };
        if ready > 0 {
            match ctx.read() {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        } else if ready < 0 {
            return;
        }

        while let Some(result) = ctx.pending_request() {
            let request = match result {
                reis::PendingRequestResult::Request(request) => request,
                _ => continue,
            };
            match converter.as_mut() {
                None => match handshaker.handle_request(request) {
                    Ok(Some(resp)) => {
                        let conv = reis::request::EisRequestConverter::new(&ctx, resp, 1);
                        let _ = conv
                            .handle()
                            .add_seat(Some("agent-shell-fixture"), capabilities);
                        let _ = conv.handle().flush();
                        log.lock().expect("eis log").handshakes += 1;
                        converter = Some(conv);
                    }
                    Ok(None) => {}
                    Err(_) => return,
                },
                Some(conv) => {
                    if conv.handle_request(request).is_err() {
                        return;
                    }
                    while let Some(request) = conv.next_request() {
                        match request {
                            reis::request::EisRequest::Bind(bind) => {
                                let groups = if split_devices {
                                    vec![
                                        reis::event::DeviceCapability::Pointer
                                            | reis::event::DeviceCapability::Button
                                            | reis::event::DeviceCapability::Scroll,
                                        reis::event::DeviceCapability::PointerAbsolute.into(),
                                        reis::event::DeviceCapability::Keyboard.into(),
                                    ]
                                } else {
                                    vec![bind.capabilities]
                                };
                                for (index, group) in groups.into_iter().enumerate() {
                                    let group = group & bind.capabilities;
                                    if group.is_empty() {
                                        continue;
                                    }
                                    let device = bind.seat.add_device(
                                        Some(&format!("virtual-{index}")),
                                        reis::eis::device::DeviceType::Virtual,
                                        group,
                                        |_| {},
                                    );
                                    device.resumed();
                                    sequence += 1;
                                    device.start_emulating(sequence);
                                    log.lock().expect("eis log").devices += 1;
                                }
                                let _ = conv.handle().flush();
                                log.lock().expect("eis log").binds += 1;
                            }
                            reis::request::EisRequest::KeyboardKey(_) => {
                                log.lock().expect("eis log").keys += 1;
                            }
                            reis::request::EisRequest::PointerMotionAbsolute(motion) => {
                                let mut l = log.lock().expect("eis log");
                                l.motion_absolute += 1;
                                l.last_absolute = Some((motion.dx_absolute, motion.dy_absolute));
                            }
                            reis::request::EisRequest::Button(button) => {
                                let mut l = log.lock().expect("eis log");
                                match button.state {
                                    reis::eis::button::ButtonState::Press => {
                                        l.button_presses += 1;
                                    }
                                    reis::eis::button::ButtonState::Released => {
                                        l.button_releases += 1;
                                    }
                                }
                            }
                            reis::request::EisRequest::ScrollDelta(scroll) => {
                                let mut l = log.lock().expect("eis log");
                                l.scrolls += 1;
                                l.last_scroll = Some((scroll.dx, scroll.dy));
                            }
                            reis::request::EisRequest::ScrollDiscrete(_) => {
                                log.lock().expect("eis log").scrolls += 1;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
