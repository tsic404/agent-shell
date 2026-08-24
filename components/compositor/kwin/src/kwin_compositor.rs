//! KWinCompositor：KDE 合成器组件（设计文档 §7.7，`mod.rs` 职责）。
//!
//! 组合双通道并实现 [`CompositorComponent`] 全部 17 方法：
//! - Wayland 会话（[`SessionKind::Wayland`]）：`KWinProtocols`（org_kde_*，
//!   基础/首选）+ `KWinBridge`（补充）；
//! - X11 会话（[`SessionKind::X11`]）：无 Wayland 协议通道，窗口管理走
//!   `X11DisplayServer` EWMH/XTest（基础），Scripting 仍为共享补充。
//!
//! 选择逻辑（§7.2 矩阵）：列表/聚焦/最小化/关闭优先协议；移动/缩放/
//! 最大化协议不支持，始终走 Scripting。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::dbus_bridge::KWinBridge;
use crate::error::{KWinError, Result};
use crate::event_script::EventScriptHandle;
use crate::scripts::ScriptTemplate;
use crate::version::{self, KWinVersion};
use crate::wayland::{FakeInput, KWinProtocols, WindowManagement};
use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::AgentShellError;
use agent_shell_core::types::{
    MonitorId, MonitorInfo, Rect, WindowId, WindowInfo, WindowState, WorkspaceId, WorkspaceInfo,
};
use agent_shell_core::{DesktopEnvironment, EventStream};

/// KWin 会话类型（构造时确定，决定基础通道形态）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    /// Wayland 会话：org_kde_* 协议 + Scripting。
    Wayland,
    /// X11 会话：EWMH/XTest + org.kde.KWin D-Bus。
    X11,
}

/// KWin 合成器组件。
pub struct KWinCompositor {
    /// Wayland 协议通道（仅 Wayland 会话为 Some）。
    wayland: Option<KWinProtocols>,
    /// D-Bus / Scripting 补充通道（会话无关，共享）。
    bridge: KWinBridge,
    /// X11 基础通道（仅 X11 会话为 Some；EWMH/XTest 操作由 T1g CLI 与
    /// 输入组件经此通道路由，本组件保留引用以维持会话生命周期）。
    #[allow(dead_code)]
    x11: Option<agent_shell_compositor_x11::X11DisplayServer>,
    /// 探测到的版本（决定脚本 API 形态）。
    version: KWinVersion,
    /// 长驻事件脚本句柄（懒启动）。
    event_handle: AsyncMutex<Option<EventScriptHandle>>,
}

impl KWinCompositor {
    /// 协议通道引用（含派发队列）。
    fn protocols(&self) -> Option<&KWinProtocols> {
        self.wayland.as_ref()
    }

    /// Wayland 会话装配（`KdeBackend::assemble` 约定签名）。
    ///
    /// 连接 `$WAYLAND_DISPLAY`、绑定 org_kde_* globals、探测版本；
    /// 任一通道部分失败都保持可用（回退语义），只有两条通道全不可用才报错。
    pub async fn new_wayland() -> Result<Self> {
        let wl = Arc::new(
            agent_shell_compositor_wayland::WaylandDisplayServer::connect()
                .map_err(|e| KWinError::Scripting(e.to_string()))?,
        );
        let protocols = KWinProtocols::probe(Arc::clone(&wl))?;
        let bridge = KWinBridge::connect().await?;
        let version = version::detect_version(bridge.connection())
            .await
            .unwrap_or_else(|_| KWinVersion {
                full: "unknown".into(),
                major: crate::version::KWinMajor::V6,
            });
        Ok(Self {
            wayland: Some(protocols),
            bridge,
            x11: None,
            version,
            event_handle: AsyncMutex::new(None),
        })
    }

    /// X11 会话装配：连接 X server + D-Bus 桥接。
    pub async fn new_x11() -> Result<Self> {
        let x11 = agent_shell_compositor_x11::X11DisplayServer::connect()
            .map_err(|e| KWinError::Scripting(e.to_string()))?;
        let bridge = KWinBridge::connect().await?;
        let version = version::detect_version(bridge.connection())
            .await
            .unwrap_or_else(|_| KWinVersion {
                full: "unknown".into(),
                major: crate::version::KWinMajor::V6,
            });
        Ok(Self {
            wayland: None,
            bridge,
            x11: Some(x11),
            version,
            event_handle: AsyncMutex::new(None),
        })
    }

    /// 会话类型。
    pub fn session_kind(&self) -> SessionKind {
        if self.wayland.is_some() {
            SessionKind::Wayland
        } else {
            SessionKind::X11
        }
    }

    /// 探测到的 KWin 版本。
    pub fn kwin_version(&self) -> &KWinVersion {
        &self.version
    }

    /// doctor 输出（§7.7 验证输出格式）。
    pub fn doctor_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(p) = &self.wayland {
            let bound = p.bound_count();
            let detail = [
                (
                    "window_mgmt",
                    p.window_mgmt
                        .as_ref()
                        .map(|w| format!("v{}", w.advertised_version)),
                ),
                ("fake_input", p.fake_input.as_ref().map(|_| "v5+".into())),
                ("vd_mgmt", p.vd_mgmt.as_ref().map(|_| "bound".into())),
            ]
            .into_iter()
            .filter_map(|(n, v)| v.map(|v| format!("{n} {v}")))
            .collect::<Vec<_>>()
            .join(", ");
            lines.push(format!(
                "{} Wayland 协议 : {}/3 globals bound ({detail})",
                if bound > 0 { "✓" } else { "⚠" },
                bound
            ));
        }
        // 版本探测结果即 org.kde.KWin 可达性的真实证据（构造时已执行）。
        lines.push(format!(
            "{} KWin 服务   : org.kde.KWin {}",
            if self.version.full != "unknown" {
                "✓"
            } else {
                "⚠"
            },
            match self.version.full.as_str() {
                "unknown" => "version probe failed (supportInformation)".to_string(),
                v => format!("v{v}"),
            }
        ));
        lines.push(format!(
            "✓ D-Bus 桥接 : callDBus ready ({} templates, req-id routed)",
            14
        ));
        if let Some(p) = &self.wayland {
            match &p.fake_input {
                Some(fi) if fi.is_authenticated() => {
                    lines.push("✓ 输入注入   : fake_input authenticated ✓".into())
                }
                Some(_) => lines.push("⚠ 输入注入   : fake_input bound, not authenticated".into()),
                None => lines.push("⚠ 输入注入   : 无 fake_input（降级 ydotool/XTest）".into()),
            }
        }
        // 事件脚本是懒启动（subscribe 时才 load），未启动前如实报告。
        let event_loaded = self
            .event_handle
            .try_lock()
            .map(|h| h.is_some())
            .unwrap_or(false);
        lines.push(if event_loaded {
            "✓ 事件脚本    : loaded (workspace.windowAdded OK)".to_string()
        } else {
            "⚠ 事件脚本    : not started (lazy; starts on first subscribe)".to_string()
        });
        lines
    }

    // ───────────────────────── 内部辅助 ─────────────────────────

    /// window_management 短绑引用。
    fn window_mgmt(&self) -> Option<&WindowManagement> {
        self.wayland.as_ref()?.window_mgmt.as_ref()
    }

    /// fake_input 引用（未 authenticate 视为不可用）。
    fn fake_input(&self) -> Option<&FakeInput> {
        let fi = &self.wayland.as_ref()?.fake_input;
        fi.as_ref().filter(|f| f.is_authenticated())
    }

    /// Scripting 查询封装：渲染模板 → run → 解析 JSON。
    async fn query(&self, tpl: ScriptTemplate, args: &[(&str, Value)]) -> Result<Value> {
        self.bridge
            .run_template(tpl, self.version.is_v6(), args)
            .await
    }

    /// 把 Scripting 返回的窗口 JSON 归一化为 core `WindowInfo`。
    fn parse_window(v: &Value, stacking_order: u32) -> Option<WindowInfo> {
        let id_str = v.get("id")?.as_str()?.to_string();
        let rect = |key: &str| {
            let g = v.get(key).cloned().unwrap_or(Value::Null);
            Rect {
                x: g.get("x").and_then(Value::as_i64).unwrap_or(0) as i32,
                y: g.get("y").and_then(Value::as_i64).unwrap_or(0) as i32,
                width: g.get("width").and_then(Value::as_i64).unwrap_or(0) as i32,
                height: g.get("height").and_then(Value::as_i64).unwrap_or(0) as i32,
            }
        };
        let mut states = Vec::new();
        if v.get("minimized").and_then(Value::as_bool).unwrap_or(false) {
            states.push(WindowState::Minimized);
        }
        if v.get("maximized").and_then(Value::as_bool).unwrap_or(false) {
            states.push(WindowState::Maximized);
        }
        if v.get("fullscreen")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            states.push(WindowState::FullScreen);
        }
        if states.is_empty() {
            states.push(WindowState::Normal);
        }
        let workspace_id = match v.get("desktop").and_then(Value::as_i64) {
            Some(d) if d >= 0 => Some(WorkspaceId {
                native_id: d.to_string(),
                de_type: DesktopEnvironment::KDE,
            }),
            _ => None,
        };
        Some(WindowInfo {
            id: WindowId {
                native_id: id_str,
                de_type: DesktopEnvironment::KDE,
            },
            title: v
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            app_id: v
                .get("appId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            pid: v.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
            geometry: rect("geometry"),
            frame_geometry: rect("frameGeometry"),
            states,
            workspace_id,
            monitor_id: None,
            stacking_order,
            desktop_file: v
                .get("desktopFile")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            window_type: parse_window_type(v.get("windowType")),
            icon_geometry: None,
            keep_above: v.get("keepAbove").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// Scripting 操作结果 `{success: bool, error?}` 校验。
    fn check_op(v: &Value) -> Result<()> {
        if v.get("success").and_then(Value::as_bool).unwrap_or(false) {
            Ok(())
        } else {
            Err(KWinError::Scripting(format!(
                "op failed: {}",
                v.get("error").and_then(Value::as_str).unwrap_or("unknown")
            )))
        }
    }
}

/// Scripting windowType 值归一化到 core WindowType。
fn parse_window_type(v: Option<&Value>) -> agent_shell_core::types::WindowType {
    use agent_shell_core::types::WindowType;
    match v.and_then(Value::as_str) {
        Some("normal") => WindowType::Normal,
        Some("dialog") => WindowType::Dialog,
        Some("dock") => WindowType::Dock,
        Some("desktop") => WindowType::Desktop,
        Some("dropdown_menu") | Some("menu") => WindowType::DropdownMenu,
        Some("tooltip") => WindowType::Tooltip,
        Some("notification") => WindowType::Notification,
        Some("splash") => WindowType::Splash,
        Some("utility") => WindowType::Utility,
        _ => WindowType::Unknown,
    }
}

/// 协议通道的窗口列表：stacking order uuids → get_window_by_uuid → 事件聚合。
#[async_trait]
impl DesktopComponent for KWinCompositor {
    fn name(&self) -> &'static str {
        "kwin-compositor"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }

    fn is_available(&self) -> bool {
        true // 构造成功即可用（部分降级由 health 表达）
    }

    async fn health(&self) -> ComponentHealth {
        match (&self.wayland, self.window_mgmt()) {
            (Some(p), Some(_)) if p.bind_failures().is_empty() => ComponentHealth::Healthy,
            (Some(_), Some(_)) => {
                ComponentHealth::Degraded("protocol partial; scripting fallback active".into())
            }
            (Some(_), None) => {
                ComponentHealth::Degraded("window_mgmt unbound; scripting fallback".into())
            }
            (None, _) => ComponentHealth::Degraded("X11 session; EWMH + scripting".into()),
        }
    }
}

/// 通道间错误语义（§7.2 矩阵的显式化，🟡2）：
///
/// | 操作 | 协议路径 | Scripting 路径 |
/// |------|---------|---------------|
/// | focus/minimize/unminimize/close | 发完即 Ok（wayland 请求无回执）；uuid 不存在时 compositor 静默忽略，**不报错** | 窗口不存在返回 `Window not found` 错误 |
/// | move/resize/maximize/set_geometry | 不可用（协议无 set_geometry），始终 Scripting | 同上报错语义 |
///
/// 即：协议通道「乐观发送」，Scripting 通道「确认式」。同一 uuid 在
/// 两通道下的失败表现不同——调用方以 `get_window_info` 预校验可消除
/// 差异；T3b 协议事件聚合落地后统一为确认式。
#[async_trait]
impl CompositorComponent for KWinCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            window_management: true, // 协议或 Scripting 至少其一
            workspace_management: true,
            monitor_layout: true,
            // 🔴3：event_monitor.js 已可启动并推送原始 JSON，但 DesktopEvent
            // 归一化在 T3b 落地——在此之前不声明该能力，避免调用方依赖
            // 一个语义未定的流。
            window_events: false,
            workspace_events: false, // T3b
            native_input: self.fake_input().is_some(),
            native_capture: false, // kde-screencast 归 capture 组件（T2b）
            virtual_desktops: true,
            effects_control: false,
        }
    }

    /// 窗口列表：始终走 list_windows.js（一次 callDBus 批量取全量详情）。
    ///
    /// 协议 stacking-order 仅提供 uuid 列表，逐窗 get_window_by_uuid 仍需
    /// 事件聚合才能取属性（本层 inert 不消费事件）——T3b 前纯协议路径
    /// 无法给出 WindowInfo，故不在此付 roundtrip 开销。window_mgmt 短绑
    /// 状态只影响 focus/minimize/close 走协议还是 Scripting。
    async fn list_windows(&self) -> agent_shell_core::error::Result<Vec<WindowInfo>> {
        let v = self.query(ScriptTemplate::ListWindows, &[]).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .enumerate()
            .filter_map(|(i, w)| Self::parse_window(w, i as u32))
            .collect())
    }

    /// 当前活动窗口（可能为空——桌面无焦点）。
    async fn get_active_window(&self) -> agent_shell_core::error::Result<Option<WindowInfo>> {
        let v = self.query(ScriptTemplate::GetActiveWindow, &[]).await?;
        Ok(if v.is_null() {
            None
        } else {
            Self::parse_window(&v, 0)
        })
    }

    /// 聚焦：协议 activate 优先（请求发出即成功——wayland 请求无回执），
    /// 未短绑时回退 focus_window.js。
    async fn focus_window(&self, id: &WindowId) -> agent_shell_core::error::Result<()> {
        if let Some(p) = self.protocols() {
            if let Some(wm) = p.window_mgmt.as_ref() {
                let qh = p.queue_handle();
                let win = wm.get_window_by_uuid(&qh, &id.native_id);
                wm.activate(&win);
                return Ok(());
            }
        }
        let v = self
            .query(ScriptTemplate::FocusWindow, &[("ID", json!(id.native_id))])
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 移动：协议无 set_geometry（§7.2），始终 Scripting。
    async fn move_window(
        &self,
        id: &WindowId,
        x: i32,
        y: i32,
    ) -> agent_shell_core::error::Result<()> {
        let v = self
            .query(
                ScriptTemplate::MoveWindow,
                &[
                    ("ID", json!(id.native_id)),
                    ("X", json!(x)),
                    ("Y", json!(y)),
                ],
            )
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 缩放：同 move_window，始终 Scripting。
    async fn resize_window(
        &self,
        id: &WindowId,
        w: i32,
        h: i32,
    ) -> agent_shell_core::error::Result<()> {
        let v = self
            .query(
                ScriptTemplate::ResizeWindow,
                &[
                    ("ID", json!(id.native_id)),
                    ("W", json!(w)),
                    ("H", json!(h)),
                ],
            )
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 最小化(true)/还原(false)：协议 set_state 位操作优先。
    async fn minimize_window(&self, id: &WindowId) -> agent_shell_core::error::Result<()> {
        if let Some(p) = self.protocols() {
            if let Some(wm) = p.window_mgmt.as_ref() {
                let qh = p.queue_handle();
                let win = wm.get_window_by_uuid(&qh, &id.native_id);
                wm.set_minimized(&win, true);
                return Ok(());
            }
        }
        let v = self
            .query(
                ScriptTemplate::MinimizeWindow,
                &[("ID", json!(id.native_id)), ("NUM", json!(1))],
            )
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    async fn unminimize_window(&self, id: &WindowId) -> agent_shell_core::error::Result<()> {
        if let Some(p) = self.protocols() {
            if let Some(wm) = p.window_mgmt.as_ref() {
                let qh = p.queue_handle();
                let win = wm.get_window_by_uuid(&qh, &id.native_id);
                wm.set_minimized(&win, false);
                return Ok(());
            }
        }
        let v = self
            .query(
                ScriptTemplate::MinimizeWindow,
                &[("ID", json!(id.native_id)), ("NUM", json!(0))],
            )
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 最大化：协议不支持（§7.2），始终 Scripting。
    async fn maximize_window(&self, id: &WindowId) -> agent_shell_core::error::Result<()> {
        let v = self
            .query(
                ScriptTemplate::MaximizeWindow,
                &[("ID", json!(id.native_id)), ("NUM", json!(1))],
            )
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 关闭：协议 close 优先。
    async fn close_window(&self, id: &WindowId) -> agent_shell_core::error::Result<()> {
        if let Some(p) = self.protocols() {
            if let Some(wm) = p.window_mgmt.as_ref() {
                let qh = p.queue_handle();
                let win = wm.get_window_by_uuid(&qh, &id.native_id);
                wm.close(&win);
                return Ok(());
            }
        }
        let v = self
            .query(ScriptTemplate::CloseWindow, &[("ID", json!(id.native_id))])
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 几何一次设定：Scripting（协议无 set_geometry）。
    async fn set_window_geometry(
        &self,
        id: &WindowId,
        geo: Rect,
    ) -> agent_shell_core::error::Result<()> {
        let v = self
            .query(
                ScriptTemplate::SetWindowGeometry,
                &[
                    ("ID", json!(id.native_id)),
                    ("X", json!(geo.x)),
                    ("Y", json!(geo.y)),
                    ("W", json!(geo.width)),
                    ("H", json!(geo.height)),
                ],
            )
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 单窗查询：list_windows 过滤（协议 get_window_by_uuid 仅给对象句柄，
    /// 属性仍需事件聚合——T3b 前以 Scripting 为准）。
    async fn get_window_info(&self, id: &WindowId) -> agent_shell_core::error::Result<WindowInfo> {
        let windows = self.list_windows().await?;
        windows
            .into_iter()
            .find(|w| w.id == *id)
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))
    }

    /// 工作区列表：list_workspaces.js。
    async fn list_workspaces(&self) -> agent_shell_core::error::Result<Vec<WorkspaceInfo>> {
        let v = self.query(ScriptTemplate::ListWorkspaces, &[]).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .map(|d| WorkspaceInfo {
                id: WorkspaceId {
                    native_id: d
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    de_type: DesktopEnvironment::KDE,
                },
                name: d
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                number: d.get("number").and_then(Value::as_u64).unwrap_or(0) as u32,
                is_active: d.get("isActive").and_then(Value::as_bool).unwrap_or(false),
                monitor_ids: Vec::new(),
                window_ids: Vec::new(),
            })
            .collect())
    }

    /// 激活工作区：switch_workspace.js（协议 vd_mgmt 的 request_activate 需要
    /// 先有桌面对象缓存，T3b 事件任务补全后切换为协议优先）。
    async fn activate_workspace(&self, id: &WorkspaceId) -> agent_shell_core::error::Result<()> {
        let v = self
            .query(
                ScriptTemplate::SwitchWorkspace,
                &[("WS", json!(id.native_id))],
            )
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 移动窗口到工作区：move_window_to_workspace.js。
    async fn move_window_to_workspace(
        &self,
        wid: &WindowId,
        ws: &WorkspaceId,
    ) -> agent_shell_core::error::Result<()> {
        let v = self
            .query(
                ScriptTemplate::MoveWindowToWorkspace,
                &[("ID", json!(wid.native_id)), ("WS", json!(ws.native_id))],
            )
            .await?;
        Self::check_op(&v).map_err(KWinError::into)
    }

    /// 显示器列表：list_monitors.js（workspace.screens）。
    async fn list_monitors(&self) -> agent_shell_core::error::Result<Vec<MonitorInfo>> {
        let v = self.query(ScriptTemplate::ListMonitors, &[]).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .map(|m| {
                let g = m.get("geometry").cloned().unwrap_or(Value::Null);
                MonitorInfo {
                    id: MonitorId {
                        native_id: m
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        de_type: DesktopEnvironment::KDE,
                    },
                    name: m
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    geometry: Rect {
                        x: g.get("x").and_then(Value::as_i64).unwrap_or(0) as i32,
                        y: g.get("y").and_then(Value::as_i64).unwrap_or(0) as i32,
                        width: g.get("width").and_then(Value::as_i64).unwrap_or(0) as i32,
                        height: g.get("height").and_then(Value::as_i64).unwrap_or(0) as i32,
                    },
                    physical_geometry: Rect::default(),
                    scale: m.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
                    is_primary: m.get("isPrimary").and_then(Value::as_bool).unwrap_or(false),
                    workspace_id: None,
                }
            })
            .collect())
    }

    /// 订阅事件流：确保长驻 event_monitor.js 在跑，返回其读取端。
    ///
    /// 返回的是**原始推送流**（`KWinEventStream`）：每条为 event_monitor.js
    /// 的 sendResult JSON（`{"event": "windowOpened", "id": ...}`）。
    /// `capabilities().window_events` 为 false——DesktopEvent 归一化在 T3b
    /// 落地；调用方若仍订阅，拿到的是明确的原始流而非永不产出的空壳。
    async fn subscribe(&self) -> agent_shell_core::error::Result<Box<dyn EventStream>> {
        let stream = self.bridge.take_event_stream().await.ok_or_else(|| {
            AgentShellError::BackendUnavailable("kwin event stream already subscribed".to_string())
        })?;
        let mut handle = self.event_handle.lock().await;
        if handle.is_none() {
            // 幂等启动；句柄保存在组件内直到 stop/drop。
            let h = crate::event_script::spawn_event_monitor(&self.bridge).await?;
            *handle = Some(EventScriptHandle::from_parts(
                h.object_path().to_string(),
                h.running_clone(),
            ));
        }
        Ok(Box::new(stream))
    }
}

/// 测试与诊断：通道组合摘要。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_kind_names() {
        // 纯枚举稳定性检查（构造函数需要真实显示服务器，见集成测试）。
        assert_ne!(
            format!("{:?}", SessionKind::Wayland),
            format!("{:?}", SessionKind::X11)
        );
    }
}
