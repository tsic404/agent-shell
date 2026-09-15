//! KWin X11 会话 EWMH 事件源（设计文档 §18.2，X11 会话的事件通道）。
//!
//! KWin 5.x 部分 X11 会话不注册 `/Scripting` 对象路径，
//! `event_monitor.js`（经 `loadScript`）无法启动，`events subscribe` 失败。
//! X11 会话的事件通道改走 EWMH：在 root window 上订阅 `PropertyNotify`，
//! 差分 `_NET_CLIENT_LIST_STACKING`（缺失回退 `_NET_CLIENT_LIST`）与
//! `_NET_ACTIVE_WINDOW`，产出与 `event_monitor.js` 等价的
//! [`RawEvent::KWinWindowAdded`] / [`KWinWindowRemoved`] /
//! [`KWinActiveWindowChanged`]，交由 daemon 侧 `EventNormalizer` 归一化。
//!
//! 事件循环在独立线程阻塞读 X11 socket（`wait_for_event`），窗口 id 输出
//! 十进制字符串——与 EWMH 枚举及 Scripting `internalId.toString()`
//! 口径一致，保证事件 id 可直接喂给窗口直捕。

use std::sync::Arc;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::WindowId;
use agent_shell_core::DesktopEnvironment;
use agent_shell_displayserver_x11::{EwmhAtoms, X11DisplayServer};
use async_trait::async_trait;
use event::{EventSource, RawEvent, RawSource};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::sync::Mutex;
use tokio::sync::mpsc;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt, EventMask};
use x11rb::protocol::Event;

/// 已启动的 EWMH 事件监视器：持接收端（一次性取走）与事件线程句柄。
///
/// 事件线程常驻组件生命周期，不设停止旗标——`wait_for_event` 阻塞读 socket，
/// 进程退出时随线程终止。`_thread` 仅用于保持句柄（drop 即 detach）。
pub struct EwmhEventMonitor {
    rx: Option<mpsc::UnboundedReceiver<RawEvent>>,
    _thread: std::thread::JoinHandle<()>,
}

impl EwmhEventMonitor {
    /// 取走一次性接收端（与 Scripting `take_raw_event_rx` 同款语义）。
    pub(crate) fn take_rx(&mut self) -> Option<mpsc::UnboundedReceiver<RawEvent>> {
        self.rx.take()
    }
}

/// KWin X11 EWMH 事件的原始事件源（[`RawSource`] 的 X11 实现）。
pub struct EwmhRawSource {
    rx: Mutex<Option<mpsc::UnboundedReceiver<RawEvent>>>,
}

impl EwmhRawSource {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<RawEvent>) -> Self {
        Self {
            rx: Mutex::new(Some(rx)),
        }
    }
}

impl RawSource for EwmhRawSource {
    fn source_name(&self) -> &'static str {
        "kwin-ewmh"
    }

    fn source_kind(&self) -> EventSource {
        EventSource::KWinX11
    }

    fn events(&self) -> BoxStream<'static, RawEvent> {
        let rx = self
            .rx
            .lock()
            .expect("kwin ewmh raw source lock poisoned")
            .take();
        let Some(rx) = rx else {
            // 已取走过（重复调用）——空流，与 EventNormalizer 单次 events() 契约一致。
            return futures::stream::empty().boxed();
        };
        futures::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|raw| (raw, rx)) },
        )
        .boxed()
    }
}

/// KWin X11 EWMH 事件的近似映射流（core [`agent_shell_core::EventStream`]）。
///
/// `subscribe()` 的 X11 分支消费本流，映射语义与 Scripting `KWinEventStream`
/// 对齐（同为 T3b 前近似流）：`WindowOpened`/`WindowFocused` 需要完整
/// [`WindowInfo`]，本层无 resolver 拿不到，暂以 [`DesktopEvent::WindowClosed`]
/// 承载窗口 id 作为「id 出现」信号（消费方勿据以移除缓存）。
pub struct EwmhEventStream {
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<RawEvent>>,
}

impl EwmhEventStream {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<RawEvent>) -> Self {
        Self {
            rx: tokio::sync::Mutex::new(rx),
        }
    }
}

#[async_trait]
impl agent_shell_core::EventStream for EwmhEventStream {
    async fn next_event(&self) -> Option<agent_shell_core::event::DesktopEvent> {
        loop {
            let raw = self.rx.lock().await.recv().await?;
            if let Some(event) = map_raw_to_core(raw) {
                return Some(event);
            }
        }
    }
}

/// [`RawEvent`] → core [`DesktopEvent`]（近似映射，见 [`EwmhEventStream`]）。
///
/// `KWinWindowAdded` / `KWinActiveWindowChanged` 映射为 `WindowClosed`（仅承载
/// id 的「id 出现」信号）；`KWinWindowRemoved` 与失焦（`id: None`）跳过——
/// 与 Scripting `KWinEventStream` 对 `windowClosed` 的跳过口径一致。
fn map_raw_to_core(raw: RawEvent) -> Option<agent_shell_core::event::DesktopEvent> {
    let id = match raw {
        RawEvent::KWinWindowAdded { id } | RawEvent::KWinActiveWindowChanged { id: Some(id) } => id,
        _ => return None,
    };
    Some(agent_shell_core::event::DesktopEvent::WindowClosed {
        id: WindowId {
            native_id: id,
            de_type: DesktopEnvironment::KDE,
        },
        source: agent_shell_core::event::EventSource::KWinX11,
        occurred_at: std::time::Instant::now(),
    })
}

/// 启动 EWMH 事件监视器：订阅 root `PropertyNotify`，spawn 事件线程。
///
/// 订阅失败（X 请求错误）返回 Err；线程 spawn 后不可失败。首次调用即绑定
/// root 事件掩码——本连接为 daemon 独占，覆盖 `PROPERTY_CHANGE` 掩码不与其他
/// 客户端冲突（掩码按连接隔离，非按 root 窗口全局共享）。
pub fn spawn_ewmh_monitor(x11: &Arc<X11DisplayServer>) -> Result<EwmhEventMonitor> {
    let conn = x11.connection();
    let root = x11.root_window();
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(|e| AgentShellError::DBus(format!("x11 request: {e}")))?;
    conn.flush()
        .map_err(|e| AgentShellError::DBus(format!("x11 request: {e}")))?;

    let (tx, rx) = mpsc::unbounded_channel();
    let x11 = Arc::clone(x11);
    let thread = std::thread::spawn(move || run_event_loop(x11, tx));
    Ok(EwmhEventMonitor {
        rx: Some(rx),
        _thread: thread,
    })
}

/// 事件线程：阻塞读 X11 事件，差分 EWMH 属性并推送原始事件。
///
/// 初始状态在订阅后读取，不产生启动伪事件；后续每收到相关 PropertyNotify
/// 重读**触发属性**并差分——`_NET_CLIENT_LIST` 与 `_NET_CLIENT_LIST_STACKING`
/// 更新不同步时，读触发属性保证拿到最新集合，避免漏报增删。属性读取失败
/// 直接终止线程：吞错误会以空表触发全量 removed/add 伪事件风暴，而连接异常
/// 本应终止事件流。
fn run_event_loop(x11: Arc<X11DisplayServer>, tx: mpsc::UnboundedSender<RawEvent>) {
    let conn = x11.connection();
    let root = x11.root_window();
    let atoms = *x11.atoms();

    // 初始快照：读取失败（连接异常）即终止，无基线可差分。
    let mut prev_clients = match initial_client_list(x11.as_ref()) {
        Some(clients) => clients,
        None => return,
    };
    let mut prev_active = match x11.get_active_window() {
        Ok(active) => active,
        Err(_) => return,
    };

    while let Ok(event) = conn.wait_for_event() {
        let Event::PropertyNotify(pn) = event else {
            continue;
        };
        if pn.window != root {
            continue;
        }
        if pn.atom == atoms._NET_CLIENT_LIST || pn.atom == atoms._NET_CLIENT_LIST_STACKING {
            let cur = match read_client_list_by_atom(x11.as_ref(), pn.atom, &atoms) {
                Ok(clients) => clients,
                Err(_) => break,
            };
            let (added, removed) = diff_window_sets(&prev_clients, &cur);
            for w in added {
                let _ = tx.send(RawEvent::KWinWindowAdded { id: w.to_string() });
            }
            for w in removed {
                let _ = tx.send(RawEvent::KWinWindowRemoved { id: w.to_string() });
            }
            prev_clients = cur;
        } else if pn.atom == atoms._NET_ACTIVE_WINDOW {
            let cur = match x11.get_active_window() {
                Ok(active) => active,
                Err(_) => break,
            };
            if cur != prev_active {
                // 与 event_monitor.js 一致：仅在存在焦点窗口时推送（激活信号
                // 回调里的 `if (w)` 守卫），失去焦点不产出事件。
                if let Some(w) = cur {
                    let _ = tx.send(RawEvent::KWinActiveWindowChanged {
                        id: Some(w.to_string()),
                    });
                }
                prev_active = cur;
            }
        }
    }
}

/// 初始客户列表快照：`_NET_CLIENT_LIST_STACKING` 优先（缺失/失败回退
/// `_NET_CLIENT_LIST`）。任一读取失败（连接异常）返回 `None`。
fn initial_client_list(x11: &X11DisplayServer) -> Option<Vec<u32>> {
    match x11.get_client_list_stacking() {
        Ok(stacking) if !stacking.is_empty() => Some(stacking),
        _ => x11.get_client_list().ok(),
    }
}

/// 按触发 atom 读同源客户列表（PropertyNotify 的 `pn.atom`）。
fn read_client_list_by_atom(
    x11: &X11DisplayServer,
    atom: u32,
    atoms: &EwmhAtoms,
) -> Result<Vec<u32>> {
    if atom == atoms._NET_CLIENT_LIST_STACKING {
        x11.get_client_list_stacking()
    } else {
        x11.get_client_list()
    }
}

/// 窗口列表差分：返回 (新增, 移除)，保持各自列表顺序。
///
/// 纯函数（可单测）；`prev`/`cur` 为十进制窗口 id 的无序集合比较。
fn diff_window_sets(prev: &[u32], cur: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let added = cur.iter().filter(|w| !prev.contains(w)).copied().collect();
    let removed = prev.iter().filter(|w| !cur.contains(w)).copied().collect();
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_window_sets_detects_added_and_removed() {
        let (added, removed) = diff_window_sets(&[1, 2, 3], &[2, 3, 4]);
        assert_eq!(added, vec![4]);
        assert_eq!(removed, vec![1]);
    }

    #[test]
    fn diff_window_sets_no_change_is_empty() {
        let (added, removed) = diff_window_sets(&[1, 2, 3], &[1, 2, 3]);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_window_sets_full_replacement() {
        let (added, removed) = diff_window_sets(&[1], &[2, 3]);
        assert_eq!(added, vec![2, 3]);
        assert_eq!(removed, vec![1]);
    }

    #[test]
    fn map_raw_to_core_maps_open_focus_to_closed_and_skips_others() {
        use agent_shell_core::event::{DesktopEvent, EventSource as CoreEventSource};
        // windowOpened → WindowClosed（id 出现信号，与 Scripting KWinEventStream 对齐）。
        let opened = map_raw_to_core(RawEvent::KWinWindowAdded { id: "42".into() });
        let DesktopEvent::WindowClosed { id, source, .. } = opened.unwrap() else {
            panic!("expected WindowClosed for windowOpened");
        };
        assert_eq!(id.native_id, "42");
        assert_eq!(id.de_type, DesktopEnvironment::KDE);
        assert_eq!(source, CoreEventSource::KWinX11);

        // windowFocused → WindowClosed（近似）。
        let focused = map_raw_to_core(RawEvent::KWinActiveWindowChanged {
            id: Some("7".into()),
        });
        assert!(matches!(focused, Some(DesktopEvent::WindowClosed { .. })));

        // windowClosed / 失焦 → 跳过。
        assert!(map_raw_to_core(RawEvent::KWinWindowRemoved { id: "42".into() }).is_none());
        assert!(map_raw_to_core(RawEvent::KWinActiveWindowChanged { id: None }).is_none());
    }
}
