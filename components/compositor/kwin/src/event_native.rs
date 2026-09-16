//! KWin Wayland 原生事件源（设计文档 §18.2，Wayland 会话的事件通道）。
//!
//! KWin 5.x Wayland 会话不注册 `/Scripting` 时，Scripting 长驻事件脚本
//! （`event_monitor.js` 经 `loadScript`）无法启动，`events subscribe` 失败。
//! 本模块改走 `org_kde_plasma_window_management` 协议事件：
//! - `window_with_uuid`（窗口映射）→ [`RawEvent::KWinWindowAdded`]；
//! - `org_kde_plasma_window.unmapped`（窗口卸载）→ [`RawEvent::KWinWindowRemoved`]。
//!
//! 事件队列由独立线程短超时 poll 驱动（`prepare_read` + `poll(timeout)`），
//! 与请求队列分离——window_mgmt 在同一 Wayland 连接上做第二次绑定（「only
//! one client」按 wl_client 计，同一客户端多绑定合法，KWin `bind_resource`
//! 仅回发初始状态、不拒绝第二次绑定）。读保护（read guard）只在 poll 短超时
//! 窗口内持有，避免饿死主线程 on-demand roundtrip（否则 `events subscribe`
//! 存活期间独立 CLI 命令全部阻塞）。

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::time::Duration;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::WindowId;
use agent_shell_core::DesktopEnvironment;
use async_trait::async_trait;
use event::{EventSource, RawEvent, RawSource};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::sync::Mutex;
use tokio::sync::mpsc;
use wayland_client::globals::GlobalList;
use wayland_client::{Connection, Dispatch, Proxy as _, QueueHandle};
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window::OrgKdePlasmaWindow;
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window_management::OrgKdePlasmaWindowManagement;

/// window_management 绑定版本区间（get_window_by_uuid 需 v12）。
const WM_MIN: u32 = 12;
const WM_MAX: u32 = 18;

/// 事件线程 poll 超时（毫秒）：读保护只在此窗口内持有，主线程的 roundtrip
/// 在窗口间隙取回读保护——过长会饿死主线程（QA 实测独立 CLI 阻塞），过短
/// 增加空闲 CPU 占用。20ms 对窗口事件的投递延迟与 CPU 双端都可接受。
const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(20);

/// 原生事件派发状态：持事件发送端与窗口对象（保活以接收 unmapped）。
///
/// `manager` 保活 window_mgmt 代理——drop 后不再收到 window_with_uuid；
/// `windows` 保活逐窗 `org_kde_plasma_window` 代理——drop 后不再收到 unmapped。
struct WaylandEventState {
    tx: mpsc::UnboundedSender<RawEvent>,
    /// 保活 window_mgmt 代理——drop 后不再收到 window_with_uuid；事件处理走
    /// Dispatch 的 `proxy` 参数，故本字段仅持引用不读取。
    #[allow(dead_code)]
    manager: OrgKdePlasmaWindowManagement,
    windows: HashMap<String, OrgKdePlasmaWindow>,
}

impl Dispatch<OrgKdePlasmaWindowManagement, ()> for WaylandEventState {
    fn event(
        state: &mut Self,
        manager: &OrgKdePlasmaWindowManagement,
        event: <OrgKdePlasmaWindowManagement as wayland_client::Proxy>::Event,
        _udata: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window_management::Event;
        if let Event::WindowWithUuid { uuid, .. } = event {
            // 建窗口对象以接收 unmapped；保活于 state.windows。
            let win = manager.get_window_by_uuid(uuid.clone(), qh, ());
            state.windows.insert(uuid.clone(), win);
            let _ = state.tx.send(RawEvent::KWinWindowAdded { id: uuid });
        }
    }
}

impl Dispatch<OrgKdePlasmaWindow, ()> for WaylandEventState {
    fn event(
        state: &mut Self,
        window: &OrgKdePlasmaWindow,
        event: <OrgKdePlasmaWindow as wayland_client::Proxy>::Event,
        _udata: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window::Event;
        if let Event::Unmapped = event {
            let target = window.id();
            let uuid = state
                .windows
                .iter()
                .find(|(_, w)| w.id() == target)
                .map(|(uuid, _)| uuid.clone());
            if let Some(uuid) = uuid {
                state.windows.remove(&uuid);
                let _ = state.tx.send(RawEvent::KWinWindowRemoved { id: uuid });
            }
        }
    }
}

/// 已启动的 Wayland 原生事件监视器：持接收端（一次性取走）与事件线程句柄。
///
/// 事件线程常驻组件生命周期，不设停止旗标——短超时 poll 阻塞读 socket，
/// 进程退出时随线程终止。`_thread` 仅用于保持句柄（drop 即 detach）。
pub struct WaylandEventMonitor {
    rx: Option<mpsc::UnboundedReceiver<RawEvent>>,
    _thread: std::thread::JoinHandle<()>,
}

impl WaylandEventMonitor {
    /// 取走一次性接收端（与 EWMH `take_rx` 同款语义）。
    pub(crate) fn take_rx(&mut self) -> Option<mpsc::UnboundedReceiver<RawEvent>> {
        self.rx.take()
    }
}

/// 启动 Wayland 原生事件监视器：绑定 window_mgmt，spawn 事件线程。
///
/// 绑定失败（window_mgmt 未公布 / 版本过低 / 已被其他客户端绑定）返回 Err，
/// 上层回退 Scripting。线程 spawn 后不可失败。
pub(crate) fn spawn_wayland_monitor(
    conn: &Connection,
    globals: &GlobalList,
) -> Result<WaylandEventMonitor> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut queue = conn.new_event_queue::<WaylandEventState>();
    let qh = queue.handle();
    let manager = globals
        .bind::<OrgKdePlasmaWindowManagement, WaylandEventState, ()>(&qh, WM_MIN..=WM_MAX, ())
        .map_err(|e| {
            AgentShellError::BackendUnavailable(format!(
                "wayland event monitor: window_mgmt bind failed: {e}"
            ))
        })?;

    let state = WaylandEventState {
        tx,
        manager,
        windows: HashMap::new(),
    };
    let thread = std::thread::spawn(move || {
        let mut state = state;
        loop {
            // 派发已排队事件（inner queue + 事件队列）；dispatch 处理器可能
            // 发出 get_window_by_uuid 请求。
            if queue.dispatch_pending(&mut state).is_err() {
                break;
            }
            // 冲刷 bind / get_window_by_uuid 等请求。
            let _ = queue.flush();

            // 预备读取；None = inner queue 有待派发事件，回卷重试。
            let Some(guard) = queue.prepare_read() else {
                continue;
            };

            // 短超时 poll：不在读保护上无限阻塞——同一 wl_display 上主线程的
            // roundtrip（stacking_order_uuids/flush_queue）需要读保护，事件线程
            // 长期持锁会饿死主线程（QA 实测：订阅存活时独立 CLI 全部阻塞）。
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready =
                unsafe { libc::poll(&mut pfd, 1, EVENT_POLL_TIMEOUT.as_millis() as libc::c_int) };
            if ready > 0 {
                // 读 socket + 派发（read 消费 guard）。
                let _ = guard.read();
            } else {
                // 超时/错误：drop 取消预备读取，释放读保护。
                drop(guard);
            }

            // 派发刚读入的事件。
            if queue.dispatch_pending(&mut state).is_err() {
                break;
            }
            // 让出，给主线程取回读保护的机会。
            std::thread::yield_now();
        }
    });
    Ok(WaylandEventMonitor {
        rx: Some(rx),
        _thread: thread,
    })
}

/// KWin Wayland 原生事件的原始事件源（[`RawSource`] 的 Wayland 实现）。
pub struct WaylandRawSource {
    rx: Mutex<Option<mpsc::UnboundedReceiver<RawEvent>>>,
}

impl WaylandRawSource {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<RawEvent>) -> Self {
        Self {
            rx: Mutex::new(Some(rx)),
        }
    }
}

impl RawSource for WaylandRawSource {
    fn source_name(&self) -> &'static str {
        "kwin-wayland-native"
    }

    fn source_kind(&self) -> EventSource {
        EventSource::KWinWayland
    }

    fn events(&self) -> BoxStream<'static, RawEvent> {
        let rx = self
            .rx
            .lock()
            .expect("kwin wayland native raw source lock poisoned")
            .take();
        let Some(rx) = rx else {
            return futures::stream::empty().boxed();
        };
        futures::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|raw| (raw, rx)) },
        )
        .boxed()
    }
}

/// KWin Wayland 原生事件的近似映射流（core [`agent_shell_core::EventStream`]）。
///
/// `subscribe()` 的 Wayland 原生分支消费本流，映射语义与 EWMH/Scripting
/// 对齐（同为 T3b 前近似流）：`WindowOpened` 需完整 `WindowInfo`，本层无
/// resolver，暂以 [`DesktopEvent::WindowClosed`] 承载窗口 id 作为「id 出现」
/// 信号（消费方勿据以移除缓存）。
pub struct WaylandEventStream {
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<RawEvent>>,
}

impl WaylandEventStream {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<RawEvent>) -> Self {
        Self {
            rx: tokio::sync::Mutex::new(rx),
        }
    }
}

#[async_trait]
impl agent_shell_core::EventStream for WaylandEventStream {
    async fn next_event(&self) -> Option<agent_shell_core::event::DesktopEvent> {
        loop {
            let raw = self.rx.lock().await.recv().await?;
            if let Some(event) = map_raw_to_core(raw) {
                return Some(event);
            }
        }
    }
}

/// [`RawEvent`] → core [`DesktopEvent`]（近似映射，见 [`WaylandEventStream`]）。
///
/// `KWinWindowAdded` 映射为 `WindowClosed`（仅承载 id 的「id 出现」信号）；
/// `KWinWindowRemoved` 跳过——与 Scripting `KWinEventStream` / EWMH
/// `EwmhEventStream` 对 windowClosed 的跳过口径一致。
fn map_raw_to_core(raw: RawEvent) -> Option<agent_shell_core::event::DesktopEvent> {
    let id = match raw {
        RawEvent::KWinWindowAdded { id } => id,
        _ => return None,
    };
    Some(agent_shell_core::event::DesktopEvent::WindowClosed {
        id: WindowId {
            native_id: id,
            de_type: DesktopEnvironment::KDE,
        },
        source: agent_shell_core::event::EventSource::KWinWayland,
        occurred_at: std::time::Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::event::DesktopEvent;

    /// window_mgmt 绑定版本契约：get_window_by_uuid 需 v12（v12 才可按 uuid
    /// 建窗口对象以接收 unmapped），生成接口上界 v18。二次绑定依赖「同一
    /// wl_client 可多队列绑定」（KWin bind_resource 仅回发初始状态不拒绝）——
    /// 该假设由本常量区间锁定，改窄会让 v16（kwin 5.27.2）枚举失效。
    #[test]
    fn wm_version_range_covers_v12_through_v18() {
        assert_eq!(WM_MIN, 12, "get_window_by_uuid requires v12");
        assert_eq!(WM_MAX, 18, "generated interface upper bound is v18");
        // v16（kwin 5.27.2 公布版本）落在区间内——否则二次绑定失败。
        assert!((WM_MIN..=WM_MAX).contains(&16));
    }

    /// `window_with_uuid` 事件映射为「id 出现」信号（近似流：WindowOpened 需
    /// 完整 WindowInfo，本层无 resolver，与 EWMH/Scripting 近似流口径一致）。
    #[test]
    fn map_window_added_carries_kde_wayland_source() {
        let evt = map_raw_to_core(RawEvent::KWinWindowAdded {
            id: "uuid-1".to_string(),
        })
        .expect("KWinWindowAdded must map");
        match evt {
            DesktopEvent::WindowClosed { id, source, .. } => {
                assert_eq!(id.native_id, "uuid-1");
                assert_eq!(id.de_type, DesktopEnvironment::KDE);
                assert_eq!(source, agent_shell_core::event::EventSource::KWinWayland);
            }
            other => panic!("expected WindowClosed, got {other:?}"),
        }
    }

    /// `KWinWindowRemoved` 跳过（与 Scripting/EWMH 近似流对 close 的跳过口径一致）。
    #[test]
    fn map_window_removed_is_skipped() {
        assert!(map_raw_to_core(RawEvent::KWinWindowRemoved {
            id: "uuid-1".to_string()
        })
        .is_none());
    }
}
