//! SwayCompositor（`mod.rs`，设计 02-architecture §3.2–§4.3 / fallback.md §11.1）。
//!
//! 继承层次：`CompositorComponent → WaylandCompositor → WlrWaylandCompositor
//! → SwayCompositor`——wlr 标准协议的绑定与操作全部复用基类
//! [`WlrWaylandCompositor`]（本 crate 不重复绑定任何 wlr global），
//! 叠加 Sway IPC 补充通道：
//!
//! - 窗口列表 / 几何查询：GET_TREE（hyprctl clients -j 的对位物）
//! - dispatch 操作：聚焦/移动/缩放/关闭/最大化（RUN_COMMAND）
//! - 工作区：GET_WORKSPACES 列举 + RUN_COMMAND 切换
//! - 监视器：GET_OUTPUTS
//! - 事件流：SUBSCRIBE 长连接（[`crate::event`]，事件驱动窗口缓存）
//!
//! 非 Sway 会话下 IPC 连接失败返回 [`AgentShellError::BackendUnavailable`]。

mod event;
mod ipc;

pub use event::{parse_window_info, SharedWindowCache, SwayEventStream};
pub use ipc::{IpcCommand, SwayIpc};

use std::sync::Arc;

use agent_shell_compositor_wayland_core::WaylandCompositor;
use agent_shell_compositor_wlr_wayland::WlrWaylandCompositor;
use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::event::EventStream;
use agent_shell_core::types::{
    DesktopEnvironment, MonitorId, MonitorInfo, Rect, WindowId, WindowInfo, WorkspaceId,
    WorkspaceInfo,
};
use serde_json::Value;

/// Sway 合成器组件（§3.2 内部协议通道表：基础 wlr 标准协议 + 补充 Sway IPC）。
///
/// 「继承」表达同 fallback.md §11.1 子类模式：内嵌基类字段持有 wlr 完整实现，
/// IPC 仅作补充通道。构造要求 Wayland 与 IPC 双通道均可达——任一失败即
/// `BackendUnavailable`（sway 会话两条通道必然共存；半可用状态属于降级
/// 运行期故障，由 health 表达而非构造容忍）。
pub struct SwayCompositor {
    /// 基类：wlr 标准协议完整实现（foreign-toplevel / screencopy /
    /// virtual-pointer / ext-workspace 等）。
    base: WlrWaylandCompositor,
    /// 补充通道：i3 兼容 IPC（求值型命令即开即关）。
    ipc: SwayIpc,
    /// IPC 版本探测缓存（doctor 报告）。
    version: String,
    /// 构造时 IPC roundtrip 时延（doctor 报告）。
    ipc_rtt_ms: f64,
    /// 构造时窗口数快照（doctor 报告）。
    window_count: usize,
    /// 构造时工作区数快照（doctor 报告）。
    workspace_count: usize,
    /// 事件订阅句柄（懒启动；持有即维持长连接与缓存刷新任务存活）。
    event_stream: tokio::sync::Mutex<Option<Arc<SwayEventStream>>>,
}

impl std::fmt::Debug for SwayCompositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwayCompositor")
            .field("base", &self.base)
            .field("ipc", &self.ipc)
            .field("version", &self.version)
            .finish()
    }
}

impl SwayCompositor {
    /// 装配入口（§4.3 `SwayBackend::assemble` 对位签名）。
    ///
    /// 连接 `$WAYLAND_DISPLAY`（经基类绑定全部 wlr globals）+ `$SWAYSOCK`
    /// （GET_VERSION 探测）。任一通道不可用 → `BackendUnavailable`。
    pub async fn connect() -> Result<Self> {
        let base = WlrWaylandCompositor::connect()?;
        Self::with_base(base).await
    }
    /// 基于既有基类实例装配（测试注入 / 多会话场景）。
    pub async fn with_base(base: WlrWaylandCompositor) -> Result<Self> {
        let ipc = SwayIpc::from_env()?;
        // 版本探测与 roundtrip 时延一次往返取得（同一条 GET_VERSION）。
        let start = std::time::Instant::now();
        let version_reply = ipc.roundtrip(IpcCommand::GetVersion, "").await?;
        let ipc_rtt_ms = start.elapsed().as_secs_f64() * 1000.0;
        let version = version_reply
            .get("full_version")
            .or_else(|| version_reply.get("major"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        // 窗口/工作区计数快照（doctor 报告；失败时记 0，不阻断构造）。
        let window_count = match ipc.roundtrip(IpcCommand::GetTree, "").await {
            Ok(tree) => {
                let mut wins = Vec::new();
                collect_windows(&tree, 0, &mut wins);
                wins.len()
            }
            Err(e) => {
                tracing::warn!("sway doctor: GET_TREE snapshot failed: {e}");
                0
            }
        };
        let workspace_count = match ipc.roundtrip(IpcCommand::GetWorkspaces, "").await {
            Ok(arr) => arr.as_array().map(|a| a.len()).unwrap_or(0),
            Err(e) => {
                tracing::warn!("sway doctor: GET_WORKSPACES snapshot failed: {e}");
                0
            }
        };
        Ok(Self {
            base,
            ipc,
            version,
            ipc_rtt_ms,
            window_count,
            workspace_count,
            event_stream: tokio::sync::Mutex::new(None),
        })
    }

    /// 基类引用（wlr 协议操作通道）。
    pub fn base(&self) -> &WlrWaylandCompositor {
        &self.base
    }

    /// IPC 客户端引用。
    pub fn ipc(&self) -> &SwayIpc {
        &self.ipc
    }

    /// 探测到的 sway 版本（doctor 报告）。
    pub fn version(&self) -> &str {
        &self.version
    }

    // ───────────────────── IPC 查询 ─────────────────────

    /// 工作区列表（GET_WORKSPACES → WorkspaceInfo）。
    pub async fn query_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let arr = self
            .ipc
            .roundtrip(IpcCommand::GetWorkspaces, "")
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(arr
            .iter()
            .filter_map(|ws| {
                Some(WorkspaceInfo {
                    id: WorkspaceId {
                        native_id: ws.get("name")?.as_str()?.to_string(),
                        de_type: DesktopEnvironment::Sway,
                    },
                    name: ws
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    number: ws.get("num").and_then(Value::as_u64).unwrap_or(0) as u32,
                    is_active: ws.get("focused").and_then(Value::as_bool).unwrap_or(false),
                    monitor_ids: ws
                        .get("output")
                        .and_then(Value::as_str)
                        .map(|o| {
                            vec![MonitorId {
                                native_id: o.to_string(),
                                de_type: DesktopEnvironment::Sway,
                            }]
                        })
                        .unwrap_or_default(),
                    window_ids: Vec::new(),
                })
            })
            .collect())
    }

    /// 监视器列表（GET_OUTPUTS → MonitorInfo）。
    pub async fn query_monitors(&self) -> Result<Vec<MonitorInfo>> {
        let arr = self
            .ipc
            .roundtrip(IpcCommand::GetOutputs, "")
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(arr.iter().filter_map(parse_monitor).collect())
    }

    /// 窗口树全量拉取并展平为窗口列表（GET_TREE）。
    ///
    /// 树遍历只收集真实应用容器（`app_id` / X11 `window` 属性存在者）；
    /// stacking order 以遍历序近似（sway 树本身即焦点优先序）。
    pub async fn query_windows(&self) -> Result<Vec<WindowInfo>> {
        let tree = self.ipc.roundtrip(IpcCommand::GetTree, "").await?;
        let mut windows = Vec::new();
        collect_windows(&tree, 0, &mut windows);
        Ok(windows)
    }

    // ───────────────────── IPC dispatch ─────────────────────

    /// 按 `[con_id=N] <cmd>` 形态 dispatch（聚焦/移动等统一入口）。
    async fn dispatch(&self, selector: &str, cmd: &str) -> Result<()> {
        self.ipc
            .run_command(&format!("[con_id=\"{selector}\"] {cmd}"))
            .await
    }

    /// 确保事件泵在跑（subscribe 懒启动语义），返回缓存快照读取端。
    async fn ensure_event_stream(&self) -> Result<SharedWindowCache> {
        let mut guard = self.event_stream.lock().await;
        if guard.is_none() {
            let (stream, cache) = SwayEventStream::spawn(&self.ipc).await?;
            *guard = Some(Arc::new(stream));
            return Ok(cache);
        }
        Ok(guard.as_ref().expect("checked above").cache())
    }

    /// doctor 输出行（格式对照 Hyprland/WlrWayland 验证输出风格）。
    pub fn doctor_lines(&self) -> Vec<String> {
        let bound = self.base.bound_count();
        let detail = self
            .base
            .bind_failures()
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let failures = if detail.is_empty() {
            "all ok".to_string()
        } else {
            format!("failed: {detail}")
        };
        vec![
            format!(
                "{} Wayland 协议 : {}/7 globals bound ({failures})",
                if bound > 0 { "✓" } else { "⚠" },
                bound
            ),
            format!(
                "{} Sway IPC     : {} (SWAYSOCK, roundtrip {:.2}ms)",
                if self.version != "unknown" {
                    "✓"
                } else {
                    "⚠"
                },
                self.version,
                self.ipc_rtt_ms
            ),
            format!(
                "✓ 桌面状态     : {} 窗口 / {} 工作区",
                self.window_count, self.workspace_count
            ),
        ]
    }
}

/// GET_TREE 节点递归展平（深度上界防御异常环状树——sway 正常树深有限）。
fn collect_windows(node: &Value, depth: usize, out: &mut Vec<WindowInfo>) {
    const MAX_DEPTH: usize = 64;
    if depth > MAX_DEPTH {
        return;
    }
    if let Some(w) = parse_window_info(node) {
        out.push(w);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(Value::as_array) {
            for child in children {
                collect_windows(child, depth + 1, out);
            }
        }
    }
}

/// output JSON → MonitorInfo（`current_workspace` 关联当前工作区）。
fn parse_monitor(o: &Value) -> Option<MonitorInfo> {
    let name = o.get("name")?.as_str()?.to_string();
    Some(MonitorInfo {
        id: MonitorId {
            native_id: name.clone(),
            de_type: DesktopEnvironment::Sway,
        },
        name,
        geometry: monitor_rect(o.get("rect")),
        physical_geometry: Rect {
            x: 0,
            y: 0,
            width: o
                .pointer("/current_mode/width")
                .and_then(Value::as_i64)
                .unwrap_or(0) as i32,
            height: o
                .pointer("/current_mode/height")
                .and_then(Value::as_i64)
                .unwrap_or(0) as i32,
        },
        scale: o.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
        is_primary: o.get("primary").and_then(Value::as_bool).unwrap_or(false),
        workspace_id: o
            .get("current_workspace")
            .and_then(Value::as_str)
            .map(|n| WorkspaceId {
                native_id: n.to_string(),
                de_type: DesktopEnvironment::Sway,
            }),
    })
}

fn monitor_rect(v: Option<&Value>) -> Rect {
    let Some(v) = v else {
        return Rect::default();
    };
    Rect {
        x: v.get("x").and_then(Value::as_i64).unwrap_or(0) as i32,
        y: v.get("y").and_then(Value::as_i64).unwrap_or(0) as i32,
        width: v.get("width").and_then(Value::as_i64).unwrap_or(0) as i32,
        height: v.get("height").and_then(Value::as_i64).unwrap_or(0) as i32,
    }
}

impl WaylandCompositor for SwayCompositor {
    fn display_server(&self) -> &agent_shell_displayserver_wayland::WaylandDisplayServer {
        self.base.display_server()
    }
}

#[async_trait::async_trait]
impl DesktopComponent for SwayCompositor {
    fn name(&self) -> &'static str {
        "sway-compositor"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }

    fn is_available(&self) -> bool {
        true // 构造成功即可用；部分协议缺失由 health 表达
    }

    async fn health(&self) -> ComponentHealth {
        // IPC 可达性是 sway 特有通道的真实健康信号；wlr 绑定缺失走降级链
        // （portal / AT-SPI / Sway IPC 兜底，§5.4）。
        let failures = self.base.bind_failures();
        if self.base.bound_count() == 0 {
            ComponentHealth::Degraded("no wlr protocols; Sway IPC + portal/AT-SPI fallback".into())
        } else if failures.is_empty() {
            ComponentHealth::Healthy
        } else {
            ComponentHealth::Degraded("wlr protocols partial; IPC fallback".into())
        }
    }
}

/// 通道选择（§3.2 调用优先级）：窗口管理主路径复用基类 wlr 实现；
/// Sway IPC 承担精确几何、工作区切换与树查询。17 方法中基类尚未落地的
/// 部分（T3b foreign-toplevel 聚合前）由 IPC 全量兜底——这正是补充通道
/// 存在的意义，不返回 NotImplemented。
#[async_trait::async_trait]
impl CompositorComponent for SwayCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            window_management: true,
            workspace_management: true,
            monitor_layout: true,
            window_events: true,
            workspace_events: true,
            native_input: self.base.bindings().virtual_pointer.is_some(),
            native_capture: self.base.bindings().screencopy.is_some(),
            virtual_desktops: true,
            effects_control: false,
        }
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        // 缓存命中优先（事件驱动）；空缓存（未订阅）时直接 GET_TREE。
        if let Some(stream) = self
            .event_stream
            .try_lock()
            .ok()
            .as_ref()
            .and_then(|g| g.as_ref())
        {
            let cached = stream.cache().read().clone();
            if !cached.is_empty() {
                return Ok(cached);
            }
        }
        self.query_windows().await
    }

    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        Ok(self.query_windows().await?.into_iter().find(|w| {
            w.states
                .contains(&agent_shell_core::types::WindowState::Normal)
        }))
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        self.dispatch(&id.native_id, "focus").await
    }

    async fn move_window(&self, id: &WindowId, x: i32, y: i32) -> Result<()> {
        // floating 容器才接受绝对坐标；tiling 容器由 sway 自行布局拒绝
        // move position——调用方以 set_window_geometry（含 floating enable）
        // 表达精确几何意图。
        self.dispatch(&id.native_id, &format!("move position {x} {y}"))
            .await
    }

    async fn resize_window(&self, id: &WindowId, w: i32, h: i32) -> Result<()> {
        self.dispatch(&id.native_id, &format!("resize set {w} {h}"))
            .await
    }

    async fn minimize_window(&self, id: &WindowId) -> Result<()> {
        self.dispatch(&id.native_id, "move to scratchpad").await
    }

    async fn unminimize_window(&self, id: &WindowId) -> Result<()> {
        self.dispatch(&id.native_id, "scratchpad show").await
    }

    async fn maximize_window(&self, id: &WindowId) -> Result<()> {
        self.dispatch(&id.native_id, "fullscreen enable").await
    }

    async fn close_window(&self, id: &WindowId) -> Result<()> {
        self.dispatch(&id.native_id, "kill").await
    }

    async fn set_window_geometry(&self, id: &WindowId, geo: Rect) -> Result<()> {
        // 精确几何是 IPC 通道的存在理由（wlr foreign-toplevel 无 set_geometry）：
        // floating enable 后一次设定位置与尺寸。
        self.ipc
            .run_command(&format!(
                "[con_id=\"{}\"] floating enable, resize set {} {}, move position {} {}",
                id.native_id, geo.width, geo.height, geo.x, geo.y
            ))
            .await
    }

    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        self.query_windows()
            .await?
            .into_iter()
            .find(|w| w.id == *id)
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        self.query_workspaces().await
    }

    async fn activate_workspace(&self, id: &WorkspaceId) -> Result<()> {
        // 工作区名可能含特殊字符，按 i3 字符串字面量转义引号。
        let name = id.native_id.replace('\\', "\\\\").replace('"', "\\\"");
        self.ipc.run_command(&format!("workspace \"{name}\"")).await
    }

    async fn move_window_to_workspace(&self, wid: &WindowId, ws: &WorkspaceId) -> Result<()> {
        let name = ws.native_id.replace('\\', "\\\\").replace('"', "\\\"");
        self.ipc
            .run_command(&format!(
                "[con_id=\"{}\"] move container to workspace \"{}\"",
                wid.native_id, name
            ))
            .await
    }

    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        self.query_monitors().await
    }

    async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
        let cache = self.ensure_event_stream().await?;
        // 订阅建立后立即做一次全量快照填充缓存（事件只推增量）。
        match self.query_windows().await {
            Ok(windows) => {
                *cache.write() = windows;
            }
            Err(e) => tracing::warn!("sway subscribe: initial snapshot failed: {e}"),
        }
        let stream = self
            .event_stream
            .lock()
            .await
            .as_ref()
            .map(|s| Arc::clone(s) as Arc<dyn EventStream>)
            .ok_or_else(|| AgentShellError::BackendUnavailable("sway event stream gone".into()))?;
        // Box<dyn EventStream> 需要所有权；SwayEventStream 本体在 Arc 后面，
        // 直接把共享实现包一层返回（Arc 内类型已实现 EventStream）。
        Ok(Box::new(ArcEventStream(stream)))
    }
}

/// `Arc<dyn EventStream>` → `Box<dyn EventStream>` 适配（共享底层连接）。
struct ArcEventStream(Arc<dyn EventStream>);

#[async_trait::async_trait]
impl EventStream for ArcEventStream {
    async fn next_event(&self) -> Option<agent_shell_core::event::DesktopEvent> {
        self.0.next_event().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = r#"{
        "id": 1, "layout": "splith", "type": "root", "name": "root",
        "nodes": [{
            "id": 2, "layout": "output", "type": "output", "name": "HEADLESS-1",
            "nodes": [{
                "id": 3, "layout": "splith", "type": "con",
                "nodes": [
                    {"id": 10, "app_id": "foot", "pid": 100, "name": "term",
                     "rect": {"x": 0, "y": 0, "width": 960, "height": 1080},
                     "geometry": {"x": 2, "y": 2, "width": 956, "height": 1076},
                     "workspace": "1"},
                    {"id": 11, "app_id": null, "window": 4194305, "pid": 200,
                     "name": "xeyes",
                     "window_properties": {"class": "XEye"},
                     "rect": {"x": 960, "y": 0, "width": 960, "height": 1080},
                     "geometry": {"x": 960, "y": 0, "width": 960, "height": 1080}}
                ],
                "floating_nodes": [
                    {"id": 12, "app_id": "calc", "pid": 300, "name": "calc",
                     "fullscreen_mode": 0,
                     "rect": {"x": 100, "y": 100, "width": 400, "height": 300},
                     "geometry": {"x": 100, "y": 100, "width": 400, "height": 300}}
                ]
            }]
        }],
        "floating_nodes": []
    }"#;

    #[test]
    fn tree_flattens_real_windows_only() {
        let tree: Value = serde_json::from_str(TREE).unwrap();
        let mut out = Vec::new();
        collect_windows(&tree, 0, &mut out);
        assert_eq!(out.len(), 3);
        let ids: Vec<&str> = out.iter().map(|w| w.id.native_id.as_str()).collect();
        assert_eq!(ids, ["10", "11", "12"]);
        // tiling + X11 (window_properties.class 回退) + floating 各一。
        assert_eq!(out[0].app_id, "foot");
        assert_eq!(out[1].app_id, "XEye");
        assert_eq!(out[2].app_id, "calc");
    }

    #[test]
    fn monitor_parse_maps_mode_and_workspace() {
        let m: Value = serde_json::from_str(
            r#"{"name": "HEADLESS-1", "primary": true, "scale": 1.5,
                "active": true,
                "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080},
                "current_mode": {"width": 1920, "height": 1080, "refresh": 60000},
                "current_workspace": "1"}"#,
        )
        .unwrap();
        let info = parse_monitor(&m).unwrap();
        assert_eq!(info.scale, 1.5);
        assert!(info.is_primary);
        assert_eq!(info.physical_geometry.width, 1920);
        assert_eq!(info.workspace_id.unwrap().native_id, "1");
    }

    #[test]
    fn inactive_output_is_skipped_by_caller_contract() {
        // parse_monitor 只对有 name 的输出生效；无 name（禁用输出占位）→ None。
        let m: Value = serde_json::from_str(r#"{"name": null}"#).unwrap();
        assert!(parse_monitor(&m).is_none());
    }

    #[tokio::test]
    async fn missing_swaysock_fails_with_backend_unavailable() {
        // 临时清除 SWAYSOCK，保证断言在任何 CI 环境下都执行（即便外部
        // 注入了该变量）。
        let prev = std::env::var_os("SWAYSOCK");
        std::env::remove_var("SWAYSOCK");
        // with_base 需要真实 Wayland 连接才能走到 IPC 探测；这里直接验证
        // from_env 的错误映射（connect() 的第一道闸门）。
        let err = SwayIpc::from_env().expect_err("must fail without SWAYSOCK");
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
        // 恢复以避免污染同进程后续测试。
        if let Some(v) = prev {
            std::env::set_var("SWAYSOCK", v);
        }
    }
}
