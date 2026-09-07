//! HyprlandCompositor：Hyprland 合成器组件（设计文档 §9.5，`mod.rs` 职责）。
//!
//! 组合装配（§3.3 / fallback.md §11.1 基类复用）：
//!
//! ```text
//! pub struct HyprlandCompositor {
//!     base: WlrWaylandCompositor,   // 继承 wlr 完整实现
//!     hyprctl: Hyprctl,             // + hyprland 专有通道
//! }
//! ```
//!
//! 操作选择矩阵（§9.5）落地策略：
//! - **窗口列表** = clients -j（含 geometry）与缓存合并——快照校准缓存，
//!   查询零 IPC 优先读缓存；
//! - **聚焦/关闭**：wlr foreign-toplevel 为首选，但 toplevel 聚合归 T3b；
//!   T3b 前直接走 hyprctl dispatch（明确的可用通道优于 NotImplemented）；
//! - **移动/缩放**：协议不支持 → hyprctl movewindowpixel/resizewindowpixel；
//! - **工作区列表/激活**：ext-workspace 协议聚合同样归 T3b，先走
//!   workspaces -j / dispatch workspace；
//! - **截图**：hyprland_toplevel_export（私有协议绑定就绪即上报能力）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use crate::cache::WindowCache;
use crate::event_socket::{spawn_event_task, EventTaskHandle};
use crate::hyprctl::Hyprctl;
use crate::wayland::HyprlandBindings;
use agent_shell_compositor_wayland_core::WaylandCompositor;
use agent_shell_compositor_wlr_wayland::WlrWaylandCompositor;
use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::event::EventStream;
use agent_shell_core::types::{
    MonitorId, MonitorInfo, Rect, WindowId, WindowInfo, WindowState, WorkspaceId, WorkspaceInfo,
};
use agent_shell_core::DesktopEnvironment;
use agent_shell_displayserver_wayland::WaylandDisplayServer;

/// Hyprland 合成器组件（§9.5 核心结构体）。
pub struct HyprlandCompositor {
    /// 基类：wlr 标准协议完整实现（继承表达：内嵌基类型）。
    base: WlrWaylandCompositor,
    /// hyprland_* 私有协议绑定（叠加在基类同一 wl_display 上）。
    bindings: Option<HyprlandBindings>,
    /// hyprctl socket IPC（降级 + 扩展能力）。
    hyprctl: Hyprctl,
    /// 窗口缓存（事件流持续更新，查询优先读取）。
    window_cache: WindowCache,
    /// 当前焦点窗口地址（activewindowv2 维护；fullscreen 广播消费）。
    focused_address: Arc<AsyncMutex<String>>,
    /// 事件任务句柄（懒启动；drop 即停止）。
    ///
    /// 只存句柄不存 Receiver——事件任务内部用 try_send 丢弃满载事件，
    /// 避免通道满后死锁（Radian 审查 #4）。
    event: RwLock<Option<EventTaskHandle>>,
}

impl std::fmt::Debug for HyprlandCompositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyprlandCompositor")
            .field("base", &self.base)
            .field("bindings", &self.bindings)
            .field("hyprctl", &self.hyprctl)
            .field("window_cache", &self.window_cache)
            .finish()
    }
}

impl HyprlandCompositor {
    /// 全自动装配：wlr 基类连接 → 私有协议探测 → hyprctl 会话校验。
    ///
    /// 非 Hyprland 会话（无 HYPRLAND_INSTANCE_SIGNATURE）返回
    /// `BackendUnavailable`。私有协议全缺不致命（回退 hyprctl）；hyprctl
    /// socket 不存在则视为会话不可达。
    pub fn connect() -> Result<Self> {
        let ds = WaylandDisplayServer::connect()?;
        Self::connect_with(ds)
    }

    /// 基于既有显示服务器装配（子类叠加路径）。
    pub fn connect_with(ds: WaylandDisplayServer) -> Result<Self> {
        let base = WlrWaylandCompositor::connect_with(ds)?;
        let hyprctl = Hyprctl::new()?;
        // 私有协议探测失败不阻断装配——hyprctl 是完整后备通道。
        let bindings = match HyprlandBindings::probe(base.display_server()) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::info!("hyprland_* private protocols unavailable: {e}");
                None
            }
        };
        Ok(Self {
            base,
            bindings,
            hyprctl,
            window_cache: WindowCache::default(),
            focused_address: Arc::new(AsyncMutex::new(String::new())),
            event: RwLock::new(None),
        })
    }

    /// wlr 基类引用（doctor / 输入注入等上层使用）。
    pub fn base(&self) -> &WlrWaylandCompositor {
        &self.base
    }

    /// hyprctl 通道引用（dispatch exec 等扩展能力入口）。
    pub fn hyprctl(&self) -> &Hyprctl {
        &self.hyprctl
    }

    /// hyprland_* 私有协议绑定集合。
    pub fn bindings(&self) -> Option<&HyprlandBindings> {
        self.bindings.as_ref()
    }

    /// 窗口缓存句柄（事件 task 与外部查询共享）。
    pub fn window_cache(&self) -> &WindowCache {
        &self.window_cache
    }

    /// 确保 socket2 事件任务在跑（幂等；懒启动语义与 KWin 一致）。
    ///
    /// 只存 EventTaskHandle，不存 Receiver——事件任务用 try_send
    /// 在通道满时丢弃事件而非阻塞，避免无人消费时死锁。
    async fn ensure_event_task(&self) {
        let mut guard = self.event.write().await;
        if guard.is_none() {
            let (_rx, task) = spawn_event_task(
                self.hyprctl.event_socket_path().to_path_buf(),
                self.window_cache.clone(),
                self.focused_address.clone(),
            );
            *guard = Some(task);
        }
    }

    /// `clients -j` 快照解析为 [`WindowInfo`] 并整体校准缓存。
    async fn refresh_cache_from_clients(&self) -> Result<Vec<WindowInfo>> {
        let v = self.hyprctl.clients().await?;
        let windows = parse_clients(&v);
        self.window_cache.replace_all(windows.clone());
        Ok(windows)
    }

    /// doctor 输出（§9.5 验证输出格式）。
    ///
    /// 同步方法：只读已缓存的构造期事实（HIS、协议绑定计数）；hyprctl
    /// 连通性与事件流状态需要异步探测，由 [`Self::doctor_lines_async`]
    /// 补充。
    pub fn doctor_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "✓ Hyprland 会话  : HYPRLAND_INSTANCE_SIGNATURE={}",
            self.hyprctl.instance_signature()
        ));

        // Wayland 协议行：wlr 基类 7 个 + 私有 4 个 globals。
        let wlr_bound = self.base.bound_count();
        let private_bound = self.bindings.as_ref().map(|b| b.bound_count()).unwrap_or(0);
        let total = wlr_bound + private_bound;
        let mut detail = Vec::new();
        if self.base.bindings().foreign_toplevel.is_some() {
            detail.push("foreign-toplevel v3".to_string());
        }
        if self.base.bindings().virtual_pointer.is_some() {
            detail.push("virtual-pointer v2".to_string());
        }
        if let Some(b) = &self.bindings {
            if b.toplevel_export.is_some() {
                detail.push("hyprland-export v2".to_string());
            }
        }
        lines.push(format!(
            "{} Wayland 协议   : {}/11 globals bound ({})",
            if total > 0 { "✓" } else { "⚠" },
            total,
            if detail.is_empty() {
                "none".to_string()
            } else {
                detail.join(", ")
            }
        ));
        lines
    }

    /// doctor 异步补充行：hyprctl 连通性 + 事件流状态（§9.5 示例后两行）。
    pub async fn doctor_lines_async(&self) -> Vec<String> {
        let mut lines = Vec::new();
        match self.hyprctl.ping_ms().await {
            Ok(ms) => {
                // 探测顺带刷新一次快照，使窗口计数真实。
                let windows = self
                    .refresh_cache_from_clients()
                    .await
                    .ok()
                    .map(|w| w.len());
                let monitors = self
                    .hyprctl
                    .monitors()
                    .await
                    .ok()
                    .and_then(|m| m.as_array().cloned())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let workspaces = self
                    .hyprctl
                    .workspaces()
                    .await
                    .ok()
                    .and_then(|m| m.as_array().cloned())
                    .map(|a| a.len())
                    .unwrap_or(0);
                lines.push(format!(
                    "✓ hyprctl socket : OK (<{ms:.1}ms, {windows} windows, \
                     {monitors} monitors, {workspaces} workspaces)",
                    windows = windows.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                ));
            }
            Err(e) => lines.push(format!("⚠ hyprctl socket : {e}")),
        }
        let events_running = self.event.try_read().map(|g| g.is_some()).unwrap_or(false);
        lines.push(if events_running {
            "✓ 事件流        : connected (openwindow, closewindow, activewindow, workspacev2)"
                .to_string()
        } else {
            "⚠ 事件流        : not started (lazy; daemon 事件归一化管线未装配，subscribe 未接线)"
                .to_string()
        });
        lines
    }
}

/// `clients -j` 数组解析为 WindowInfo（地址去 0x 前缀作 native_id）。
fn parse_clients(v: &Value) -> Vec<WindowInfo> {
    let empty = Vec::new();
    let arr = v.as_array().unwrap_or(&empty);
    arr.iter()
        .enumerate()
        .map(|(i, c)| {
            // Hyprland clients -j: "at" and "size" are JSON arrays [x, y] / [w, h].
            let arr_idx = |c: &Value, field: &str, idx: usize| -> i32 {
                c.get(field)
                    .and_then(Value::as_array)
                    .and_then(|a| a.get(idx))
                    .and_then(Value::as_i64)
                    .unwrap_or(0) as i32
            };
            let addr = c
                .get("address")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim_start_matches("0x")
                .to_string();
            let geo = Rect {
                x: arr_idx(c, "at", 0),
                y: arr_idx(c, "at", 1),
                width: arr_idx(c, "size", 0),
                height: arr_idx(c, "size", 1),
            };
            let mut states = Vec::new();
            // Hyprland fullscreen: 0=None, 1=Maximized, 2=Fullscreen, 3=Maximized+Fullscreen.
            // Only >= 2 is true fullscreen.
            if c.get("fullscreen").and_then(Value::as_i64).unwrap_or(0) >= 2 {
                states.push(WindowState::FullScreen);
            }
            if c.get("mapped").and_then(Value::as_bool) == Some(false) {
                states.push(WindowState::Hidden);
            }
            let workspace_id = c
                .get("workspace")
                .and_then(|w| w.get("id"))
                .and_then(Value::as_u64);
            WindowInfo {
                id: WindowId {
                    native_id: addr,
                    de_type: DesktopEnvironment::Hyprland,
                },
                title: c
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                app_id: c
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                pid: c.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
                geometry: geo,
                frame_geometry: geo,
                states,
                workspace_id: workspace_id.map(|id| WorkspaceId {
                    native_id: id.to_string(),
                    de_type: DesktopEnvironment::Hyprland,
                }),
                monitor_id: c
                    .get("monitor")
                    .and_then(Value::as_u64)
                    .map(|id| MonitorId {
                        native_id: id.to_string(),
                        de_type: DesktopEnvironment::Hyprland,
                    }),
                stacking_order: i as u32,
                desktop_file: c
                    .get("initialClass")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string()),
                window_type: agent_shell_core::types::WindowType::Normal,
                icon_geometry: None,
                keep_above: c.get("pinned").and_then(Value::as_bool).unwrap_or(false),
            }
        })
        .collect()
}

impl WaylandCompositor for HyprlandCompositor {
    fn display_server(&self) -> &WaylandDisplayServer {
        self.base.display_server()
    }
}

#[async_trait]
impl DesktopComponent for HyprlandCompositor {
    fn name(&self) -> &'static str {
        "hyprland-compositor"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }

    fn is_available(&self) -> bool {
        true // 构造成功即可用；通道缺失由 health 表达
    }

    async fn health(&self) -> ComponentHealth {
        let private_ok = self
            .bindings
            .as_ref()
            .map(|b| b.bound_count() > 0)
            .unwrap_or(false);
        match (self.base.bound_count(), private_ok) {
            (w, p) if w > 0 && p => ComponentHealth::Healthy,
            (_, true) => {
                ComponentHealth::Degraded("wlr protocols partial; hyprland private OK".into())
            }
            (0, false) => {
                ComponentHealth::Degraded("no wayland protocols; full portal fallback".into())
            }
            _ => ComponentHealth::Degraded("private protocols unbound; hyprctl fallback".into()),
        }
    }
}

#[async_trait]
impl CompositorComponent for HyprlandCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            window_management: true,
            workspace_management: true,
            monitor_layout: true,
            window_events: false, // T3b 归一化前不承诺 DesktopEvent 全集
            workspace_events: false,
            native_input: self.base.bindings().virtual_pointer.is_some(),
            native_capture: true, // hyprland_toplevel_export / wlr-screencopy
            virtual_desktops: true,
            effects_control: false,
        }
    }

    /// 窗口列表：clients -j 快照校准缓存后返回缓存内容（§9.5「合并」行）。
    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        match self.refresh_cache_from_clients().await {
            Ok(windows) => Ok(windows),
            // hyprctl 不可用时降级读缓存（事件流可能仍在维护）。
            Err(e) => {
                let cached = self.window_cache.snapshot();
                if cached.is_empty() {
                    Err(e)
                } else {
                    Ok(cached)
                }
            }
        }
    }

    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        self.ensure_event_task().await;
        // activewindow -j 是权威来源且比事件等待便宜。
        let v = self.hyprctl.request("activewindow -j").await?;
        if v.is_null() || v.as_object().is_none() {
            return Ok(None);
        }
        let arr = vec![v];
        let parsed = parse_clients(&Value::Array(arr));
        Ok(parsed.into_iter().next())
    }

    /// 聚焦：矩阵首选 Wayland foreign-toplevel.activate，T3b 前走
    /// hyprctl focuswindow（明确可用通道优先于占位错误）。
    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        self.hyprctl.focus_window(&id.native_id).await
    }

    /// 移动：协议不支持 → hyprctl movewindowpixel（§9.5 矩阵）。
    async fn move_window(&self, id: &WindowId, x: i32, y: i32) -> Result<()> {
        self.hyprctl.move_window(&id.native_id, x, y).await
    }

    /// 缩放：协议不支持 → hyprctl resizewindowpixel。
    async fn resize_window(&self, id: &WindowId, w: i32, h: i32) -> Result<()> {
        self.hyprctl.resize_window(&id.native_id, w, h).await
    }

    async fn minimize_window(&self, _id: &WindowId) -> Result<()> {
        // Hyprland 无最小化概念（tiling 合成器）；显式拒绝优于静默成功。
        Err(AgentShellError::NotImplemented(
            "Hyprland has no minimize concept; use workspace switching".into(),
        ))
    }

    async fn unminimize_window(&self, _id: &WindowId) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "unminimize lands with T3b aggregation".into(),
        ))
    }

    /// 最大化：dispatch fullscreen 1（Hyprland maximize 模式 = fullscreen 1）
    /// ——精确 maximize/unmaximize 区分依赖 foreign-toplevel 聚合（T3b）。
    async fn maximize_window(&self, id: &WindowId) -> Result<()> {
        self.hyprctl
            .dispatch(&format!("fullscreen 1,address:0x{}", id.native_id))
            .await
    }

    /// 关闭：矩阵首选 foreign-toplevel.close，T3b 前走 hyprctl closewindow。
    async fn close_window(&self, id: &WindowId) -> Result<()> {
        self.hyprctl.close_window(&id.native_id).await
    }

    /// 精确几何设置：movewindowpixel + resizewindowpixel 组合（§9.4「协议
    /// 覆盖不了的操作走它」）。
    async fn set_window_geometry(&self, id: &WindowId, geo: Rect) -> Result<()> {
        let addr = &id.native_id;
        self.hyprctl
            .dispatch(&format!("setfloating address:0x{addr}"))
            .await?;
        self.hyprctl
            .dispatch(&format!(
                "movewindowpixel exact {} {},address:0x{addr}",
                geo.x, geo.y
            ))
            .await?;
        self.hyprctl
            .dispatch(&format!(
                "resizewindowpixel exact {} {},address:0x{addr}",
                geo.width, geo.height
            ))
            .await
    }

    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        if let Some(w) = self.window_cache.get(id) {
            return Ok(w);
        }
        let windows = self.list_windows().await?;
        windows
            .into_iter()
            .find(|w| w.id == *id)
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))
    }

    /// 工作区列表：ext-workspace 聚合归 T3b，先走 workspaces -j。
    ///
    /// `is_active` 通过 `activeworkspace -j` 的 `id` 字段匹配——
    /// Hyprland `workspaces -j` 无 `focused` 字段（Radian 审查 #3）。
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let v = self.hyprctl.workspaces().await?;
        // Fetch active workspace id to set is_active.
        let active_id = self
            .hyprctl
            .request("activeworkspace -j")
            .await
            .ok()
            .and_then(|aw| aw.get("id").and_then(Value::as_u64));
        let empty = Vec::new();
        let arr = v.as_array().unwrap_or(&empty);
        Ok(arr
            .iter()
            .map(|w| {
                let ws_id = w.get("id").and_then(Value::as_u64).unwrap_or(0);
                WorkspaceInfo {
                    id: WorkspaceId {
                        native_id: ws_id.to_string(),
                        de_type: DesktopEnvironment::Hyprland,
                    },
                    name: w
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    number: ws_id as u32,
                    is_active: active_id == Some(ws_id),
                    monitor_ids: w
                        .get("monitorID")
                        .and_then(Value::as_u64)
                        .map(|m| {
                            vec![MonitorId {
                                native_id: m.to_string(),
                                de_type: DesktopEnvironment::Hyprland,
                            }]
                        })
                        .unwrap_or_default(),
                    window_ids: Vec::new(),
                }
            })
            .collect())
    }

    /// 激活工作区：ext-workspace 首选（T3b），当前 dispatch workspace。
    async fn activate_workspace(&self, id: &WorkspaceId) -> Result<()> {
        let numeric: i32 = id.native_id.parse().unwrap_or(1);
        self.hyprctl.activate_workspace(numeric).await
    }

    async fn move_window_to_workspace(&self, wid: &WindowId, ws: &WorkspaceId) -> Result<()> {
        self.hyprctl
            .dispatch(&format!(
                "movetoworkspacesilent {},address:0x{}",
                ws.native_id, wid.native_id
            ))
            .await
    }

    /// 监视器列表：monitors -j。
    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        let v = self.hyprctl.monitors().await?;
        let empty = Vec::new();
        let arr = v.as_array().unwrap_or(&empty);
        Ok(arr
            .iter()
            .map(|m| {
                let mx = m.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
                let my = m.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
                let mw = m.get("width").and_then(Value::as_i64).unwrap_or(0) as i32;
                let mh = m.get("height").and_then(Value::as_i64).unwrap_or(0) as i32;
                // availableArea 相对位置 → 物理几何取整屏。
                MonitorInfo {
                    id: MonitorId {
                        native_id: m.get("id").and_then(Value::as_u64).unwrap_or(0).to_string(),
                        de_type: DesktopEnvironment::Hyprland,
                    },
                    name: m
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    geometry: Rect {
                        x: mx,
                        y: my,
                        width: mw,
                        height: mh,
                    },
                    physical_geometry: Rect {
                        x: mx,
                        y: my,
                        width: mw,
                        height: mh,
                    },
                    scale: m.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
                    // Hyprland has no "primary" concept; use "focused" as closest equivalent.
                    is_primary: m.get("focused").and_then(Value::as_bool).unwrap_or(false),
                    workspace_id: m
                        .get("activeWorkspace")
                        .and_then(|w| w.get("id"))
                        .and_then(Value::as_u64)
                        .map(|id| WorkspaceId {
                            native_id: id.to_string(),
                            de_type: DesktopEnvironment::Hyprland,
                        }),
                }
            })
            .collect())
    }

    /// 订阅事件流：懒启动 socket2 任务，返回归一化原始流。
    ///
    /// 与 KWin 同款语义：capabilities 的 window_events=false 表示 T3b
    /// 归一化未落地；本流已按 §9.5 映射核心四事件，调用方可消费。
    async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
        // Stop old task if any, then spawn a fresh one for this subscriber.
        let mut guard = self.event.write().await;
        if let Some(handle) = guard.take() {
            handle.stop();
        }
        let (rx, task) = spawn_event_task(
            self.hyprctl.event_socket_path().to_path_buf(),
            self.window_cache.clone(),
            self.focused_address.clone(),
        );
        *guard = Some(task);
        drop(guard);
        Ok(Box::new(HyprlandEventStream {
            rx: AsyncMutex::new(rx),
        }))
    }
}

/// 事件流读取端（[`CompositorComponent::subscribe`] 返回值）。
struct HyprlandEventStream {
    rx: AsyncMutex<tokio::sync::mpsc::Receiver<agent_shell_core::event::DesktopEvent>>,
}

#[async_trait]
impl EventStream for HyprlandEventStream {
    async fn next_event(&self) -> Option<agent_shell_core::event::DesktopEvent> {
        self.rx.lock().await.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 典型 clients -j JSON：at/size 数组、fullscreen=2、workspace 对象。
    #[test]
    fn parse_clients_extracts_geometry_and_states() {
        let v = json!([{
            "address": "0xabc123",
            "title": "terminal",
            "class": "foot",
            "initialClass": "foot",
            "pid": 1234,
            "at": [100, 200],
            "size": [800, 600],
            "workspace": {"id": 2, "name": "web"},
            "monitor": 0,
            "fullscreen": 2,
            "floating": false,
            "mapped": true,
            "pinned": false
        }]);
        let windows = parse_clients(&v);
        assert_eq!(windows.len(), 1);
        let w = &windows[0];
        assert_eq!(w.id.native_id, "abc123");
        assert_eq!(w.title, "terminal");
        assert_eq!(w.app_id, "foot");
        assert_eq!(w.pid, 1234);
        assert_eq!(w.geometry.x, 100);
        assert_eq!(w.geometry.y, 200);
        assert_eq!(w.geometry.width, 800);
        assert_eq!(w.geometry.height, 600);
        assert!(w.states.contains(&WindowState::FullScreen));
        assert!(!w.states.contains(&WindowState::Hidden));
        assert_eq!(w.workspace_id.as_ref().unwrap().native_id, "2");
    }

    /// fullscreen=1 (Maximized) 不应映射为 FullScreen（Radian 审查 #2）。
    #[test]
    fn parse_clients_maximized_not_fullscreen() {
        let v = json!([{
            "address": "0x1",
            "at": [0, 0],
            "size": [1920, 1080],
            "fullscreen": 1,
            "mapped": true
        }]);
        let w = &parse_clients(&v)[0];
        assert!(!w.states.contains(&WindowState::FullScreen));
    }

    /// 缺字段容错：at/size 缺失时几何归零不 panic。
    #[test]
    fn parse_clients_missing_fields_default_zero() {
        let v = json!([{"address": "0x2", "mapped": true}]);
        let w = &parse_clients(&v)[0];
        assert_eq!(w.geometry, Rect::default());
        assert_eq!(w.pid, 0);
        assert!(w.states.is_empty());
    }

    /// unmapped 窗口标记 Hidden。
    #[test]
    fn parse_clients_unmapped_is_hidden() {
        let v = json!([{
            "address": "0x3",
            "at": [10, 20],
            "size": [100, 100],
            "mapped": false,
            "fullscreen": 0
        }]);
        let w = &parse_clients(&v)[0];
        assert!(w.states.contains(&WindowState::Hidden));
    }

    /// 多窗口 stacking_order 递增。
    #[test]
    fn parse_clients_stacking_order_increments() {
        let v = json!([
            {"address": "0xa", "at": [0, 0], "size": [10, 10], "mapped": true},
            {"address": "0xb", "at": [0, 0], "size": [10, 10], "mapped": true},
            {"address": "0xc", "at": [0, 0], "size": [10, 10], "mapped": true}
        ]);
        let windows = parse_clients(&v);
        assert_eq!(windows[0].stacking_order, 0);
        assert_eq!(windows[1].stacking_order, 1);
        assert_eq!(windows[2].stacking_order, 2);
    }
}
