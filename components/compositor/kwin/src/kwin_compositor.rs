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

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::dbus_bridge::KWinBridge;
use crate::error::{KWinError, Result};
use crate::event_script::EventScriptHandle;
use crate::scripts::ScriptTemplate;
use crate::version::{self, KWinVersion};
use crate::wayland::{FakeInput, KWinProtocols, WindowManagement};
use agent_shell_compositor_wayland_core::WaylandCompositor;
use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::AgentShellError;
use agent_shell_core::types::{
    MonitorId, MonitorInfo, Rect, WindowId, WindowInfo, WindowState, WorkspaceId, WorkspaceInfo,
};
use agent_shell_core::{DesktopEnvironment, EventStream};
use agent_shell_displayserver_wayland::WaylandDisplayServer;
use agent_shell_displayserver_x11::X11DisplayServer;

/// KWin 会话类型（构造时确定，决定基础通道形态）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    /// Wayland 会话：org_kde_* 协议 + Scripting。
    Wayland,
    /// X11 会话：EWMH/XTest + org.kde.KWin D-Bus。
    X11,
}

/// KWin 合成器组件（§3.3：`KWinCompositor` 直接继承 `WaylandCompositor`）。
///
/// 继承表达：Wayland 会话下持有纯 core 的 [`WaylandDisplayServer`] 基类
/// 通道（`display_server()` 返回它），叠加 org_kde_* 私有协议
/// （[`KWinProtocols`]）；X11 会话下组合 [`X11DisplayServer`]（EWMH +
/// ICCCM + XTest），Scripting 仍为共享补充通道。
pub struct KWinCompositor {
    /// 基类协议通道（仅 Wayland 会话为 Some；私有协议叠加其上）。
    wayland_core: Option<WaylandDisplayServer>,
    /// org_kde_* 私有协议通道（仅 Wayland 会话为 Some，叠加在基类之上）。
    protocols: Option<KWinProtocols>,
    /// D-Bus / Scripting 补充通道（会话无关，共享）。
    bridge: KWinBridge,
    /// X11 基础通道（仅 X11 会话为 Some；EWMH/ICCCM/XTest 操作由 T1g CLI
    /// 与输入组件经此通道路由，本组件保留引用以维持会话生命周期）。
    #[allow(dead_code)]
    x11: Option<X11DisplayServer>,
    /// 探测到的版本（决定脚本 API 形态）。
    version: KWinVersion,
    /// 长驻事件脚本句柄（懒启动）。
    event_handle: AsyncMutex<Option<EventScriptHandle>>,
    /// `/Scripting` 探测状态（TSI-2374）：0=未探测，1=失败（不缓存，
    /// 允许重试），2=成功。原子而非锁——doctor_lines(&self) 同步读取。
    scripting_probe: std::sync::atomic::AtomicU8,
}

/// `scripting_probe` 状态值（TSI-2374）。
const PROBE_UNSET: u8 = 0;
const PROBE_FAIL: u8 = 1;
const PROBE_OK: u8 = 2;

impl KWinCompositor {
    /// org_kde_* 私有协议通道引用（含派发队列）。
    fn protocols(&self) -> Option<&KWinProtocols> {
        self.protocols.as_ref()
    }
    /// WaylandCompositor 基类通道（§3.3：Wayland 系合成器共享的纯 core 层）。
    ///
    /// X11 会话无 Wayland 通道——此时合成器不经 `WaylandCompositor`
    /// 抽象使用（EWMH/ICCCM 基础通道为 `x11` 字段），与设计文档
    /// 「KWinCompositor 组合 WaylandDisplayServer + X11DisplayServer」一致。
    pub fn wayland_display_server(&self) -> Option<&WaylandDisplayServer> {
        self.wayland_core.as_ref()
    }

    /// Wayland 会话装配（`KdeBackend::assemble` 约定签名）。
    ///
    /// 连接 `$WAYLAND_DISPLAY`、绑定 org_kde_* globals、探测版本；
    /// 任一通道部分失败都保持可用（回退语义），只有两条通道全不可用才报错。
    pub async fn new_wayland() -> Result<Self> {
        let wl =
            WaylandDisplayServer::connect().map_err(|e| KWinError::Scripting(e.to_string()))?;
        let protocols = KWinProtocols::probe(&wl)?;
        let bridge = KWinBridge::connect().await?;
        let version = version::detect_version(bridge.connection())
            .await
            .unwrap_or_else(|_| KWinVersion {
                full: "unknown".into(),
                major: crate::version::KWinMajor::V6,
            });
        Ok(Self {
            wayland_core: Some(wl),
            protocols: Some(protocols),
            bridge,
            x11: None,
            version,
            event_handle: AsyncMutex::new(None),
            scripting_probe: std::sync::atomic::AtomicU8::new(PROBE_UNSET),
        })
    }

    /// X11 会话装配：连接 X server + D-Bus 桥接。
    pub async fn new_x11() -> Result<Self> {
        let x11 = X11DisplayServer::connect().map_err(|e| KWinError::Scripting(e.to_string()))?;
        let bridge = KWinBridge::connect().await?;
        let version = version::detect_version(bridge.connection())
            .await
            .unwrap_or_else(|_| KWinVersion {
                full: "unknown".into(),
                major: crate::version::KWinMajor::V6,
            });
        Ok(Self {
            wayland_core: None,
            protocols: None,
            bridge,
            x11: Some(x11),
            version,
            event_handle: AsyncMutex::new(None),
            scripting_probe: std::sync::atomic::AtomicU8::new(PROBE_UNSET),
        })
    }

    /// 测试注入点（TSI-2502/TSI-2505）：跨 crate 测试需绕过真实显示
    /// 服务器/版本探测路径构造最小实例。给定桥接连接与 `/Scripting`
    /// 探测初值：`None`=未探测，`Some(false)`=最近失败，`Some(true)`=
    /// 已确认可用。
    ///
    /// 生产构造路径（`new_wayland` / `new_x11`）不受影响；本构造函数
    /// 不触碰 `ensure_scripting_probe` 的真实探测逻辑。
    ///
    /// 权衡：`#[doc(hidden)]` 只隐藏文档、不隐藏符号，release 构建中下游
    /// crate 仍可调用本测试构造器。采纳该模式是因为 DDE 组件的集成测试
    /// 需要从 crate 外注入最小 KWin 实例（`Option<bool>` 表达三态探测），
    /// 而 `#[cfg(test)]` 注入点对下游 crate 不可见；风险仅限误用构造器，
    /// 不触及真实探测/构造路径。
    #[doc(hidden)]
    pub fn for_test(bridge: KWinBridge, probe: Option<bool>) -> Self {
        use std::sync::atomic::AtomicU8;
        let probe = match probe {
            Some(true) => PROBE_OK,
            Some(false) => PROBE_FAIL,
            None => PROBE_UNSET,
        };
        Self {
            wayland_core: None,
            protocols: None,
            bridge,
            x11: None,
            version: KWinVersion {
                full: "6.1.4".into(),
                major: crate::version::KWinMajor::V6,
            },
            event_handle: AsyncMutex::new(None),
            scripting_probe: AtomicU8::new(probe),
        }
    }

    /// 会话类型。
    pub fn session_kind(&self) -> SessionKind {
        if self.wayland_core.is_some() {
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
        if let Some(p) = &self.protocols {
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
        // 桥接就绪以 /Scripting 探测为证据（TSI-2374）——不再无条件打 ✓。
        lines.push(match self.scripting_probe_ok() {
            Some(true) => "✓ D-Bus 桥接 : callDBus ready (14 templates, req-id routed; \
                           /Scripting introspected)"
                .to_string(),
            Some(false) => "⚠ D-Bus 桥接 : /Scripting 未就绪（KWin 启动早期或不可达；\
                            Scripting 调用将按需重试，Wayland 协议通道不受影响）"
                .to_string(),
            None => "⚠ D-Bus 桥接 : 未探测（调用 ensure_scripting_probe 后更新）".to_string(),
        });
        if let Some(p) = &self.protocols {
            match &p.fake_input {
                Some(fi) if fi.is_authenticated() => {
                    lines.push("✓ 输入注入   : fake_input authenticated ✓".into())
                }
                Some(_) => lines.push("⚠ 输入注入   : fake_input bound, not authenticated".into()),
                None => lines.push(
                    "⚠ 输入注入   : 无 fake_input（降级 libei/ydotool/XTest/xdotool）".into(),
                ),
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
            "⚠ 事件脚本    : 可选（T3b 待办；daemon 未装配事件归一化管线，subscribe 未接线）"
                .to_string()
        });
        lines
    }

    /// doctor 输出的异步版本：在渲染桥接行前触发一次 `/Scripting` 探测
    /// （懒探测缓存，成功后升级为确认态）。
    ///
    /// TSI-2486：doctor 路径从不调用 [`Self::ensure_scripting_probe`]，
    /// 导致 D-Bus 桥接行恒为「未探测」——尽管 `org.kde.KWin` 的
    /// `/Scripting` 实际可达。同步版本 [`Self::doctor_lines`] 保留给
    /// 内部状态渲染；daemon doctor 走本方法补齐证据后渲染。
    pub async fn doctor_lines_async(&self) -> Vec<String> {
        if self.scripting_probe_ok() != Some(true) {
            let _ = self.ensure_scripting_probe().await;
        }
        self.doctor_lines()
    }

    /// 探测并缓存 `/Scripting` 可用性（TSI-2374）。
    ///
    /// doctor 与降级链的证据来源：成功后 `doctor_lines` 的桥接行升级为
    /// 确认态；失败写入 PROBE_FAIL（doctor 显示「未就绪」而非「未探测」），
    /// 但不阻止下次调用重试——KWin 启动早期未就绪属时序现象，可自愈。
    pub async fn ensure_scripting_probe(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.scripting_probe.load(Ordering::Relaxed) == PROBE_OK {
            return Ok(());
        }
        let result = crate::dbus_bridge::probe_scripting(self.bridge.connection()).await;
        self.scripting_probe.store(
            if result.is_ok() { PROBE_OK } else { PROBE_FAIL },
            Ordering::Relaxed,
        );
        result
    }

    /// 探测状态读取端（doctor_lines 用）：Some(true)=已确认可用，
    /// Some(false)=最近一次失败，None=尚未探测。
    fn scripting_probe_ok(&self) -> Option<bool> {
        use std::sync::atomic::Ordering;
        match self.scripting_probe.load(Ordering::Relaxed) {
            PROBE_OK => Some(true),
            PROBE_FAIL => Some(false),
            PROBE_UNSET => None,
            _ => None,
        }
    }

    // ───────────────────────── 内部辅助 ─────────────────────────

    /// window_management 短绑引用。
    fn window_mgmt(&self) -> Option<&WindowManagement> {
        self.protocols.as_ref()?.window_mgmt.as_ref()
    }

    /// fake_input 引用（未 authenticate 视为不可用）。
    fn fake_input(&self) -> Option<&FakeInput> {
        let fi = &self.protocols.as_ref()?.fake_input;
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
        // 以 org_kde_* 私有协议通道（KWinProtocols）的真实绑定失败记录为准——
        // 基类通道 wayland_core 的 bind_failures 恒为空（纯 core 层无协议绑定）。
        match (self.protocols.as_ref(), self.window_mgmt()) {
            (Some(p), Some(_)) if p.bind_failures().is_empty() => ComponentHealth::Healthy,
            (Some(_), Some(_)) => {
                ComponentHealth::Degraded("protocol partial; scripting fallback active".into())
            }
            (Some(_), None) => {
                ComponentHealth::Degraded("window_mgmt unbound; scripting fallback".into())
            }
            (None, _) => ComponentHealth::Degraded("X11 session; EWMH/ICCCM + scripting".into()),
        }
    }
}

/// §3.3 继承层次落地：`KWinCompositor` 直接继承 `WaylandCompositor`
/// （组合 `WaylandDisplayServer`，叠加 org_kde_* 私有协议）。
///
/// 仅 Wayland 会话满足本抽象——X11 会话下合成器的基础通道是
/// `X11DisplayServer`（EWMH/ICCCM），不经 Wayland 系抽象使用。
impl WaylandCompositor for KWinCompositor {
    fn display_server(&self) -> &WaylandDisplayServer {
        self.wayland_core
            .as_ref()
            .expect("WaylandCompositor is only implemented for Wayland sessions; check session_kind() first")
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

    /// 事件脚本懒启动（首次 `subscribe()` 才 load `event_monitor.js`），
    /// 因此 `window_events`/`workspace_events` 激活前为 false 而非永久不可用。
    fn lazy_capabilities(&self) -> &'static [&'static str] {
        &["window_events", "workspace_events"]
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
        // 订阅前刷新 /Scripting 探测（TSI-2374）：启动早期未就绪时由
        // spawn_event_script 内部的重试探测兜底，这里只做缓存预热。
        let _ = self.ensure_scripting_probe().await;
        let mut handle = self.event_handle.lock().await;
        if handle.is_none() {
            // 幂等启动；句柄（含 StagedScript 暂存文件）原样保存在组件内
            // 直到 stop/drop——不得重建副本，否则暂存文件被提前 Drop 删除。
            *handle = Some(
                crate::event_script::spawn_event_monitor(&self.bridge, self.version.is_v6())
                    .await?,
            );
        }
        Ok(Box::new(stream))
    }
}

/// 测试与诊断：通道组合摘要 + `/Scripting` 探测三态迁移（TSI-2502）。
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// 独立私有 session bus（避免 `Connection::session()` 环境变量在并行
    /// 测试间竞争）。daemon 与桥接/被测对象共享同一地址。
    struct TestBus {
        addr: String,
        _child: std::process::Child,
    }

    impl TestBus {
        async fn start() -> Self {
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("dbus-daemon must be installed for kwin probe tests");
            let stdout = child.stdout.take().expect("piped stdout");
            let addr = read_address_line(stdout);
            assert!(
                addr.starts_with("unix:"),
                "dbus-daemon printed unexpected address: {addr:?}"
            );
            Self {
                addr,
                _child: child,
            }
        }

        async fn connect(&self) -> zbus::Connection {
            zbus::connection::Builder::address(self.addr.as_str())
                .expect("dbus-daemon address must parse")
                .build()
                .await
                .expect("connect to private session bus")
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self._child.kill();
            let _ = self._child.wait();
        }
    }

    /// 逐字节读地址行：`dbus-daemon --print-address=1` 恰好一行，
    /// 不依赖 `read_line` 缓冲是否越界吞掉后续（daemon 无后续输出）。
    fn read_address_line(stdout: std::process::ChildStdout) -> String {
        use std::io::Read as _;
        let mut reader = std::io::BufReader::new(stdout);
        let mut bytes = Vec::new();
        loop {
            let mut buf = [0u8; 1];
            match reader.read_exact(&mut buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("read dbus-daemon address: {e}"),
            }
            bytes.push(buf[0]);
            if buf[0] == b'\n' {
                break;
            }
        }
        let line = String::from_utf8(bytes).expect("dbus-daemon address must be UTF-8");
        assert!(!line.is_empty(), "dbus-daemon printed no address line");
        line.trim_end_matches('\n').to_string()
    }
    /// 注册 org.kde.KWin 的 /Scripting 单例（仅声明接口，供 introspect 判定）。
    #[derive(Clone, Copy)]
    struct KWinScripting;

    #[zbus::interface(name = "org.kde.kwin.Scripting")]
    impl KWinScripting {
        fn load_script(&self, _file_path: String, _plugin_name: String) -> i32 {
            0
        }
    }

    impl KWinScripting {
        fn new() -> Self {
            Self
        }
    }

    async fn spawn_fake_kwin(bus: &TestBus) -> zbus::Connection {
        let conn = bus.connect().await;
        conn.object_server()
            .at("/Scripting", KWinScripting::new())
            .await
            .expect("register /Scripting");
        use zbus::names::WellKnownName;
        let name = WellKnownName::try_from("org.kde.KWin".to_string()).expect("valid bus name");
        conn.request_name(name).await.expect("claim org.kde.KWin");
        conn
    }

    async fn bridge(bus: &TestBus) -> KWinBridge {
        let conn = bus.connect().await;
        KWinBridge::with_connection(conn)
            .await
            .expect("build KWinBridge on private bus")
    }

    fn has_ready_bridge(lines: &[String]) -> bool {
        lines.iter().any(|l| l.contains("✓ D-Bus 桥接"))
    }

    fn has_not_ready_bridge(lines: &[String]) -> bool {
        lines.iter().any(|l| l.contains("未就绪"))
    }

    #[test]
    fn session_kind_names() {
        // 纯枚举稳定性检查（构造函数需要真实显示服务器，见集成测试）。
        assert_ne!(
            format!("{:?}", SessionKind::Wayland),
            format!("{:?}", SessionKind::X11)
        );
    }

    /// 懒启动能力声明：事件脚本首次 `subscribe()` 才 load，`capabilities()`
    /// 的 `window_events`/`workspace_events` false 非永久不可用（TSI-2822）。
    #[tokio::test]
    async fn lazy_capabilities_lists_event_streams() {
        let bus = TestBus::start().await;
        let comp = KWinCompositor::for_test(bridge(&bus).await, None);
        assert_eq!(
            comp.lazy_capabilities(),
            &["window_events", "workspace_events"]
        );
    }

    /// 三态迁移：`None`（PROBE_UNSET）触发探测 → 成功升级为 Some(true)。
    #[tokio::test]
    async fn unset_probe_triggers_probe_and_becomes_ok() {
        let bus = TestBus::start().await;
        let _kwin = spawn_fake_kwin(&bus).await;
        let comp = KWinCompositor::for_test(bridge(&bus).await, None);

        assert_eq!(comp.scripting_probe_ok(), None);
        let lines = comp.doctor_lines_async().await;

        assert_eq!(comp.scripting_probe_ok(), Some(true));
        assert!(has_ready_bridge(&lines));
    }

    /// TSI-2486 回归守卫：`Some(false)`（PROBE_FAIL）必须重试，不能把
    /// 一次性失败固化为永不重试的假阴性。
    #[tokio::test]
    async fn failed_probe_is_retried_and_becomes_ok() {
        let bus = TestBus::start().await;
        // 先建桥（无 org.kde.KWin 服务），在桥接上探测一次失败。
        let comp = KWinCompositor::for_test(bridge(&bus).await, None);
        let _ = comp.ensure_scripting_probe().await;
        assert_eq!(comp.scripting_probe_ok(), Some(false));

        // 服务事后可达——旧失败必须被重试，升级为确认态。
        let _kwin = spawn_fake_kwin(&bus).await;
        let lines = comp.doctor_lines_async().await;

        assert_eq!(comp.scripting_probe_ok(), Some(true));
        assert!(has_ready_bridge(&lines));
    }

    /// 三态迁移：`Some(true)`（PROBE_OK）短路，不再发探测。
    #[tokio::test]
    async fn ok_probe_short_circuits_without_probing() {
        let bus = TestBus::start().await;
        // 不注册 org.kde.KWin：若短路失败，doctor_lines_async 会重测并
        // 把 PROBE_OK 覆写为 PROBE_FAIL。
        let comp = KWinCompositor::for_test(bridge(&bus).await, Some(true));

        let lines = comp.doctor_lines_async().await;

        assert_eq!(comp.scripting_probe_ok(), Some(true));
        assert!(has_ready_bridge(&lines));
    }

    /// 三态迁移：`Some(false)` 服务仍不可达 → 保持 PROBE_FAIL，桥接行
    /// 报「未就绪」而非「未探测」。
    #[tokio::test]
    async fn failed_probe_remains_failed_when_still_unreachable() {
        let bus = TestBus::start().await;
        let comp = KWinCompositor::for_test(bridge(&bus).await, Some(false));

        let lines = comp.doctor_lines_async().await;

        assert_eq!(comp.scripting_probe_ok(), Some(false));
        assert!(has_not_ready_bridge(&lines));
        assert!(!has_ready_bridge(&lines));
    }

    /// doctor 事件脚本行如实标注为可选（T3b 待办），而非以「未装配/未接线」
    /// 呈现为待修复缺口（TSI-2912）。
    #[tokio::test]
    async fn doctor_event_script_line_marks_optional() {
        let bus = TestBus::start().await;
        let comp = KWinCompositor::for_test(bridge(&bus).await, None);
        let lines = comp.doctor_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("⚠ 事件脚本") && l.contains("可选")),
            "event script line must mark optional: {lines:#?}"
        );
    }
}
