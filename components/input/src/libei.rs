//! libei/EIS 后端（设计文档 §12.3）。
//!
//! 通过 `xdg-desktop-portal.RemoteDesktop` 获取 EIS fd，再用纯 Rust EI 客户端建立
//! 协议连接并注入（CreateSession → SelectDevices → Start 弹窗 → ConnectToEIS → 注入）。
//! 会话授权弹窗意味着 `new()` 只做轻探测；完整会话建立延迟到首次注入
//! （`ensure_connected`）且连接由实例持有复用——与 §21.22「daemon 持有会话、避免
//! CLI 瞬态进程反复弹窗」一致。

use std::collections::HashMap;
use std::sync::Arc;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{KeyCombo, MouseButton};
use async_trait::async_trait;
use enumflags2::BitFlags;
use futures_util::StreamExt;
use reis::event::{DeviceCapability, EiEvent};
use tokio::sync::Mutex;

use super::dispatcher::{InputService, Op, SetupFailure};
use crate::keymap::{char_to_evdev, combo_to_press_sequence, evdev_mouse_button};

/// portal 服务 bus 名。
const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
/// portal RemoteDesktop 接口名。
const REMOTE_DESKTOP_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";
/// portal RemoteDesktop 对象路径（CreateSession/ConnectToEIS 所在）。
const REMOTE_DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";

/// `RemoteDesktop.ConnectToEIS` 缺失的诊断消息——portal 在场但无 EIS 注入
/// 手段（如 DDE 20 自带老 portal）。探测构造与 doctor/input_send 透出共用，
/// 避免字符串漂移。
const MISSING_CONNECT_TO_EIS: &str =
    "portal backend does not support EIS: RemoteDesktop.ConnectToEIS missing";

/// 轻探测预算：portal 在场性/内省属于装配路径，不能把单条命令拖过 C-154。
pub const LIBEI_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
/// portal RemoteDesktop → EIS 会话建立预算。
///
/// 必须低于 C-154 的单命令 5s：授权弹窗无人确认时，本次命令要在预算内返回
/// 「授权未确认」而不是让 CLI 先超时。确认后会话由本实例持有并复用。
pub const LIBEI_SETUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// 待确认 portal 会话自身预算（C-161）。
///
/// 自首次发出 `Start` 授权请求时起算，覆盖一次人工确认所需的固定窗口；
/// 后续命令接续同一会话不重置、不延长。它独立于单条命令预算：命令在
/// [`LIBEI_SETUP_BUDGET`] 内返回「授权待确认」，界面留在此窗口内等待用户。
pub const PORTAL_SESSION_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// worker 内握手截止后再等它退出的宽限。
///
/// 握手在阻塞 worker 的 runtime 内受同一 deadline 约束（可取消的 tokio 流），
/// 这里只等它把 fd 释放、线程结束；仍叠加在命令预算（4s）之内。
const WORKER_JOIN_GRACE: std::time::Duration = std::time::Duration::from_millis(300);
/// EIS 设备枚举的静默窗口：最后一个 `DeviceAdded` 之后再等这么久收齐同批
/// 设备；能力已覆盖所需接口时不必等满窗口。GNOME 把键盘与指针拆成多个
/// `ei_device`，只取首个设备会漏掉其余能力。
const EI_DEVICE_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// libei/EIS 注入后端。
pub struct LibeiInput {
    /// session bus 连接（构造时建立）。
    bus: zbus::Connection,
    /// 会话建立状态：已建立的 EI 会话、portal 待授权会话与尝试序号。
    /// 用 `Arc` 是为了让到期清理任务能在不借用 `self` 的情况下关闭会话。
    state: Arc<Mutex<LibeiState>>,
    /// 待确认会话自身预算（生产为 [`PORTAL_SESSION_BUDGET`]；验收可缩短）。
    session_budget: std::time::Duration,
    /// 单条命令等待建立的预算（生产为 [`LIBEI_SETUP_BUDGET`]；验收可缩短）。
    setup_budget: std::time::Duration,
}

/// 跨命令保留的建立状态。
///
/// `pending` 是「弹窗已发出但用户尚未确认」的 portal 会话：请求路径、
/// 会话句柄与**已订阅的 Response 流**都必须保留，否则用户晚确认时信号已被
/// 丢弃，下一条命令只能重新 CreateSession/Start 再弹一次窗。
#[derive(Default)]
struct LibeiState {
    /// 已建立的 EI 会话（跨调用复用，避免重复弹窗）。
    session: Option<Arc<EiSession>>,
    /// 待授权确认的 portal 会话（可接续）。
    pending: Option<PendingPortalSession>,
    /// 会话尝试序号：每次全新建立使用独立 token，避免旧 Request/会话路径冲突。
    attempt: u64,
}

/// 一次仍在等待用户确认（或已确认、待建 EIS）的 portal RemoteDesktop 会话。
///
/// 会话与 Response 流由持久状态持有：整条命令预算取消建立 future 时，局部
/// 变量会随 future 一起丢弃，只有留在 `LibeiState` 里才能被下一条命令接续。
struct PendingPortalSession {
    /// 已创建（STARTED 前）的 portal 会话对象路径。
    session_path: zbus::zvariant::ObjectPath<'static>,
    /// `Start` 返回并经实际订阅的 Request 路径（诊断用）。
    start_request_path: String,
    /// 会话所处阶段；`Start` 流只在尚未确认时保留。
    phase: PendingPhase,
    /// 会话自身预算的截止时刻：首次发出授权请求时固定，后续命令不重置。
    budget_deadline: tokio::time::Instant,
}

/// [`PendingPortalSession`] 的建立阶段。
enum PendingPhase {
    /// `Start` 已发出、等待用户确认；流在调用 `Start` **之前**订阅，
    /// 保留它才能接续晚到的授权。
    AwaitingStart(Box<zbus::MessageStream>),
    /// 用户已确认；取消后可直接重新 ConnectToEIS，不再弹窗。
    Authorized,
}

/// 一条已授权的 portal→EI 链路。
struct EiSession {
    inner: reis::event::Connection,
    /// EIS 可把一个能力集合拆到多个 `ei_device`（GNOME 把键盘与指针分开
    /// 广告）；每个设备各自持有自己的接口与帧边界，不能只留第一个。
    devices: Vec<EiDevice>,
}

/// 单个 EI 设备及其实际绑到的接口。
struct EiDevice {
    device: reis::event::Device,
    keyboard: Option<reis::ei::keyboard::Keyboard>,
    pointer_absolute: Option<reis::ei::pointer_absolute::PointerAbsolute>,
    button: Option<reis::ei::button::Button>,
    scroll: Option<reis::ei::scroll::Scroll>,
}

impl EiDevice {
    fn from_device(device: reis::event::Device) -> Self {
        let keyboard = device.interface();
        let pointer_absolute = device.interface();
        let button = device.interface();
        let scroll = device.interface();
        Self {
            device,
            keyboard,
            pointer_absolute,
            button,
            scroll,
        }
    }

    fn has_any_used_interface(&self) -> bool {
        self.keyboard.is_some()
            || self.pointer_absolute.is_some()
            || self.button.is_some()
            || self.scroll.is_some()
    }
}

impl EiSession {
    fn keyboard_device(&self) -> Option<&EiDevice> {
        self.devices.iter().find(|d| d.keyboard.is_some())
    }

    fn absolute_pointer_device(&self) -> Option<&EiDevice> {
        self.devices.iter().find(|d| d.pointer_absolute.is_some())
    }

    fn button_device(&self) -> Option<&EiDevice> {
        self.devices.iter().find(|d| d.button.is_some())
    }

    fn scroll_device(&self) -> Option<&EiDevice> {
        self.devices.iter().find(|d| d.scroll.is_some())
    }

    /// 所有设备合起来仍缺的接口（doctor/命令路径同判的输入）。
    fn missing_capabilities(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.keyboard_device().is_none() {
            missing.push("keyboard");
        }
        if self.absolute_pointer_device().is_none() {
            missing.push("absolute-pointer");
        }
        if self.button_device().is_none() {
            missing.push("button");
        }
        if self.scroll_device().is_none() {
            missing.push("scroll");
        }
        missing
    }
}

impl LibeiInput {
    /// 构造候选实例。轻探测：只验证 session bus 可连、portal 服务在 bus 上、
    /// RemoteDesktop 实现 `ConnectToEIS`；不触发 portal 弹窗。失败返回 `Err`
    /// 由 dispatcher 跳过该候选。
    pub async fn new() -> Result<Self> {
        match tokio::time::timeout(LIBEI_PROBE_BUDGET, async {
            let bus = zbus::Connection::session()
                .await
                .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
            Self::from_bus(bus).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(AgentShellError::BackendUnavailable(format!(
                "libei: portal probe exceeded {}s budget (portal not responding)",
                LIBEI_PROBE_BUDGET.as_secs()
            ))),
        }
    }

    /// 用给定的 session bus 装配（轻探测；调用方可指向私有总线）。
    pub async fn from_bus(bus: zbus::Connection) -> Result<Self> {
        Self::from_bus_with_budgets(bus, PORTAL_SESSION_BUDGET, LIBEI_SETUP_BUDGET).await
    }

    /// 同 [`Self::from_bus`]，可指定待确认会话自身预算（验收缩短窗口用）。
    pub async fn from_bus_with_session_budget(
        bus: zbus::Connection,
        session_budget: std::time::Duration,
    ) -> Result<Self> {
        Self::from_bus_with_budgets(bus, session_budget, LIBEI_SETUP_BUDGET).await
    }

    /// 同 [`Self::from_bus`]，可分别指定会话预算与单条命令建立预算
    /// （验收缩短等待窗口用；生产入口始终取 [`PORTAL_SESSION_BUDGET`] 与
    /// [`LIBEI_SETUP_BUDGET`]）。
    pub async fn from_bus_with_budgets(
        bus: zbus::Connection,
        session_budget: std::time::Duration,
        setup_budget: std::time::Duration,
    ) -> Result<Self> {
        let has_portal = zbus::fdo::DBusProxy::new(&bus)
            .await
            .map_err(|e| AgentShellError::DBus(format!("DBusProxy: {e}")))?
            .name_has_owner(
                PORTAL_DESTINATION
                    .try_into()
                    .map_err(|e| AgentShellError::DBus(format!("bus name: {e}")))?,
            )
            .await
            .map_err(|e| {
                tracing::debug!("libei probe: NameHasOwner failed: {e}");
                e
            })
            .unwrap_or(false);
        if !has_portal {
            return Err(AgentShellError::BackendUnavailable(
                "xdg-desktop-portal not running".into(),
            ));
        }
        // 探测 RemoteDesktop 是否实现 ConnectToEIS：老 portal（如 DDE 20 自带）
        // 只实现到 Start，缺 ConnectToEIS 时注入必然 UnknownMethod——在探测阶段
        // 即报可诊断错误并让 dispatcher 走降级链，而非等注入才失败。
        if !portal_has_connect_to_eis(&bus).await {
            return Err(AgentShellError::BackendUnavailable(
                MISSING_CONNECT_TO_EIS.into(),
            ));
        }
        let state = Arc::new(Mutex::new(LibeiState::default()));
        Self::spawn_session_expiry(bus.clone(), Arc::downgrade(&state));
        Ok(Self {
            bus,
            state,
            session_budget,
            setup_budget,
        })
    }

    /// 会话预算到期后关闭待确认的 portal 会话（C-161：无后续调用也要撤下界面）。
    ///
    /// 只在 `pending` 存在时按最近一次截止时刻唤醒；`Weak` 让 `LibeiInput`
    /// 全部释放后任务自行退出，不延长状态与 bus 连接的生命。
    fn spawn_session_expiry(bus: zbus::Connection, state: std::sync::Weak<Mutex<LibeiState>>) {
        tokio::spawn(async move {
            loop {
                let Some(shared) = state.upgrade() else {
                    return;
                };
                let remaining = {
                    let guard = shared.lock().await;
                    guard.pending.as_ref().map(|pending| {
                        pending
                            .budget_deadline
                            .saturating_duration_since(tokio::time::Instant::now())
                    })
                };
                let Some(remaining) = remaining else {
                    drop(shared);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                };
                if !remaining.is_zero() {
                    drop(shared);
                    tokio::time::sleep(remaining.min(std::time::Duration::from_millis(250))).await;
                    continue;
                }
                let expired = {
                    let mut guard = shared.lock().await;
                    match guard.pending.as_ref() {
                        Some(pending) if pending.budget_deadline <= tokio::time::Instant::now() => {
                            guard.pending.take()
                        }
                        _ => None,
                    }
                };
                drop(shared);
                if let Some(pending) = expired {
                    tracing::debug!(
                        session = %pending.session_path,
                        "libei: portal authorization window expired; withdrawing request"
                    );
                    withdraw_pending_session(&bus, &pending).await;
                }
            }
        });
    }

    /// 惰性建立完整 portal→EI 会话（首次注入时调用）。
    async fn ensure_connected(&self) -> Result<Arc<EiSession>> {
        match self.ensure_connected_setup().await {
            Ok(session) => Ok(session),
            Err(SetupFailure::CapabilityGap(message)) => {
                Err(AgentShellError::BackendUnavailable(message))
            }
            Err(SetupFailure::Pending(e)) | Err(SetupFailure::Unavailable(e)) => Err(e),
        }
    }

    /// [`Self::ensure_connected`] 的建立期结果：保留「授权待确认」与「通道
    /// 失败」的区别——前者不摘除候选，后者按 §19 重试/降级。
    async fn ensure_connected_setup(&self) -> std::result::Result<Arc<EiSession>, SetupFailure> {
        let deadline = tokio::time::Instant::now() + self.setup_budget;
        let mut guard = self.state.lock().await;
        if let Some(s) = guard.session.as_ref() {
            return Ok(Arc::clone(s));
        }

        // 1–4. portal RemoteDesktop 全流程（异步），拿到 EIS socket fd。
        let bus = self.bus.clone();
        let owned_fd =
            portal_connect_to_eis(&bus, &mut guard, self.session_budget, deadline).await?;

        // 5–6. EI 握手与 seat/device 协商在独立线程完成：
        //      reis 的 EiConvertEventStream 持有非 Send 回调表，其 future
        //      不能跨 async_trait 的 Send 边界；协商是一次性流程，用临时
        //      current_thread runtime 隔离执行，仅回传可 Send 的连接与代理。
        //
        //      预算必须下到 worker 内部：只对外层 JoinHandle 做 timeout 不会
        //      中止已经运行的阻塞任务，静默 EIS peer 会让线程与 fd 无限存活。
        //      这里在 worker 的 runtime 内用同一 deadline 封顶可取消的握手流，
        //      并等它退出后才返回。
        let negotiated = match tokio::time::timeout_at(
            deadline + WORKER_JOIN_GRACE,
            tokio::task::spawn_blocking(move || -> Result<EiNegotiated> {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| AgentShellError::Input(format!("libei runtime: {e}")))?
                    .block_on(async move { ei_handshake_and_negotiate(owned_fd, deadline).await })
            }),
        )
        .await
        {
            Ok(Ok(Ok(negotiated))) => negotiated,
            Ok(Ok(Err(e))) => return Err(SetupFailure::Unavailable(e)),
            Ok(Err(e)) => {
                return Err(SetupFailure::Unavailable(AgentShellError::Input(format!(
                    "libei setup join: {e}"
                ))))
            }
            Err(_) => {
                return Err(SetupFailure::Unavailable(AgentShellError::Timeout(
                    format!(
                        "libei: EIS handshake worker did not exit within the {}ms setup budget",
                        self.setup_budget.as_millis()
                    ),
                )))
            }
        };

        let s = Arc::new(EiSession {
            inner: negotiated.inner,
            devices: negotiated.devices,
        });
        guard.session = Some(Arc::clone(&s));
        Ok(s)
    }

    fn now_us() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    }

    /// 注入一组 evdev 键事件并逐事件成帧。
    async fn key_sequence(&self, seq: &[(u32, bool)]) -> Result<()> {
        use reis::ei::keyboard::KeyState;
        let s = self.ensure_connected().await?;
        let device = s.keyboard_device().ok_or_else(|| {
            AgentShellError::BackendUnavailable("libei: no keyboard capability".into())
        })?;
        let kb = device
            .keyboard
            .as_ref()
            .expect("keyboard_device carries keyboard");
        for (code, down) in seq {
            kb.key(
                *code,
                if *down {
                    KeyState::Press
                } else {
                    KeyState::Released
                },
            );
            device.device.device().frame(0, Self::now_us());
        }
        s.inner.flush().map_err(flush_err)
    }

    async fn send_key_combo(&self, combo: &KeyCombo) -> Result<()> {
        // combo_to_press_sequence 的 bool 是 is_temp_shift 标记，非 down 状态；
        // press 阶段统一置 true（按下），逆序释放阶段置 false（临时 shift
        // 在其实体键之后弹起）。
        let order = combo_to_press_sequence(combo).map_err(AgentShellError::Input)?;
        let presses: Vec<(u32, bool)> = order.iter().map(|(c, _)| (*c, true)).collect();
        let releases: Vec<(u32, bool)> = order.iter().rev().map(|(c, _)| (*c, false)).collect();
        let mut full = presses;
        full.extend(releases);
        self.key_sequence(&full).await
    }

    /// 鼠标按键注入（press+release 各自成帧）。
    async fn click_button(&self, button: u32) -> Result<()> {
        use reis::ei::button::ButtonState;
        let s = self.ensure_connected().await?;
        let device = s.button_device().ok_or_else(|| {
            AgentShellError::BackendUnavailable("libei: no pointer/button capability".into())
        })?;
        let btn = device
            .button
            .as_ref()
            .expect("button_device carries button");
        btn.button(button, ButtonState::Press);
        device.device.device().frame(0, Self::now_us());
        btn.button(button, ButtonState::Released);
        device.device.device().frame(0, Self::now_us());
        s.inner.flush().map_err(flush_err)
    }
}

/// 握手+协商结果（全部字段可跨线程传递）。
struct EiNegotiated {
    inner: reis::event::Connection,
    devices: Vec<EiDevice>,
}

/// EI 握手（Sender）→ bind 能力 → 等待带键盘/指针能力的设备就绪。
///
/// 整个握手（含 reis 的初始 `handshake_tokio`，它自身没有超时）都在同一
/// deadline 下：超时丢弃 future 即关闭 EIS socket，worker 线程随之退出。
async fn ei_handshake_and_negotiate(
    owned_fd: zbus::zvariant::OwnedFd,
    deadline: tokio::time::Instant,
) -> Result<EiNegotiated> {
    match tokio::time::timeout_at(
        deadline,
        ei_handshake_and_negotiate_inner(owned_fd, deadline),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(AgentShellError::Timeout(format!(
            "libei: EI handshake exceeded the {}ms setup budget",
            LIBEI_SETUP_BUDGET.as_millis()
        ))),
    }
}

/// [`ei_handshake_and_negotiate`] 本体（外层按同一 deadline 封顶）。
async fn ei_handshake_and_negotiate_inner(
    owned_fd: zbus::zvariant::OwnedFd,
    deadline: tokio::time::Instant,
) -> Result<EiNegotiated> {
    let std_fd: std::os::fd::OwnedFd = owned_fd.into();
    let ctx = reis::ei::Context::new(std::os::unix::net::UnixStream::from(std_fd))
        .map_err(|e| AgentShellError::Input(format!("libei: context: {e}")))?;
    let (conn, mut events) = ctx
        .handshake_tokio("agent-shell", reis::ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| AgentShellError::Input(format!("libei: handshake: {e}")))?;

    let mut devices: Vec<EiDevice> = Vec::new();
    let mut settle_deadline: Option<tokio::time::Instant> = None;
    loop {
        // 有设备进来后只等到静默窗口结束；否则等到总预算耗尽。
        let wait_until = settle_deadline.map_or(deadline, |settle| settle.min(deadline));
        let next = tokio::time::timeout_at(wait_until, events.next()).await;
        let ev = match next {
            Ok(Some(Ok(ev))) => ev,
            Ok(Some(Err(e))) => return Err(AgentShellError::Input(format!("libei event: {e}"))),
            Ok(None) => {
                return Err(AgentShellError::Input(
                    "libei: EIS connection closed".into(),
                ))
            }
            Err(_) if wait_until < deadline => break,
            Err(_) => {
                return Err(AgentShellError::Timeout(
                    "libei: seat/device negotiation timed out".into(),
                ))
            }
        };
        match ev {
            EiEvent::SeatAdded(added) => {
                added.seat.bind_capabilities(required_ei_capabilities());
                // bind 只是写入发送缓冲；不 flush 的话 EIS 永远看不到它，
                // 也就不会下发设备——协商会一直等到预算耗尽。
                conn.flush().map_err(flush_err)?;
            }
            EiEvent::DeviceAdded(added) => {
                let d = EiDevice::from_device(added.device);
                tracing::debug!(
                    name = d.device.name().unwrap_or("unnamed"),
                    keyboard = d.keyboard.is_some(),
                    pointer_absolute = d.pointer_absolute.is_some(),
                    button = d.button.is_some(),
                    scroll = d.scroll.is_some(),
                    "libei: EIS device advertised"
                );
                if d.has_any_used_interface() {
                    d.device.device().start_emulating(0, 1);
                    devices.push(d);
                    settle_deadline = Some(tokio::time::Instant::now() + EI_DEVICE_SETTLE);
                    if devices_cover_used_interfaces(&devices) {
                        break;
                    }
                }
            }
            EiEvent::Disconnected(dc) => {
                return Err(AgentShellError::BackendUnavailable(format!(
                    "libei: disconnected during setup ({})",
                    dc.explanation.unwrap_or_else(|| "no reason".into())
                )));
            }
            _ => {}
        }
    }
    conn.flush().map_err(flush_err)?;
    if devices.is_empty() {
        return Err(AgentShellError::BackendUnavailable(
            "libei: no usable device".into(),
        ));
    }
    Ok(EiNegotiated {
        inner: conn,
        devices,
    })
}

/// 已收集设备覆盖本组件使用的全部能力（键盘、绝对指针、按键、滚动）。
fn devices_cover_used_interfaces(devices: &[EiDevice]) -> bool {
    devices.iter().any(|d| d.keyboard.is_some())
        && devices.iter().any(|d| d.pointer_absolute.is_some())
        && devices.iter().any(|d| d.button.is_some())
        && devices.iter().any(|d| d.scroll.is_some())
}

fn flush_err(e: rustix::io::Errno) -> AgentShellError {
    AgentShellError::Input(format!("libei flush: {e}"))
}

/// 本组件实际注入所需的 EI 能力。
///
/// `bind_capabilities` 只按掩码向 EIS 申请接口，不会自动扩展：漏掉
/// PointerAbsolute/Button/Scroll 时 EIS 不会下发对应接口，随后
/// [`LibeiInput::ensure_ready`] 会把这些操作判为能力缺口——即使 portal
/// 已授权完整键鼠。
fn required_ei_capabilities() -> BitFlags<DeviceCapability> {
    BitFlags::from(DeviceCapability::Keyboard)
        | BitFlags::from(DeviceCapability::Pointer)
        | BitFlags::from(DeviceCapability::PointerAbsolute)
        | BitFlags::from(DeviceCapability::Button)
        | BitFlags::from(DeviceCapability::Scroll)
}

/// RemoteDesktop portal 的设备类型位掩码（`AvailableDeviceTypes` 同款编码）。
const KEYBOARD_DEVICE_TYPE: u32 = 1 << 0;
const POINTER_DEVICE_TYPE: u32 = 1 << 1;

/// 探测失败若为「portal 在场但缺 ConnectToEIS」返回诊断消息，否则 None。
///
/// 仅此一类失败是首选后端探测的可诊断根因（portal 在但能力缺失）。无
/// portal（headless/TTY/SSH 常态）、无 session bus 等不是缺陷，是「无注入
/// 后端」的常态——dispatcher 据此不记录根因，doctor 回退友好文案。
pub(crate) fn missing_connect_to_eis_reason(err: &AgentShellError) -> Option<&str> {
    match err {
        AgentShellError::BackendUnavailable(m) if m == MISSING_CONNECT_TO_EIS => Some(m),
        _ => None,
    }
}

/// 探测 portal RemoteDesktop 是否实现 `ConnectToEIS`（§12.3 第 4 步）。
///
/// 对 [`REMOTE_DESKTOP_PATH`] 做一次只读 Introspect，检查方法是否被广告。
/// Introspect 失败按「无法判定」处理并返回 true——内省受限或 portal 早期
/// 状态不应误摘除可用后端，届时沿用惰性探测（注入期报错）。仅当内省成功
/// 且方法缺失时才返回 false：该 portal 无 EIS 注入手段。
async fn portal_has_connect_to_eis(bus: &zbus::Connection) -> bool {
    let Ok(path) = zbus::zvariant::ObjectPath::try_from(REMOTE_DESKTOP_PATH) else {
        return true;
    };
    let Ok(proxy) = zbus::fdo::IntrospectableProxy::builder(bus)
        .destination(PORTAL_DESTINATION)
        .and_then(|b| b.path(path))
    else {
        return true;
    };
    let Ok(proxy) = proxy.build().await else {
        return true;
    };
    match proxy.introspect().await {
        Ok(xml) => xml_advertises_connect_to_eis(&xml),
        Err(_) => true,
    }
}

/// introspection XML 是否广告了 `ConnectToEIS` 方法。
///
/// 只认方法声明 `<method name="ConnectToEIS"`——裸子串会误匹配文档/注解中
/// 的提及。method 元素含子节点（in/out 参数），故按开标签前缀匹配。
fn xml_advertises_connect_to_eis(xml: &str) -> bool {
    xml.contains("<method name=\"ConnectToEIS\"")
}

/// portal CreateSession → SelectDevices → Start → ConnectToEIS 全流程，
/// 返回 EIS socket fd。
///
/// 每步使用独立 `handle_token`——portal Request 路径由
/// `/org/freedesktop/portal/desktop/request/<sender>/<token>` 计算，复用同一
/// token 会让多个请求的 Response 落在同一路径（后端只认首个请求），
/// `await_response` 会读到 CreateSession 而非 Start 的应答，ConnectToEIS
/// 时便会话尚未 STARTED（`AccessDenied: Invalid session`）。
///
/// 真正的 session_handle 由 CreateSession 的 Response 信号 results 里的
/// `session_handle`（规范误用 `s` 类型）返回，不自行拼路径。Start 步骤触发
/// 用户确认弹窗——调用方必须是持有本实例的常驻 daemon（§21.22）。
async fn portal_connect_to_eis(
    bus: &zbus::Connection,
    state: &mut LibeiState,
    session_budget: std::time::Duration,
    command_deadline: tokio::time::Instant,
) -> std::result::Result<zbus::zvariant::OwnedFd, SetupFailure> {
    use zbus::zvariant::{ObjectPath, Value};

    // 已有待确认的会话：只消费它的 Start Response 并继续 ConnectToEIS，
    // 不重新 CreateSession/SelectDevices/Start（否则会再弹一次授权窗）。
    if let Some(pending) = state.pending.as_ref() {
        if tokio::time::Instant::now() >= pending.budget_deadline {
            // 会话自身预算已耗尽而清理任务尚未跑到：本次调用先撤下请求，
            // 由后续调用重新发起，绝不沿用过期会话。
            let expired = state.pending.take().expect("pending checked");
            withdraw_pending_session(bus, &expired).await;
            return Err(SetupFailure::Unavailable(AgentShellError::Timeout(
                format!(
                    "libei: portal authorization window ({}s) expired; the request was withdrawn",
                    session_budget.as_secs()
                ),
            )));
        }
        tracing::debug!(
            session = %pending.session_path,
            request = %pending.start_request_path,
            remaining_ms = pending
                .budget_deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_millis(),
            "libei: resuming pending portal session"
        );
        return resume_pending_portal_session(bus, state, session_budget, command_deadline).await;
    }

    // 每次全新建立使用独立序号：旧请求/会话即使残留也不会与新请求撞路径。
    state.attempt += 1;
    let attempt = state.attempt;
    let sender_part = bus
        .unique_name()
        .map(|n| encode_sender_part(n.as_str()))
        .ok_or_else(|| AgentShellError::DBus("no unique name on session bus".into()))?;
    let pid = std::process::id();
    let req_prefix = "/org/freedesktop/portal/desktop/request/";
    let create_token = format!("agent_shell_input_create_{pid}_{attempt}");
    let select_token = format!("agent_shell_input_select_{pid}_{attempt}");
    let start_token = format!("agent_shell_input_start_{pid}_{attempt}");
    let session_token = format!("agent_shell_input_session_{pid}_{attempt}");

    let rd = zbus::Proxy::new(
        bus,
        PORTAL_DESTINATION,
        REMOTE_DESKTOP_PATH,
        REMOTE_DESKTOP_IFACE,
    )
    .await
    .map_err(|e| AgentShellError::DBus(format!("RemoteDesktop proxy: {e}")))?;

    // 1. CreateSession：方法返回 Request 对象路径；session_handle 在其
    //    Response 信号 results["session_handle"]（s）里。
    let create_path = request_path(req_prefix, &sender_part, &create_token)?;
    let mut create_stream = subscribe_response(bus, &create_path).await?;
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(create_token.as_str()));
    options.insert("session_handle_token", Value::from(session_token.as_str()));
    let create_request: zbus::zvariant::OwnedObjectPath = call_bounded(
        command_deadline,
        "CreateSession",
        rd.call("CreateSession", &(&options,)),
    )
    .await?;
    let create_results = await_response(
        &mut create_stream,
        bus,
        create_path.as_str(),
        create_request.as_str(),
        "CreateSession",
        command_deadline,
    )
    .await?;
    let session_handle = create_results
        .get("session_handle")
        .and_then(|v| v.downcast_ref::<&str>().ok())
        .ok_or_else(|| {
            AgentShellError::DBus(
                "RemoteDesktop: no session_handle in CreateSession response".into(),
            )
        })?;
    let session_path: ObjectPath<'static> = ObjectPath::try_from(session_handle.to_owned())
        .map_err(|e| AgentShellError::DBus(format!("bad session_handle: {e}")))?
        .into_owned();

    // 2. SelectDevices：types = KEYBOARD | POINTER。
    let select_path = request_path(req_prefix, &sender_part, &select_token)?;
    let mut select_stream = subscribe_response(bus, &select_path).await?;
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(select_token.as_str()));
    options.insert(
        "types",
        Value::from(KEYBOARD_DEVICE_TYPE | POINTER_DEVICE_TYPE),
    );
    let select_request: zbus::zvariant::OwnedObjectPath = call_bounded(
        command_deadline,
        "SelectDevices",
        rd.call::<_, (&ObjectPath<'_>, HashMap<&str, Value>), zbus::zvariant::OwnedObjectPath>(
            "SelectDevices",
            &(&session_path, options),
        ),
    )
    .await?;
    let _ = await_response(
        &mut select_stream,
        bus,
        select_path.as_str(),
        select_request.as_str(),
        "SelectDevices",
        command_deadline,
    )
    .await?;

    // 3. Start：触发用户确认弹窗。
    //    会话预算自首次发出授权请求时起算；此处固定，后续命令不得重置。
    let budget_deadline = tokio::time::Instant::now() + session_budget;
    let start_path = request_path(req_prefix, &sender_part, &start_token)?;
    let start_stream = subscribe_response(bus, &start_path).await?;
    // 会话、响应流与固定截止时间必须先落到持久状态，再发 Start：方法应答
    // 本身也可能晚于命令预算，外层 future 被取消时局部变量会连同会话句柄
    // 一起丢失，后台清理与会话预算都找不到它。
    state.pending = Some(PendingPortalSession {
        session_path: session_path.clone(),
        start_request_path: start_path.as_str().to_string(),
        phase: PendingPhase::AwaitingStart(Box::new(start_stream)),
        budget_deadline,
    });
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(start_token.as_str()));
    match call_bounded(
        command_deadline,
        "Start",
        rd.call::<_, (&ObjectPath<'_>, &str, HashMap<&str, Value>), zbus::zvariant::OwnedObjectPath>(
            "Start",
            &(&session_path, "", options),
        ),
    )
    .await
    {
        Ok(start_request) => {
            // 路径失配时改订阅返回路径；随后把**这条流**更新进持久状态，
            // 让晚到的授权确认能被下一条命令接续。
            if start_request.as_str() != start_path.as_str() {
                tracing::warn!(
                    expected = %start_path,
                    got = %start_request,
                    "portal request path mismatch; re-subscribing on returned path"
                );
                let stream = match subscribe_response(bus, &start_request).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        let expired = state.pending.take();
                        if let Some(expired) = expired {
                            withdraw_pending_session(bus, &expired).await;
                        }
                        return Err(e.into());
                    }
                };
                if let Some(pending) = state.pending.as_mut() {
                    pending.start_request_path = start_request.as_str().to_string();
                    pending.phase = PendingPhase::AwaitingStart(Box::new(stream));
                }
            }
        }
        Err(SetupFailure::Unavailable(AgentShellError::Timeout(_))) => {
            // Start 已发出、方法应答晚于命令预算：请求与授权界面仍归会话预算
            // 管，保持 pending，按「待确认」返回而不是丢弃会话。
            tracing::debug!("libei: portal Start method reply exceeded the command budget");
        }
        Err(other) => {
            let expired = state.pending.take();
            if let Some(expired) = expired {
                withdraw_pending_session(bus, &expired).await;
            }
            return Err(other);
        }
    }
    resume_pending_portal_session(bus, state, session_budget, command_deadline).await
}

/// 接续 [`LibeiState::pending`] 中已创建的 portal 会话，直到拿到 EIS fd。
///
/// pending 在整个可取消等待期间留在持久状态里，绝不 `take()` 到局部变量：
/// 外层命令预算取消本 future 时，会话句柄与已订阅的 Response 流仍在
/// `LibeiState`，下一条命令可以继续等待同一次授权，而不是重新弹窗。
async fn resume_pending_portal_session(
    bus: &zbus::Connection,
    state: &mut LibeiState,
    session_budget: std::time::Duration,
    command_deadline: tokio::time::Instant,
) -> std::result::Result<zbus::zvariant::OwnedFd, SetupFailure> {
    let budget_deadline = state
        .pending
        .as_ref()
        .map(|p| p.budget_deadline)
        .ok_or_else(|| {
            SetupFailure::Unavailable(AgentShellError::Input(
                "libei: no pending portal session to resume".into(),
            ))
        })?;
    // 单条命令预算与会话自身预算同时封顶：前者决定何时返回「待确认」，
    // 后者决定授权界面何时撤下；任何一方都不放大另一方。
    let deadline = command_deadline.min(budget_deadline);
    let awaiting_start = matches!(
        state.pending.as_ref().map(|p| &p.phase),
        Some(PendingPhase::AwaitingStart(_))
    );
    if awaiting_start {
        let wait = {
            let pending = state
                .pending
                .as_mut()
                .expect("pending checked before waiting for Start");
            match &mut pending.phase {
                PendingPhase::AwaitingStart(stream) => {
                    drain_response(stream, "Start", deadline).await
                }
                PendingPhase::Authorized => unreachable!("checked AwaitingStart"),
            }
        };
        match wait {
            Ok(_) => {
                if let Some(pending) = state.pending.as_mut() {
                    pending.phase = PendingPhase::Authorized;
                }
            }
            // 授权未确认：命令预算先到就保持原状（界面留给会话预算），
            // 会话预算先到则关闭会话、撤下界面，交由下一次调用重新发起。
            Err(SetupFailure::Pending(e)) => {
                if tokio::time::Instant::now() >= budget_deadline {
                    let expired = state.pending.take();
                    if let Some(expired) = expired {
                        withdraw_pending_session(bus, &expired).await;
                    }
                    return Err(SetupFailure::Unavailable(AgentShellError::Timeout(
                        format!(
                            "libei: portal authorization window ({}s) expired before confirmation; \
                             the request was withdrawn",
                            session_budget.as_secs()
                        ),
                    )));
                }
                return Err(SetupFailure::Pending(e));
            }
            Err(e) => {
                if let Some(pending) = state.pending.take() {
                    withdraw_pending_session(bus, &pending).await;
                }
                return Err(e);
            }
        }
    }

    let Some(session_path) = state.pending.as_ref().map(|p| p.session_path.clone()) else {
        return Err(SetupFailure::Unavailable(AgentShellError::Input(
            "libei: no pending portal session to resume".into(),
        )));
    };
    match connect_to_eis(bus, &session_path, deadline).await {
        Ok(fd) => {
            state.pending = None;
            Ok(fd)
        }
        Err(e) => {
            // 会话已拿到授权却无法建 EIS：关闭它，避免残留已授权的空会话。
            state.pending = None;
            close_portal_session(bus, &session_path).await;
            Err(e)
        }
    }
}

/// 对已授权的 portal 会话取 EIS socket（`ConnectToEIS`）。
async fn connect_to_eis(
    bus: &zbus::Connection,
    session_path: &zbus::zvariant::ObjectPath<'_>,
    deadline: tokio::time::Instant,
) -> std::result::Result<zbus::zvariant::OwnedFd, SetupFailure> {
    use zbus::zvariant::Value;

    let rd = zbus::Proxy::new(
        bus,
        PORTAL_DESTINATION,
        REMOTE_DESKTOP_PATH,
        REMOTE_DESKTOP_IFACE,
    )
    .await
    .map_err(|e| {
        SetupFailure::Unavailable(AgentShellError::DBus(format!("RemoteDesktop proxy: {e}")))
    })?;
    call_bounded(
        deadline,
        "ConnectToEIS",
        rd.call(
            "ConnectToEIS",
            &(session_path, HashMap::<&str, Value>::new()),
        ),
    )
    .await
}

/// 撤销一条尚未确认的 portal 会话。
///
/// 先关 `Start` 的 Request——授权界面属于请求对象，`org.freedesktop.portal.Request.Close`
/// 才会让后端撤下对话框；只关 Session 时 GNOME 后端的对话框仍留在屏幕上
/// （实测 xdg-desktop-portal-gnome 51.1）。随后关闭会话，回收 portal 侧资源。
async fn withdraw_pending_session(bus: &zbus::Connection, pending: &PendingPortalSession) {
    close_portal_request(bus, &pending.start_request_path).await;
    close_portal_session(bus, &pending.session_path).await;
}

/// 尽力关闭 portal 请求（`org.freedesktop.portal.Request.Close`）。
///
/// 请求对象绑定发起方 unique name：只有持有会话的服务实例能撤下它，
/// 其他连接（含运维手工 gdbus）会被 portal 以 `Unmatched caller` 拒绝。
async fn close_portal_request(bus: &zbus::Connection, request_path: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    let proxy = tokio::time::timeout_at(
        deadline,
        zbus::Proxy::new(
            bus,
            PORTAL_DESTINATION,
            request_path,
            "org.freedesktop.portal.Request",
        ),
    )
    .await;
    let Ok(Ok(proxy)) = proxy else {
        tracing::debug!(request = %request_path, "libei: portal request close proxy unavailable");
        return;
    };
    if tokio::time::timeout_at(deadline, proxy.call::<_, _, ()>("Close", &()))
        .await
        .is_err()
    {
        tracing::debug!(request = %request_path, "libei: portal request Close timed out");
    }
}

/// 尽力关闭 portal 会话（`org.freedesktop.portal.Session.Close`）。
///
/// 清理本身受 200ms 小预算约束且失败只记日志：命令的结论不应被清理路径
/// 改写，但已确认无望的会话不能一直留在 portal 侧。
async fn close_portal_session(
    bus: &zbus::Connection,
    session_path: &zbus::zvariant::ObjectPath<'_>,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    let proxy = tokio::time::timeout_at(
        deadline,
        zbus::Proxy::new(
            bus,
            PORTAL_DESTINATION,
            session_path.as_str(),
            "org.freedesktop.portal.Session",
        ),
    )
    .await;
    let Ok(Ok(proxy)) = proxy else {
        tracing::debug!(session = %session_path, "libei: portal session close proxy unavailable");
        return;
    };
    if tokio::time::timeout_at(deadline, proxy.call::<_, _, ()>("Close", &()))
        .await
        .is_err()
    {
        tracing::debug!(session = %session_path, "libei: portal session Close timed out");
    }
}

/// 预算内的 portal 方法调用：方法本身挂起也必须在会话建立预算内返回。
async fn call_bounded<T, F>(
    deadline: tokio::time::Instant,
    step: &str,
    call: F,
) -> std::result::Result<T, SetupFailure>
where
    F: std::future::Future<Output = std::result::Result<T, zbus::Error>>,
{
    match tokio::time::timeout_at(deadline, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(portal_err(e).into()),
        Err(_) => Err(SetupFailure::Unavailable(AgentShellError::Timeout(
            format!("libei: portal RemoteDesktop {step} call exceeded the setup budget"),
        ))),
    }
}

/// 将 D-Bus unique name 编码为 portal 路径的 sender 段。
///
/// `":1.42"` → `"1_42"`——xdg-desktop-portal handle_token 路径编码规则：
/// 去掉前导 `':'`，将 `'.'` 替换为 `'_'`。
fn encode_sender_part(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').replace('.', "_")
}

/// 预算 portal Request 对象路径（`/org/freedesktop/portal/desktop/request/<sender>/<token>`）。
fn request_path(
    req_prefix: &str,
    sender_part: &str,
    token: &str,
) -> Result<zbus::zvariant::ObjectPath<'static>> {
    zbus::zvariant::ObjectPath::try_from(format!("{req_prefix}{sender_part}/{token}"))
        .map_err(|e| AgentShellError::DBus(format!("request path: {e}")))
        .map(|p| p.into_owned())
}

/// 在发送 portal 调用**之前**订阅指定 Request 路径上的 `Response` 信号，
/// 避免后端在订阅前就回复导致信号丢失（竞态）。
async fn subscribe_response(
    bus: &zbus::Connection,
    request_path: &zbus::zvariant::ObjectPath<'_>,
) -> Result<zbus::MessageStream> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")
        .map_err(|e| AgentShellError::DBus(format!("match rule interface: {e}")))?
        .member("Response")
        .map_err(|e| AgentShellError::DBus(format!("match rule member: {e}")))?
        .path(request_path.clone())
        .map_err(|e| AgentShellError::DBus(format!("match rule path: {e}")))?
        .build();
    zbus::MessageStream::for_match_rule(rule, bus, Some(4))
        .await
        .map_err(|e| AgentShellError::DBus(format!("signal stream: {e}")))
}

/// 解析 `Request.Response` 信号体 `(ua{sv})` → (response code, results vardict)。
fn parse_response_body(
    msg: &zbus::Message,
) -> Result<(u32, HashMap<String, zbus::zvariant::OwnedValue>)> {
    msg.body()
        .deserialize()
        .map_err(|e| AgentShellError::DBus(format!("Response body: {e}")))
}

/// 消费预订阅的流，读取一条 `Response` 信号并解析 `(ua{sv})`，返回 results；
/// 响应码非 0 归一为 `Permission`。弹窗等待由调用方给出的 deadline 封顶
/// （单条命令预算与会话预算取较小者）。
///
/// `Start` 在预算内无应答归一为 `Pending`（授权尚未确认）：这不是通道失效，
/// dispatcher 不摘除候选，用户确认后下一条命令可再次尝试；其余步骤超时才是
/// 通道建立失败。重试只会再弹一次窗，故 Pending 不重试。
async fn drain_response(
    stream: &mut zbus::MessageStream,
    step: &str,
    deadline: tokio::time::Instant,
) -> std::result::Result<HashMap<String, zbus::zvariant::OwnedValue>, SetupFailure> {
    let msg = match tokio::time::timeout_at(deadline, stream.next()).await {
        Ok(Some(Ok(msg))) => msg,
        Ok(Some(Err(e))) => {
            return Err(SetupFailure::Unavailable(AgentShellError::DBus(format!(
                "signal read: {e}"
            ))))
        }
        Ok(None) => {
            return Err(SetupFailure::Unavailable(AgentShellError::DBus(
                "portal signal stream ended".to_string(),
            )))
        }
        Err(_) if step == "Start" => {
            return Err(SetupFailure::Pending(AgentShellError::Permission(
                "portal RemoteDesktop Start 授权未在本次命令预算内确认；请在门户对话框中允许后重试"
                    .into(),
            )))
        }
        Err(_) => {
            return Err(SetupFailure::Unavailable(AgentShellError::Timeout(
                format!("libei: portal RemoteDesktop {step} response exceeded the setup budget"),
            )))
        }
    };
    let (code, results) = parse_response_body(&msg)?;
    if code != 0 {
        return Err(SetupFailure::Unavailable(AgentShellError::Permission(
            format!("portal RemoteDesktop {step} authorization refused (code {code})"),
        )));
    }
    Ok(results)
}

/// 在方法返回后订阅 `returned_path` 上的 Response（路径失配兜底，存在竞态）。
async fn wait_response_on_path(
    bus: &zbus::Connection,
    returned_path: &str,
    step: &str,
    deadline: tokio::time::Instant,
) -> std::result::Result<HashMap<String, zbus::zvariant::OwnedValue>, SetupFailure> {
    let path = zbus::zvariant::ObjectPath::try_from(returned_path)
        .map_err(|e| AgentShellError::DBus(format!("request path: {e}")))?;
    let mut stream = subscribe_response(bus, &path).await?;
    drain_response(&mut stream, step, deadline).await
}

/// 等待 `Response` 并返回 results：返回路径与预算路径一致时消费预订阅流，
/// 失配时改在返回路径上重建订阅（非默认路径）。
async fn await_response(
    stream: &mut zbus::MessageStream,
    bus: &zbus::Connection,
    predicted: &str,
    returned: &str,
    step: &str,
    deadline: tokio::time::Instant,
) -> std::result::Result<HashMap<String, zbus::zvariant::OwnedValue>, SetupFailure> {
    if predicted == returned {
        drain_response(stream, step, deadline).await
    } else {
        tracing::warn!(
            expected = %predicted,
            got = returned,
            "portal request path mismatch; re-subscribing on returned path"
        );
        wait_response_on_path(bus, returned, step, deadline).await
    }
}

fn portal_err(e: zbus::Error) -> AgentShellError {
    AgentShellError::DBus(format!("RemoteDesktop call: {e}"))
}

#[async_trait]
impl InputService for LibeiInput {
    fn name(&self) -> &'static str {
        "libei"
    }

    async fn is_available(&self) -> bool {
        // 构造已验证 portal 在场；可用性即「会话能建立或已建立」。
        // 未连接 → 保持 true 惰性重试，真实失败在首次注入时以错误暴露。
        true
    }

    async fn health(&self) -> agent_shell_core::component::ComponentHealth {
        use agent_shell_core::component::ComponentHealth;
        let g = self.state.lock().await;
        if let Some(session) = g.session.as_ref() {
            // 会话存在不等于全部操作可用：按实际绑到的接口报告缺项，
            // 避免 doctor 就绪而点击/滚动/绝对定位返回 BackendUnavailable。
            let missing = session.missing_capabilities();
            if missing.is_empty() {
                ComponentHealth::Healthy
            } else {
                ComponentHealth::Degraded(format!(
                    "portal session missing EI capabilities: {}",
                    missing.join(", ")
                ))
            }
        } else if g.pending.is_some() {
            // 弹窗已发出、授权尚未确认：会话资源仍在，等用户允许即可接续。
            ComponentHealth::Degraded(
                "portal RemoteDesktop 授权待确认（会话已保留，下一条命令可接续）".into(),
            )
        } else {
            ComponentHealth::Degraded(
                "portal present but EIS session not yet established (lazy)".into(),
            )
        }
    }

    async fn ensure_ready(&self, op: Op<'_>) -> Result<(), SetupFailure> {
        // 建立 portal→EI 会话（可能触发授权弹窗）但不注入：会话建立失败是通道级
        // 失败（重试后降级）；建会话后才发现的能力缺口是另一类事实——门户可能
        // 只授权部分设备（如仅 pointer 而缺 keyboard），此时该后端对本次操作
        // 不可用但仍是其它操作的候选，不得因此被摘除。
        let s = self.ensure_connected_setup().await?;
        let missing = match op {
            Op::Key(_) | Op::Text(..) => s.keyboard_device().is_none().then_some("keyboard"),
            Op::Move(..) => s
                .absolute_pointer_device()
                .is_none()
                .then_some("absolute-pointer"),
            Op::Click(_) => s.button_device().is_none().then_some("button"),
            Op::Scroll(..) => s.scroll_device().is_none().then_some("scroll"),
        };
        match missing {
            Some(cap) => Err(SetupFailure::CapabilityGap(format!(
                "libei: no {cap} capability"
            ))),
            None => Ok(()),
        }
    }

    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        self.send_key_combo(combo).await
    }

    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        for c in text.chars() {
            let (code, shift) = char_to_evdev(c)
                .ok_or_else(|| AgentShellError::Input(format!("cannot type character {c:?}")))?;
            let mut seq: Vec<(u32, bool)> = Vec::new();
            if shift {
                seq.push((42, true));
            }
            seq.push((code, true));
            seq.push((code, false));
            if shift {
                seq.push((42, false));
            }
            self.key_sequence(&seq).await?;
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(u64::from(delay_ms))).await;
            }
        }
        Ok(())
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        let s = self.ensure_connected().await?;
        if let Some(device) = s.absolute_pointer_device() {
            let pa = device
                .pointer_absolute
                .as_ref()
                .expect("absolute_pointer_device carries pointer_absolute");
            // EI 绝对指针使用设备坐标系的逻辑像素（与 region 同单位），
            // 直接对应 CLI 的桌面逻辑坐标，不能再归一化。
            pa.motion_absolute(x as f32, y as f32);
            device.device.device().frame(0, Self::now_us());
            s.inner.flush().map_err(flush_err)
        } else {
            // 能力缺失（本会话无绝对指针设备）——非「参数语义不满足」，
            // 归 BackendUnavailable 而非 NotImplemented。
            Err(AgentShellError::BackendUnavailable(
                "libei: no absolute-pointer capability".into(),
            ))
        }
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.click_button(evdev_mouse_button(button)).await
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        let s = self.ensure_connected().await?;
        let device = s.scroll_device().ok_or_else(|| {
            AgentShellError::BackendUnavailable("libei: no scroll capability".into())
        })?;
        let sc = device
            .scroll
            .as_ref()
            .expect("scroll_device carries scroll");
        if dy != 0 {
            sc.scroll(0.0, dy as f32);
            device.device.device().frame(0, Self::now_us());
        }
        if dx != 0 {
            sc.scroll(dx as f32, 0.0);
            device.device.device().frame(0, Self::now_us());
        }
        s.inner.flush().map_err(flush_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_capabilities_cover_keyboard_and_all_pointer_actions() {
        let flags = required_ei_capabilities();
        for cap in [
            DeviceCapability::Keyboard,
            DeviceCapability::Pointer,
            DeviceCapability::PointerAbsolute,
            DeviceCapability::Button,
            DeviceCapability::Scroll,
        ] {
            assert!(
                flags.contains(cap),
                "bind mask must request every used interface"
            );
        }
        assert!(!flags.contains(DeviceCapability::Touch));
    }

    #[test]
    fn session_budget_stays_below_command_and_operation_budgets() {
        // C-154：建立过程预算必须小于单命令预算，否则 daemon 的回应会晚于
        // CLI 的 5s 调用预算；同时不得放大 dispatcher 的单次操作预算。
        assert!(LIBEI_PROBE_BUDGET <= LIBEI_SETUP_BUDGET);
        assert!(
            LIBEI_SETUP_BUDGET < crate::dispatcher::INPUT_OPERATION_BUDGET,
            "session setup must fit inside one input operation"
        );
        assert!(
            LIBEI_SETUP_BUDGET < agent_shell_core::fallback::DEFAULT_COMMAND_TIMEOUT,
            "session setup must fit inside the C-154 command budget"
        );
    }

    #[test]
    fn missing_connect_to_eis_reason_only_for_capability_gap() {
        // 仅「portal 在场但缺 ConnectToEIS」是可诊断根因；headless 无 portal /
        // 无 session bus 是常态，dispatcher 不记录，doctor 回退友好文案。
        let missing = AgentShellError::BackendUnavailable(MISSING_CONNECT_TO_EIS.into());
        assert_eq!(
            missing_connect_to_eis_reason(&missing),
            Some(MISSING_CONNECT_TO_EIS)
        );
        assert_eq!(
            missing_connect_to_eis_reason(&AgentShellError::BackendUnavailable(
                "xdg-desktop-portal not running".into()
            )),
            None
        );
        assert_eq!(
            missing_connect_to_eis_reason(&AgentShellError::DBus("session bus: gone".into())),
            None
        );
    }

    #[test]
    fn connect_to_eis_detection_matches_only_method_declaration() {
        // 真实 portal 内省：method 元素带参数子节点，开标签即声明。
        let xml = r#"<node>
  <interface name="org.freedesktop.portal.RemoteDesktop">
    <method name="CreateSession">
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="o" name="handle" direction="out"/>
    </method>
    <method name="ConnectToEIS">
      <arg type="o" name="session_handle" direction="in"/>
      <arg type="h" name="fd" direction="out"/>
    </method>
  </interface>
</node>"#;
        assert!(xml_advertises_connect_to_eis(xml));

        // 缺 ConnectToEIS（老 portal，仅到 Start）。
        let xml = r#"<node>
  <interface name="org.freedesktop.portal.RemoteDesktop">
    <method name="CreateSession">
      <arg type="a{sv}" name="options" direction="in"/>
    </method>
    <method name="Start">
      <arg type="o" name="session_handle" direction="in"/>
    </method>
  </interface>
</node>"#;
        assert!(!xml_advertises_connect_to_eis(xml));

        // 文档/注解中提及 ConnectToEIS 不算方法声明。
        let xml = r#"<node>
  <interface name="org.freedesktop.portal.RemoteDesktop">
    <annotation name="doc" value="ConnectToEIS is unavailable here"/>
  </interface>
</node>"#;
        assert!(!xml_advertises_connect_to_eis(xml));
    }

    #[test]
    fn encode_sender_part_strips_colon_and_dots() {
        // ":1.42" → "1_42"——portal handle_token 路径编码规则。
        assert_eq!(encode_sender_part(":1.42"), "1_42");
        assert_eq!(encode_sender_part(":1.99"), "1_99");
        assert_eq!(encode_sender_part(":1.0"), "1_0");
    }

    #[test]
    fn request_path_encodes_sender_and_token() {
        let p = request_path(
            "/org/freedesktop/portal/desktop/request/",
            "1_42",
            "agent_shell_input_create_123",
        )
        .expect("valid request path");
        assert_eq!(
            p.as_str(),
            "/org/freedesktop/portal/desktop/request/1_42/agent_shell_input_create_123"
        );
    }

    #[test]
    fn parse_response_body_decodes_portal_vardict() {
        // 规范签名 `(ua{sv})`：旧代码按 `(uv)` 反序列化会报 Signature mismatch
        // （got '(ua{sv})', expected '(uv)'），此测试锚定 `(ua{sv})` 解析路径，
        // 防止 TC-201/202/204 的根因回退。
        let mut results = HashMap::<&str, zbus::zvariant::Value>::new();
        results.insert(
            "session_handle",
            zbus::zvariant::Value::from("/org/freedesktop/portal/desktop/session/1_42/tok"),
        );
        results.insert("devices", zbus::zvariant::Value::from(3u32));
        let body = (0u32, results);
        let msg = zbus::Message::signal(
            "/org/freedesktop/portal/desktop/request/1_42/tok",
            "org.freedesktop.portal.Request",
            "Response",
        )
        .expect("signal builder")
        .build(&body)
        .expect("build (ua{sv}) message");

        let (code, parsed) = parse_response_body(&msg).expect("parse (ua{sv})");
        assert_eq!(code, 0);
        let handle = parsed
            .get("session_handle")
            .and_then(|v| v.downcast_ref::<&str>().ok())
            .expect("session_handle present");
        assert_eq!(handle, "/org/freedesktop/portal/desktop/session/1_42/tok");
        let devices = parsed
            .get("devices")
            .and_then(|v| v.downcast_ref::<u32>().ok())
            .expect("devices present");
        assert_eq!(devices, 3);
    }

    #[tokio::test]
    async fn silent_eis_peer_times_out_and_releases_worker() {
        use std::io::Read;

        // 静默 EIS peer：接受连接但不发送任何初始握手数据。外层 JoinHandle
        // 超时不会中止阻塞任务，因此预算必须在 worker 内的握手流上生效，
        // 且 worker 退出时释放 fd。
        let (client, mut peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let std_fd: std::os::fd::OwnedFd = client.into();
        let owned = zbus::zvariant::OwnedFd::from(std_fd);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(120);
        let started = std::time::Instant::now();
        let worker = tokio::task::spawn_blocking(move || -> Result<EiNegotiated> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| AgentShellError::Input(format!("libei runtime: {e}")))?
                .block_on(async move { ei_handshake_and_negotiate(owned, deadline).await })
        });
        let joined = tokio::time::timeout(std::time::Duration::from_secs(2), worker)
            .await
            .expect("worker must exit after the handshake deadline")
            .expect("worker must not panic");
        assert!(
            matches!(&joined, Err(AgentShellError::Timeout(m)) if m.contains("handshake")),
            "silent EIS peer must fail with a bounded handshake timeout"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "handshake must be bounded inside the worker: {:?}",
            started.elapsed()
        );

        // worker 退出必须释放 fd：先消费客户端握手字节，随后应读到 EOF。
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .expect("read timeout");
        let mut buf = [0u8; 64];
        let mut saw_eof = false;
        for _ in 0..64 {
            match peer.read(&mut buf) {
                Ok(0) => {
                    saw_eof = true;
                    break;
                }
                Ok(_) => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                // ECONNRESET 同样说明对端已关闭。
                Err(_) => {
                    saw_eof = true;
                    break;
                }
            }
        }
        assert!(
            saw_eof,
            "worker must close its EIS socket so the peer observes EOF"
        );
    }
}
