//! Sway IPC 事件订阅（`event.rs`）。
//!
//! SUBSCRIBE 长连接订阅 `workspace` / `window` 两类事件：sway 对每条
//! 事件推送一帧 i3 IPC 报文（类型 0x8000..），payload 为
//! `{"change": "...", ...}` JSON。逐帧映射到统一 [`DesktopEvent`]：
//!
//! | sway 事件 | 统一事件 |
//! |-----------|---------|
//! | `workspace::init`    | `WorkspaceChanged`（新工作区激活） |
//! | `workspace::focus`   | `WorkspaceChanged` |
//! | `workspace::empty`   | `WorkspaceListChanged`（工作区移除） |
//! | `window::new`        | `WindowOpened` |
//! | `window::close`      | `WindowClosed` |
//! | `window::focus`      | `WindowFocused` |
//! | `window::move`       | `WindowMoved` |
//! | `window::title`      | `WindowMetadataChanged` |

//! 维护 `Arc<RwLock<Vec<WindowInfo>>>`；GET_TREE 全量快照在 `window::new`
//! / `close` 时刷新（sway 事件载荷只含受影响容器，增量重建树成本高于
//! 直接拉取）。查询优先读缓存。

use std::sync::Arc;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::event::{DesktopEvent, EventSource, EventStream};
use agent_shell_core::types::{
    DesktopEnvironment, Rect, WindowId, WindowInfo, WindowState, WindowType, WorkspaceId,
};
use parking_lot::RwLock;

use crate::ipc::read_message;

/// 订阅的事件类型（SUBSCRIBE payload）。
pub const SUBSCRIBED_EVENTS: &str = r#"["workspace","window"]"#;

/// 共享窗口缓存（hyprland.md cache.rs 模式）。
pub type SharedWindowCache = Arc<RwLock<Vec<WindowInfo>>>;

/// Sway 事件订阅句柄。
///
/// [`SwayEventStream::spawn`] 建立长连接并返回读取端；缓存引用与流共享，
/// 查询方（compositor）读缓存即可获得事件驱动刷新的窗口列表。
pub struct SwayEventStream {
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<DesktopEvent>>,
    /// 事件任务持有的窗口缓存（与 compositor 共享）。
    cache: SharedWindowCache,
}

impl SwayEventStream {
    /// 建立 SUBSCRIBE 长连接，启动事件泵任务。
    ///
    /// 连接失败 → `BackendUnavailable`（sway 已退出 / socket 失效）；
    /// 泵任务随 socket 关闭自然退出，读取端随后返回 None。
    pub async fn spawn(ipc: &crate::ipc::SwayIpc) -> Result<(Self, SharedWindowCache)> {
        let mut stream = ipc.connect_stream().await?;
        crate::ipc::write_message(
            &mut stream,
            crate::ipc::IpcCommand::Subscribe,
            SUBSCRIBED_EVENTS,
        )
        .await?;
        // SUBSCRIBE 的回执是普通 REPLY 帧（success 标记），先消费掉再进入事件循环。
        let (_ty, body) = read_message(&mut stream).await?;
        let ok = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("success").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);
        if !ok {
            return Err(AgentShellError::BackendUnavailable(format!(
                "sway ipc subscribe rejected: {body}"
            )));
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cache: SharedWindowCache = Arc::new(RwLock::new(Vec::new()));
        // 泵任务独占长连接；每帧解析 → 映射 → 推送 + 刷新缓存。解析失败的
        // 帧记日志后丢弃——单帧损坏不应终止整条事件流。
        let pump_cache = Arc::clone(&cache);
        tokio::spawn(async move {
            loop {
                match read_message(&mut stream).await {
                    Ok((reply_type, body)) => {
                        match serde_json::from_str::<serde_json::Value>(&body) {
                            Ok(event) => pump_frame(reply_type, &event, &pump_cache, &tx),
                            Err(e) => tracing::warn!("sway event: bad JSON frame: {e}"),
                        }
                    }
                    Err(e) => {
                        tracing::info!("sway event stream closed: {e}");
                        break;
                    }
                }
            }
        });
        Ok((
            Self {
                rx: tokio::sync::Mutex::new(rx),
                cache: Arc::clone(&cache),
            },
            cache,
        ))
    }

    /// 窗口缓存只读视图（compositor 查询路径）。
    pub fn cache(&self) -> SharedWindowCache {
        Arc::clone(&self.cache)
    }
}

#[async_trait::async_trait]
impl EventStream for SwayEventStream {
    async fn next_event(&self) -> Option<DesktopEvent> {
        self.rx.lock().await.recv().await
    }
}
/// 处理一帧事件：按 i3 IPC 报文头 `reply_type` 分发（sway 事件 payload 无
/// `type` 字段，分发必须依据报文头而非 payload）。
///
/// - `EVENT_WORKSPACE` (0x80000000|0) → workspace::init/focus/empty
/// - `EVENT_WINDOW`   (0x80000000|3) → window::new/close/focus/move/title/...
fn pump_frame(
    reply_type: u32,
    event: &serde_json::Value,
    cache: &SharedWindowCache,
    tx: &tokio::sync::mpsc::UnboundedSender<DesktopEvent>,
) {
    use crate::ipc::{EVENT_MASK, EVENT_WINDOW, EVENT_WORKSPACE};
    if reply_type & EVENT_MASK == 0 {
        return; // 非事件帧（SUBSCRIBE 回执等）——不应进入泵循环，忽略。
    }
    let source = EventSource::Sway;
    let occurred_at = std::time::Instant::now();
    let change = event.get("change").and_then(serde_json::Value::as_str);
    match (reply_type, change) {
        (EVENT_WORKSPACE, Some("init" | "focus")) => {
            if let Some(info) = parse_workspace_info(event) {
                send(
                    tx,
                    DesktopEvent::WorkspaceChanged {
                        info,
                        source,
                        occurred_at,
                    },
                );
            }
        }
        (EVENT_WORKSPACE, Some("empty")) => {
            // 工作区被清空回收：以全量 GET_WORKSPACES 快照刷新列表语义。
            // 事件本身不含完整列表，此处只发占位信号（空列表），T3b
            // 归一化管线可按需重查。
            send(
                tx,
                DesktopEvent::WorkspaceListChanged {
                    workspaces: Vec::new(),
                    source,
                    occurred_at,
                },
            );
        }
        (EVENT_WINDOW, change) => {
            handle_window_event(change, event, cache, tx, source, occurred_at);
        }
        _ => {}
    }
}

/// window::* 事件分发。
fn handle_window_event(
    change: Option<&str>,
    event: &serde_json::Value,
    cache: &SharedWindowCache,
    tx: &tokio::sync::mpsc::UnboundedSender<DesktopEvent>,
    source: EventSource,
    occurred_at: std::time::Instant,
) {
    let Some(container) = event.get("container") else {
        return;
    };
    let native_id = container
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .map(|id| id.to_string());
    let id = native_id.map(|native_id| WindowId {
        native_id,
        de_type: DesktopEnvironment::Sway,
    });
    let Some(id) = id else { return };

    match change {
        Some("new") => {
            if let Some(info) = parse_window_info(container) {
                cache.write().push(info.clone());
                send(
                    tx,
                    DesktopEvent::WindowOpened {
                        info,
                        source,
                        occurred_at,
                    },
                );
            }
        }
        Some("close") => {
            cache.write().retain(|w| w.id != id);
            send(
                tx,
                DesktopEvent::WindowClosed {
                    id,
                    source,
                    occurred_at,
                },
            );
        }
        Some("focus") => {
            set_focused(cache, &id);
            if let Some(info) = current_window(cache, &id) {
                send(
                    tx,
                    DesktopEvent::WindowFocused {
                        info,
                        source,
                        occurred_at,
                    },
                );
            }
        }
        Some("move") => {
            refresh_from_container(cache, container);
            let geometry = container_geometry(container);
            send(
                tx,
                DesktopEvent::WindowMoved {
                    id,
                    geometry,
                    source,
                    occurred_at,
                },
            );
        }
        Some("title") => {
            refresh_from_container(cache, container);
            send(
                tx,
                DesktopEvent::WindowMetadataChanged {
                    id,
                    title: container
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    app_id: container
                        .get("app_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    source,
                    occurred_at,
                },
            );
        }
        Some("floating_node" | "fullscreen_mode" | "mark") => {
            // 几何/状态扰动：刷新缓存但不产生独立事件变体（状态变化由
            // T3b 归一化统一推导）。
            refresh_from_container(cache, container);
        }
        _ => {}
    }
}

/// 缓存内标记唯一焦点窗口。
fn set_focused(cache: &SharedWindowCache, focused: &WindowId) {
    let mut windows = cache.write();
    for w in windows.iter_mut() {
        w.states.retain(|s| *s != WindowState::Hidden);
        if w.id == *focused && !w.states.contains(&WindowState::Normal) {
            w.states.push(WindowState::Normal);
        }
    }
}

/// 从事件容器刷新缓存中的既有窗口（找不到则忽略——close 已先行清理）。
fn refresh_from_container(cache: &SharedWindowCache, container: &serde_json::Value) {
    let Some(native) = container.get("id").and_then(serde_json::Value::as_u64) else {
        return;
    };
    let mut windows = cache.write();
    for w in windows.iter_mut() {
        if w.id.native_id == native.to_string() {
            let geo = container_geometry(container);
            w.geometry = geo;
            w.frame_geometry = container.get("deco_rect").map(|_| geo).unwrap_or(geo);
            if let Some(title) = container.get("name").and_then(serde_json::Value::as_str) {
                w.title = title.to_string();
            }
        }
    }
}

fn current_window(cache: &SharedWindowCache, id: &WindowId) -> Option<WindowInfo> {
    cache.read().iter().find(|w| &w.id == id).cloned()
}

fn send(tx: &tokio::sync::mpsc::UnboundedSender<DesktopEvent>, event: DesktopEvent) {
    // 无订阅者时发送失败属正常（fan-out 未建立），静默丢弃。
    let _ = tx.send(event);
}

/// 容器 geometry 字段 → [`Rect`]。
fn container_geometry(container: &serde_json::Value) -> Rect {
    rect_of(container.get("geometry"))
}

/// workspace 事件顶层字段 → [`agent_shell_core::types::WorkspaceInfo`] 最小形。
fn parse_workspace_info(
    event: &serde_json::Value,
) -> Option<agent_shell_core::types::WorkspaceInfo> {
    let name = event
        .get("current")
        .and_then(|c| c.get("name"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| event.get("name").and_then(serde_json::Value::as_str))?
        .to_string();
    Some(agent_shell_core::types::WorkspaceInfo {
        id: WorkspaceId {
            native_id: name.clone(),
            de_type: DesktopEnvironment::Sway,
        },
        number: event
            .get("current")
            .and_then(|c| c.get("num"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        name,
        // 必设 focused=true，empty/move 等非激活语义事件下为 false 或缺省。
        is_active: event
            .get("current")
            .and_then(|c| c.get("focused"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        monitor_ids: Vec::new(),
        window_ids: Vec::new(),
    })
}

/// tiling/floating 容器 JSON → [`WindowInfo`]。
///
/// 仅处理真实应用窗口（`app_id` 或 `window` 属性存在）；纯容器节点
/// （split 构造、workspace 节点）跳过。
pub fn parse_window_info(container: &serde_json::Value) -> Option<WindowInfo> {
    let is_window = container.get("app_id").is_some() || container.get("window").is_some();
    if !is_window {
        return None;
    }
    let frame = rect_of(container.get("rect"));
    let geometry = container_geometry(container);
    let states = states_of(container);
    let id = container.get("id").and_then(serde_json::Value::as_u64)?;
    Some(WindowInfo {
        id: WindowId {
            native_id: id.to_string(),
            de_type: DesktopEnvironment::Sway,
        },
        title: container
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        app_id: container
            .get("app_id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                container
                    .get("window_properties")
                    .and_then(|p| p.get("class"))
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap_or_default()
            .to_string(),
        geometry,
        frame_geometry: frame,
        pid: container
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        states,
        workspace_id: container
            .get("workspace")
            .and_then(|w| {
                w.as_str()
                    .or_else(|| w.get("name").and_then(serde_json::Value::as_str))
            })
            .map(|n| WorkspaceId {
                native_id: n.to_string(),
                de_type: DesktopEnvironment::Sway,
            }),
        monitor_id: container
            .get("output")
            .and_then(serde_json::Value::as_str)
            .map(|n| agent_shell_core::types::MonitorId {
                native_id: n.to_string(),
                de_type: DesktopEnvironment::Sway,
            }),
        stacking_order: container
            .get("focus")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.len() as u32)
            .unwrap_or(0),
        desktop_file: None,
        window_type: WindowType::Normal,
        icon_geometry: None,
        keep_above: false,
    })
}

/// x/y/width/height 形状的对象 → [`Rect`]。
fn rect_of(v: Option<&serde_json::Value>) -> Rect {
    let Some(v) = v else {
        return Rect::default();
    };
    Rect {
        x: num_i32(v.get("x")),
        y: num_i32(v.get("y")),
        width: num_i32(v.get("width")),
        height: num_i32(v.get("height")),
    }
}

/// 容器几何优先级：rect（含装饰外框）为主，geometry 为内容区域。
/// 本层统一取 rect 作为 frame、geometry 作为内容；无 decoration 时两者一致。
fn states_of(container: &serde_json::Value) -> Vec<WindowState> {
    let mut states = Vec::new();
    if container
        .get("visible")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        states.push(WindowState::Hidden);
    }
    if container
        .get("focused")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        // sway `focused==true` 是焦点窗口的唯一可靠标记；映射为 Normal
        // 态供 get_active_window 在 GET_TREE 快照路径上筛选（事件路径下
        // set_focused 同样产 Normal，语义一致）。
        states.push(WindowState::Normal);
    }
    for (key, state) in [
        ("fullscreen_mode", WindowState::FullScreen),
        ("maximized", WindowState::Maximized),
    ] {
        let on = match container.get(key).and_then(serde_json::Value::as_i64) {
            Some(n) => n > 0,
            None => container
                .get(key)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        };
        if on {
            states.push(state);
        }
    }
    states
}

fn num_i32(v: Option<&serde_json::Value>) -> i32 {
    v.and_then(serde_json::Value::as_i64).unwrap_or(0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW_CONTAINER: &str = r#"{
        "id": 42,
        "name": "Editor — main.rs",
        "app_id": "org.gnome.TextEditor",
        "pid": 1234,
        "focused": false,
        "visible": true,
        "fullscreen_mode": 0,
        "layout": "con",
        "workspace": "2",
        "output": "HEADLESS-1",
        "rect": {"x": 10, "y": 20, "width": 800, "height": 600},
        "geometry": {"x": 12, "y": 22, "width": 796, "height": 596}
    }"#;

    #[test]
    fn parses_real_window_container() {
        let c: serde_json::Value = serde_json::from_str(WINDOW_CONTAINER).unwrap();
        let w = parse_window_info(&c).expect("window");
        assert_eq!(w.id.native_id, "42");
        assert_eq!(w.app_id, "org.gnome.TextEditor");
        assert_eq!(w.frame_geometry.width, 800); // rect = 含装饰外框
        assert_eq!(w.geometry.width, 796); // geometry = 内容区域
        assert_eq!(w.pid, 1234);
        assert_eq!(w.workspace_id.as_ref().unwrap().native_id, "2");
    }

    #[test]
    fn skips_pure_container_nodes() {
        let split: serde_json::Value =
            serde_json::from_str(r#"{"id": 1, "layout": "splith"}"#).unwrap();
        assert!(parse_window_info(&split).is_none());
    }

    #[test]
    fn fullscreen_state_parsed_from_numeric_field() {
        let c: serde_json::Value = serde_json::from_str(WINDOW_CONTAINER).unwrap();
        let mut c = c;
        c["fullscreen_mode"] = serde_json::json!(1);
        let w = parse_window_info(&c).unwrap();
        assert!(w.states.contains(&WindowState::FullScreen));
    }

    #[test]
    fn window_close_event_removes_from_cache() {
        // 真实 sway 帧：payload 无 `type` 字段，分发依据报文头 reply_type
        // （EVENT_WINDOW = 0x80000003）。
        let cache: SharedWindowCache = Arc::new(RwLock::new(vec![]));
        let c: serde_json::Value = serde_json::from_str(WINDOW_CONTAINER).unwrap();
        cache.write().push(parse_window_info(&c).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let frame: serde_json::Value =
            serde_json::from_str(r#"{"change": "close", "container": {"id": 42}}"#).unwrap();
        pump_frame(crate::ipc::EVENT_WINDOW, &frame, &cache, &tx);
        assert!(cache.read().is_empty());
    }

    #[test]
    fn window_new_event_appends_to_cache_and_emits() {
        let cache: SharedWindowCache = Arc::new(RwLock::new(vec![]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let frame: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"change": "new", "container": {WINDOW_CONTAINER}}}"#
        ))
        .unwrap();
        pump_frame(crate::ipc::EVENT_WINDOW, &frame, &cache, &tx);
        assert_eq!(cache.read().len(), 1);
        assert!(matches!(
            rx.try_recv().unwrap(),
            DesktopEvent::WindowOpened { .. }
        ));
    }

    #[test]
    fn non_event_frame_is_ignored() {
        // 报文头不带 EVENT_MASK（如 SUBSCRIBE 回执 reply_type=0）应被忽略。
        let cache: SharedWindowCache = Arc::new(RwLock::new(vec![]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let frame: serde_json::Value = serde_json::from_str(r#"{"success": true}"#).unwrap();
        pump_frame(0, &frame, &cache, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn workspace_init_event_emits_workspace_changed() {
        let cache: SharedWindowCache = Arc::new(RwLock::new(vec![]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let frame: serde_json::Value = serde_json::from_str(
            r#"{"change": "init", "current": {"name": "3", "num": 3, "focused": true}}"#,
        )
        .unwrap();
        pump_frame(crate::ipc::EVENT_WORKSPACE, &frame, &cache, &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            DesktopEvent::WorkspaceChanged { .. }
        ));
    }
}
