//! 事件流通道（设计文档 §9.3：`.socket2.sock` 长期连接，行协议
//! `EVENT>>DATA\n`）。
//!
//! 映射（§9.5 四种核心事件 + 全屏广播）：
//!
//! | Hyprland 行 | DesktopEvent |
//! |-------------|--------------|
//! | `openwindow>>ADDR,WS,CLASS,TITLE` | `WindowOpened`（缓存 upsert） |
//! | `closewindow>>ADDR` | `WindowClosed`（缓存移除） |
//! | `activewindowv2>>ADDR` | `WindowFocused` |
//! | `workspacev2>>WSID,WSNAME` | `WorkspaceChanged` |
//! | `fullscreen>>0/1` | `FullscreenChanged`（作用于当前焦点窗口） |
//!
//! 连接断开自动重连（1s 退避）；未识别事件跳过不阻塞流。

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Mutex};

use agent_shell_core::event::{DesktopEvent, EventSource};
use agent_shell_core::types::{Rect, WindowInfo, WorkspaceInfo};

use crate::cache::{window_id, WindowCache};

/// 事件通道容量（背压上限；事件洪峰时丢新保旧由 mpsc 满载 send 失败表达）。
const CHANNEL_CAPACITY: usize = 256;
/// 断线重连间隔。
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// 事件流句柄：持有关闭旗标，drop 不自动停 task（组件生命周期内常驻）。
#[derive(Debug)]
pub struct EventTaskHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl EventTaskHandle {
    /// 停止事件循环（幂等）。
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 是否已请求停止。
    pub fn is_stopped(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for EventTaskHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动 socket2 事件任务：返回归一化流读取端与任务句柄。
///
/// `focused_address` 共享当前焦点地址（activewindowv2 更新，fullscreen
/// 广播消费），由组件层与缓存查询共享。
pub fn spawn_event_task(
    event_socket_path: std::path::PathBuf,
    cache: WindowCache,
    focused_address: Arc<Mutex<String>>,
) -> (mpsc::Receiver<DesktopEvent>, EventTaskHandle) {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = stop.clone();
    tokio::spawn(async move {
        while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            match tokio::net::UnixStream::connect(&event_socket_path).await {
                Ok(stream) => {
                    let mut lines = BufReader::new(stream).lines();
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(RECONNECT_DELAY), if stop_flag.load(std::sync::atomic::Ordering::Relaxed) => break,
                            line = lines.next_line() => match line {
                                Ok(Some(l)) => {
                                    if let Some(evt) = parse_line(&l, &cache, &focused_address).await {
                                        if let Err(e) = tx.try_send(evt) {
                                            // 通道满或无订阅端：丢弃事件不阻塞，
                                            // 缓存仍由 parse_line 内部更新（Radian 审查 #4）。
                                            tracing::trace!("hyprland event dropped: {e}");
                                        }
                                    }
                                }
                                Ok(None) => break, // 服务端断开 → 外层重连
                                Err(e) => {
                                    tracing::warn!("hyprland event socket read error: {e}");
                                    break;
                                }
                            }
                        }
                        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("hyprland event socket connect failed: {e}");
                    tokio::time::sleep(RECONNECT_DELAY).await;
                }
            }
        }
    });
    (rx, EventTaskHandle { stop })
}

/// 解析一行 `EVENT>>DATA` 为归一化事件并同步更新窗口缓存。
pub async fn parse_line(
    line: &str,
    cache: &WindowCache,
    focused_address: &Mutex<String>,
) -> Option<DesktopEvent> {
    let (name, data) = line.split_once(">>")?;
    let source = EventSource::Hyprland;
    let occurred_at = std::time::Instant::now();
    match name {
        "openwindow" => {
            // ADDR,WORKSPACE,CLASS,TITLE（TITLE 可含逗号——splitn 限 4 段）
            let mut parts = data.splitn(4, ',');
            let addr = parts.next()?.trim().to_string();
            let ws: u32 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
            let class = parts.next().unwrap_or("").to_string();
            let title = parts.next().unwrap_or("").to_string();
            let stacking = cache.len() as u32 + 1;
            cache.upsert_minimal(&addr, ws, &class, &title, stacking);
            Some(DesktopEvent::WindowOpened {
                info: cache.get(&window_id(&addr))?,
                source,
                occurred_at,
            })
        }
        "closewindow" => {
            let addr = data.trim();
            let id = window_id(addr);
            cache.remove(addr);
            Some(DesktopEvent::WindowClosed {
                id,
                source,
                occurred_at,
            })
        }
        "activewindowv2" => {
            // 空载荷 = 焦点落到无窗口表面（桌面/layer-shell）。
            let addr = data.trim().to_string();
            *focused_address.lock().await = addr.clone();
            if addr.is_empty() {
                return None;
            }
            let info = window_info_for_focus(cache, &addr);
            Some(DesktopEvent::WindowFocused {
                info,
                source,
                occurred_at,
            })
        }
        "workspacev2" => {
            // WSID,WSNAME
            let mut parts = data.splitn(2, ',');
            let id = parts.next()?.trim().to_string();
            let name = parts.next().unwrap_or("").trim().to_string();
            Some(DesktopEvent::WorkspaceChanged {
                info: WorkspaceInfo {
                    id: agent_shell_core::types::WorkspaceId {
                        native_id: id,
                        de_type: agent_shell_core::DesktopEnvironment::Hyprland,
                    },
                    name,
                    number: 0,
                    is_active: true,
                    monitor_ids: Vec::new(),
                    window_ids: Vec::new(),
                },
                source,
                occurred_at,
            })
        }
        "fullscreen" => {
            // 广播当前焦点窗口的全屏切换（0=退出，1=进入）。
            let on = data.trim() == "1";
            let addr = focused_address.lock().await.clone();
            if addr.is_empty() {
                return None;
            }
            cache.set_fullscreen(&addr, on);
            Some(DesktopEvent::FullscreenChanged {
                enabled: on,
                window_id: Some(window_id(&addr)),
                source,
                occurred_at,
            })
        }
        _ => None,
    }
}

/// 聚焦事件的 WindowInfo：缓存命中用完整条目，未命中以最小信息构造
/// （clients -j 快照尚未校准前的 open-before-list 窗口期）。
fn window_info_for_focus(cache: &WindowCache, addr: &str) -> WindowInfo {
    if let Some(w) = cache.get(&window_id(addr)) {
        return w;
    }
    WindowInfo {
        id: window_id(addr),
        title: String::new(),
        app_id: String::new(),
        pid: 0,
        geometry: Rect::default(),
        frame_geometry: Rect::default(),
        states: Vec::new(),
        workspace_id: None,
        monitor_id: None,
        stacking_order: 0,
        desktop_file: None,
        window_type: agent_shell_core::types::WindowType::Normal,
        icon_geometry: None,
        keep_above: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (WindowCache, Mutex<String>) {
        (WindowCache::default(), Mutex::new(String::new()))
    }

    /// §9.5 行格式四核心事件 + fullscreen 的端到端解析与缓存联动。
    #[tokio::test]
    async fn parses_core_event_lines_and_updates_cache() {
        let (cache, focus) = fixture().await;

        let e = parse_line("openwindow>>abc123,2,foot,my terminal", &cache, &focus)
            .await
            .expect("openwindow");
        assert!(matches!(e, DesktopEvent::WindowOpened { .. }));
        assert_eq!(
            cache.get(&window_id("abc123")).unwrap().title,
            "my terminal"
        );

        let e = parse_line("activewindowv2>>abc123", &cache, &focus)
            .await
            .unwrap();
        assert!(matches!(e, DesktopEvent::WindowFocused { .. }));
        assert_eq!(*focus.lock().await, "abc123");

        let e = parse_line("workspacev2>>3,web", &cache, &focus)
            .await
            .unwrap();
        match e {
            DesktopEvent::WorkspaceChanged { info, .. } => {
                assert_eq!(info.id.native_id, "3");
                assert_eq!(info.name, "web");
            }
            other => panic!("unexpected {other:?}"),
        }

        let e = parse_line("fullscreen>>1", &cache, &focus).await.unwrap();
        assert!(matches!(
            e,
            DesktopEvent::FullscreenChanged { enabled: true, .. }
        ));
        assert!(cache
            .get(&window_id("abc123"))
            .unwrap()
            .states
            .contains(&agent_shell_core::types::WindowState::FullScreen));

        let e = parse_line("closewindow>>abc123", &cache, &focus)
            .await
            .unwrap();
        assert!(matches!(e, DesktopEvent::WindowClosed { .. }));
        assert!(cache.get(&window_id("abc123")).is_none());
    }

    /// 标题含逗号不被截断（splitn 限 4 段）。
    #[tokio::test]
    async fn title_with_comma_survives() {
        let (cache, focus) = fixture().await;
        parse_line("openwindow>>ff01,1,cargo,build, test, lint", &cache, &focus)
            .await
            .unwrap();
        assert_eq!(
            cache.get(&window_id("ff01")).unwrap().title,
            "build, test, lint"
        );
    }

    /// 未识别事件与空焦点广播被静默跳过。
    #[tokio::test]
    async fn unknown_and_empty_events_skipped() {
        let (cache, focus) = fixture().await;
        assert!(parse_line("configreloaded>>", &cache, &focus)
            .await
            .is_none());
        assert!(parse_line("activewindowv2>>", &cache, &focus)
            .await
            .is_none());
        assert!(parse_line("garbage-line", &cache, &focus).await.is_none());
    }
}
