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

use crate::dbus_bridge::{KWinBridge, RegistrationOutcome};
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
    MonitorId, MonitorInfo, Rect, WindowId, WindowInfo, WindowState, WindowType, WorkspaceId,
    WorkspaceInfo,
};
use agent_shell_core::{DesktopEnvironment, EventStream};
use agent_shell_displayserver_wayland::WaylandDisplayServer;
use agent_shell_displayserver_x11::{EwmhAtoms, X11DisplayServer};

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
    /// X11 基础通道（仅 X11 会话为 Some）。窗口枚举/聚焦/移动/工作区/事件
    /// 走 EWMH（`_NET_CLIENT_LIST`、`_NET_ACTIVE_WINDOW`、
    /// `_NET_MOVERESIZE_WINDOW`、`_NET_NUMBER_OF_DESKTOPS` 等）而非 Scripting
    /// `/Scripting`——部分 KWin 5.x X11 会话不注册该对象路径，
    /// 窗口管理与工作区通道必须落在 X11 原生路径上。`Arc` 供 EWMH 事件线程
    /// 与同步操作并发共享同一连接（x11rb `RustConnection` 内部互斥 + Condvar，
    /// 多线程读写安全）。
    x11: Option<Arc<X11DisplayServer>>,
    /// 探测到的版本（决定脚本 API 形态）。
    version: KWinVersion,
    /// 长驻事件脚本句柄（懒启动；仅 Wayland 会话使用）。
    event_handle: AsyncMutex<Option<EventScriptHandle>>,
    /// 事件脚本信号注册状态（doctor_lines 同步读取）：None=未加载，
    /// `Some(Ok(()))`=注册成功，`Some(Err(msg))`=最近一次注册失败诊断。
    /// 独立 std Mutex 而非 AsyncMutex——doctor_lines(&self) 同步渲染。
    event_registration: std::sync::Mutex<Option<std::result::Result<(), String>>>,
    /// X11 会话的 EWMH 事件监视器（懒启动；订阅后事件线程常驻）。
    ewmh_monitor: AsyncMutex<Option<crate::event_ewmh::EwmhEventMonitor>>,
    /// `/Scripting` 探测状态：0=未探测，1=失败（不缓存，
    /// 允许重试），2=成功。原子而非锁——doctor_lines(&self) 同步读取。
    scripting_probe: std::sync::atomic::AtomicU8,
}

/// `scripting_probe` 状态值。
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
            event_registration: std::sync::Mutex::new(None),
            event_handle: AsyncMutex::new(None),
            ewmh_monitor: AsyncMutex::new(None),
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
            x11: Some(Arc::new(x11)),
            version,
            event_registration: std::sync::Mutex::new(None),
            event_handle: AsyncMutex::new(None),
            ewmh_monitor: AsyncMutex::new(None),
            scripting_probe: std::sync::atomic::AtomicU8::new(PROBE_UNSET),
        })
    }

    /// 测试注入点：跨 crate 测试绕过真实显示服务器/版本探测，构造最小实例。
    /// `probe` 表达 `/Scripting` 三态初值：`None`=未探测，`Some(false)`=最近失败，
    /// `Some(true)`=已确认可用；生产构造路径（`new_wayland` / `new_x11`）不受影响。
    ///
    /// `#[doc(hidden)]` 只隐藏文档不隐藏符号——DDE 集成测试需从 crate 外注入
    /// 最小实例（`Option<bool>` 表达三态），`#[cfg(test)]` 注入点对下游 crate
    /// 不可见；风险仅限误用构造器，不触及真实探测/构造路径。
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
            event_registration: std::sync::Mutex::new(None),
            event_handle: AsyncMutex::new(None),
            ewmh_monitor: AsyncMutex::new(None),
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
        // 桥接就绪以 /Scripting 探测为证据——不再无条件打 ✓。
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
        // 事件脚本是懒启动（subscribe 时才 load），未启动前如实报告；
        // 启动后以信号注册结果为准（成功 / 失败诊断），而非仅「loaded」。
        let event_status = self
            .event_registration
            .try_lock()
            .map(|s| s.clone())
            .unwrap_or(None);
        lines.push(match event_status {
            Some(Ok(())) => {
                "✓ 事件脚本    : signals registered (windowAdded/windowRemoved/windowActivated OK)"
                    .to_string()
            }
            Some(Err(msg)) => {
                format!("✗ 事件脚本    : signal registration failed: {msg}")
            }
            // 本组件未加载：若残留/外部实例曾发出注册标记（无等待者被记录），
            // 据此区分「加载后零注册」与真正的「未加载」。
            None => match self.bridge.late_registration() {
                Some(RegistrationOutcome::Ready) => {
                    "⚠ 事件脚本    : 检测到外部实例已注册（本组件未加载，疑似残留 event_monitor）"
                        .to_string()
                }
                Some(RegistrationOutcome::Failed(msg)) => {
                    format!("✗ 事件脚本    : 外部实例信号注册失败: {msg}")
                }
                None => {
                    "⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）".to_string()
                }
            },
        });
        lines
    }

    /// doctor 输出的异步版本：在渲染桥接行前触发一次 `/Scripting` 探测
    /// （懒探测缓存，成功后升级为确认态）。
    ///
    /// doctor 路径从不调用 [`Self::ensure_scripting_probe`]，
    /// 导致 D-Bus 桥接行恒为「未探测」——尽管 `org.kde.KWin` 的
    /// `/Scripting` 实际可达。同步版本 [`Self::doctor_lines`] 保留给
    /// 内部状态渲染；daemon doctor 走本方法补齐证据后渲染。
    pub async fn doctor_lines_async(&self) -> Vec<String> {
        if self.scripting_probe_ok() != Some(true) {
            let _ = self.ensure_scripting_probe().await;
        }
        self.doctor_lines()
    }

    /// 探测并缓存 `/Scripting` 可用性。
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

    // ───────────────────────── X11 EWMH 枚举 ─────────────────────────

    /// `_NET_CLIENT_LIST_STACKING`（缺失回退 `_NET_CLIENT_LIST`）。
    ///
    /// KWin 是 EWMH 合规 WM，两条列表都返回受管顶层窗口；`_STACKING` 额外给
    /// 层叠顺序（从底到顶），故优先。
    fn x11_stacking_list(
        &self,
        x11: &X11DisplayServer,
    ) -> agent_shell_core::error::Result<Vec<u32>> {
        let stacking = x11.get_client_list_stacking()?;
        Ok(if stacking.is_empty() {
            x11.get_client_list()?
        } else {
            stacking
        })
    }

    /// X11 会话的窗口列表。窗口 id 输出十进制字符串——与 Scripting
    /// `internalId.toString()` 及 `screenshot --window` 的十进制解析口径一致，
    /// 保证 `windows list` 结果可直接喂给窗口直捕。
    ///
    /// 单窗查询失败（BadWindow：枚举中途窗口销毁）跳过而非中止整条列表——
    /// 与 Wayland 分支 `filter_map` 口径一致；列表读取失败仍经 `?` 传播。
    fn x11_list_windows(
        &self,
        x11: &X11DisplayServer,
    ) -> agent_shell_core::error::Result<Vec<WindowInfo>> {
        let windows = self.x11_stacking_list(x11)?;
        // stacking order：从底到顶，索引越大越靠上。
        Ok(windows
            .iter()
            .enumerate()
            .filter_map(|(i, &w)| Self::x11_build_window_info(x11, w, i as u32).ok())
            .collect())
    }

    /// EWMH 单窗信息 → core `WindowInfo`（十进制 id + KDE 标签）。
    fn x11_build_window_info(
        x11: &X11DisplayServer,
        window: u32,
        stacking_order: u32,
    ) -> agent_shell_core::error::Result<WindowInfo> {
        let title = x11.get_window_name(window)?;
        let app_id = x11.get_wm_class(window)?.unwrap_or_else(|| title.clone());
        let pid = x11.get_window_pid(window)?.unwrap_or(0);
        let geometry = x11.get_window_geometry(window)?;
        let desktop = x11.get_window_desktop(window)?;
        let states_atoms = x11.get_window_states(window)?;
        let states = Self::x11_states_from_atoms(&states_atoms, x11.atoms());

        let workspace_id = desktop.and_then(|d| {
            if d == u32::MAX {
                None // 所有工作区可见
            } else {
                Some(WorkspaceId {
                    native_id: d.to_string(),
                    de_type: DesktopEnvironment::KDE,
                })
            }
        });

        Ok(WindowInfo {
            id: WindowId {
                native_id: format!("{window}"),
                de_type: DesktopEnvironment::KDE,
            },
            title,
            app_id,
            pid,
            geometry,
            // X11 GetGeometry 返回内容几何（坐标相对 frame 窗口，非根坐标）；
            // 含装饰的外框需 `_NET_FRAME_EXTENTS` + 坐标平移，本层不计算，
            // 置 default（全零）表示未知——不得把内容几何误报为外框。
            frame_geometry: Rect::default(),
            states,
            workspace_id,
            monitor_id: None,
            stacking_order,
            desktop_file: None, // EWMH 无 desktop file 属性
            window_type: Self::x11_window_type(x11, window),
            icon_geometry: None,
            keep_above: states_atoms.contains(&x11.atoms()._NET_WM_STATE_ABOVE),
        })
    }

    /// EWMH `_NET_WM_STATE` 原子列表 → [`WindowState`] 归一化（纯函数，可单测）。
    fn x11_states_from_atoms(atoms: &[u32], ewmh: &EwmhAtoms) -> Vec<WindowState> {
        let mut states = Vec::new();
        if atoms.contains(&ewmh._NET_WM_STATE_HIDDEN) {
            states.push(WindowState::Minimized);
        }
        if atoms.contains(&ewmh._NET_WM_STATE_MAXIMIZED_VERT)
            || atoms.contains(&ewmh._NET_WM_STATE_MAXIMIZED_HORZ)
        {
            states.push(WindowState::Maximized);
        }
        if atoms.contains(&ewmh._NET_WM_STATE_FULLSCREEN) {
            states.push(WindowState::FullScreen);
        }
        if states.is_empty() {
            states.push(WindowState::Normal);
        }
        states
    }

    /// `_NET_WM_WINDOW_TYPE` 属性读取 + [`Self::x11_window_type_from_atoms`] 归一化。
    ///
    /// 读取用 type=0（AnyPropertyType）——`_NET_WM_WINDOW_TYPE` 属性类型为
    /// ATOM，零值让 x11rb 不校验实际类型直接取回。读取失败（BadWindow：窗口
    /// 已销毁）回 `Unknown`，与 Scripting `parse_window_type` 对缺失/无法解析
    /// 的回退一致；缺失属性（空列表）仍归 `Normal`（EWMH：未声明类型的顶层
    /// 窗口视为普通窗口）。
    fn x11_window_type(x11: &X11DisplayServer, window: u32) -> WindowType {
        let atoms = x11.atoms();
        let list = match x11.get_property_u32(window, atoms._NET_WM_WINDOW_TYPE, 0) {
            Ok(list) => list,
            Err(_) => return WindowType::Unknown,
        };
        Self::x11_window_type_from_atoms(&list, atoms)
    }

    /// `_NET_WM_WINDOW_TYPE` 原子列表 → [`WindowType`] 归一化（纯函数，可单测）。
    ///
    /// 属性缺失（空列表）归 `Normal`（EWMH：未声明窗口类型的顶层窗口视为
    /// 普通窗口）；属性存在但类型未识别归 `Unknown`。Utility/Notification
    /// 映射与 Scripting `parse_window_type` 对齐，保证同一 KWin 会话 X11/Wayland
    /// 回报一致的 window_type。
    fn x11_window_type_from_atoms(list: &[u32], atoms: &EwmhAtoms) -> WindowType {
        for a in list {
            if *a == atoms._NET_WM_WINDOW_TYPE_NORMAL {
                return WindowType::Normal;
            }
            if *a == atoms._NET_WM_WINDOW_TYPE_DIALOG {
                return WindowType::Dialog;
            }
            if *a == atoms._NET_WM_WINDOW_TYPE_DOCK {
                return WindowType::Dock;
            }
            if *a == atoms._NET_WM_WINDOW_TYPE_DESKTOP {
                return WindowType::Desktop;
            }
            if *a == atoms._NET_WM_WINDOW_TYPE_MENU {
                return WindowType::DropdownMenu;
            }
            if *a == atoms._NET_WM_WINDOW_TYPE_TOOLTIP {
                return WindowType::Tooltip;
            }
            if *a == atoms._NET_WM_WINDOW_TYPE_SPLASH {
                return WindowType::Splash;
            }
            if *a == atoms._NET_WM_WINDOW_TYPE_UTILITY {
                return WindowType::Utility;
            }
            if *a == atoms._NET_WM_WINDOW_TYPE_NOTIFICATION {
                return WindowType::Notification;
            }
        }
        if list.is_empty() {
            WindowType::Normal
        } else {
            WindowType::Unknown
        }
    }

    // ───────────────────── X11 EWMH 窗口管理 / 工作区 ─────────────────────

    /// 窗口 ID 字符串 → x11rb Window（u32，十进制）。
    ///
    /// EWMH 枚举输出十进制窗口 id，与 Scripting
    /// `internalId.toString()` 口径一致；此处按十进制回读，保证
    /// `windows list` → `windows focus/move` 的 id 往返自洽。
    fn x11_window_id(id: &WindowId) -> agent_shell_core::error::Result<u32> {
        id.native_id.parse::<u32>().map_err(|_| {
            AgentShellError::WindowNotFound(format!("invalid window id: {}", id.native_id))
        })
    }

    /// `_NET_DESKTOP_NAMES` → 工作区名称列表（NULL 分隔；缺失回空，调用方
    /// 按索引合成 `Desktop N` 兜底名）。
    fn x11_desktop_names(x11: &X11DisplayServer) -> Vec<String> {
        let atoms = x11.atoms();
        x11.get_property_string(
            x11.root_window(),
            atoms._NET_DESKTOP_NAMES,
            atoms.UTF8_STRING,
        )
        .ok()
        .flatten()
        .map(|s| {
            s.split('\0')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
    }

    /// 由 EWMH 计数/当前索引/名称列表构造 [`WorkspaceInfo`]（纯函数，可单测）。
    ///
    /// `native_id` 取 0 基索引——与 `_NET_WM_DESKTOP`（`x11_build_window_info`
    /// 的 `workspace_id.native_id` 同源）及 `_NET_CURRENT_DESKTOP` 口径一致；
    /// `number` 为 1 基人类可读编号。
    fn x11_workspaces(count: u32, current: u32, names: &[String]) -> Vec<WorkspaceInfo> {
        (0..count)
            .map(|i| WorkspaceInfo {
                id: WorkspaceId {
                    native_id: i.to_string(),
                    de_type: DesktopEnvironment::KDE,
                },
                name: names
                    .get(i as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("Desktop {}", i + 1)),
                number: i + 1,
                is_active: i == current,
                monitor_ids: Vec::new(),
                window_ids: Vec::new(),
            })
            .collect()
    }

    /// X11 会话的工作区列表：`_NET_NUMBER_OF_DESKTOPS` + `_NET_DESKTOP_NAMES`
    /// + `_NET_CURRENT_DESKTOP`（EWMH），无需 Scripting。
    fn x11_list_workspaces(
        x11: &X11DisplayServer,
    ) -> agent_shell_core::error::Result<Vec<WorkspaceInfo>> {
        let count = x11.get_number_of_desktops()?;
        let current = x11.get_current_desktop().unwrap_or(0);
        let names = Self::x11_desktop_names(x11);
        Ok(Self::x11_workspaces(count, current, &names))
    }

    /// 确保长驻事件脚本在跑（幂等；首次 subscribe/subscribe_raw 时加载）。
    ///
    /// 订阅前刷新 /Scripting 探测：启动早期未就绪时由
    /// `spawn_event_script` 内部的重试探测兜底，这里只做缓存预热。
    async fn ensure_event_monitor(&self) -> Result<()> {
        let _ = self.ensure_scripting_probe().await;
        let mut handle = self.event_handle.lock().await;
        if handle.is_none() {
            // 幂等启动；句柄（含 StagedScript 暂存文件）原样保存在组件内
            // 直到 stop/drop——不得重建副本，否则暂存文件被提前 Drop 删除。
            // spawn_event_monitor 内部校验信号注册：成功才落句柄；失败记录
            // 诊断供 doctor 可见，句柄保持 None 使下次 subscribe 可重试。
            match crate::event_script::spawn_event_monitor(&self.bridge, self.version.is_v6()).await
            {
                Ok(h) => {
                    *handle = Some(h);
                    *self.event_registration.lock().expect("not poisoned") = Some(Ok(()));
                }
                Err(e) => {
                    *self.event_registration.lock().expect("not poisoned") =
                        Some(Err(e.to_string()));
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// 订阅原始事件流（§18.2）：确保事件脚本在跑，返回其 [`RawSource`]
    /// 适配器，交由 daemon 的 `EventNormalizer` 归一化后 fan-out + 入 ring。
    ///
    /// 与 [`CompositorComponent::subscribe`] 共享同一底层队列，两者仅能成功
    /// 一次——daemon 侧装配归一化管线用本方法取原始事件源。
    pub async fn subscribe_raw(
        &self,
    ) -> agent_shell_core::error::Result<Box<dyn event::RawSource>> {
        // X11 会话走 EWMH 事件源（PropertyNotify 差分），不依赖 /Scripting；
        // Wayland 会话走长驻事件脚本。
        if let Some(x11) = self.x11.as_ref() {
            return self.subscribe_ewmh_raw(x11).await;
        }
        // 先确保事件脚本在跑（幂等，内部自带 /Scripting 重试探测），成功后再
        // 取一次性接收端——若先取流后启动脚本，脚本启动失败（5s 探测窗口内
        // /Scripting 未就绪、或 D-Bus 调用失败）时接收端随栈销毁，一次性事件
        // 队列永久丢失，此后订阅只会命中「already subscribed」无法恢复。
        self.ensure_event_monitor().await?;
        let rx = self.bridge.take_raw_event_rx().await.ok_or_else(|| {
            AgentShellError::BackendUnavailable("kwin event stream already subscribed".to_string())
        })?;
        let kind = match self.session_kind() {
            SessionKind::X11 => event::EventSource::KWinX11,
            SessionKind::Wayland => event::EventSource::KWinWayland,
        };
        Ok(Box::new(crate::event_source::KWinRawSource::new(rx, kind)))
    }

    /// X11 会话原始事件源：懒启动 EWMH 监视器（幂等），取一次性接收端
    /// 包装为 [`RawSource`]（与 Scripting `take_raw_event_rx` 同款一次性语义）。
    async fn subscribe_ewmh_raw(
        &self,
        x11: &Arc<X11DisplayServer>,
    ) -> agent_shell_core::error::Result<Box<dyn event::RawSource>> {
        let rx = self.ewmh_take_rx(x11).await?.ok_or_else(|| {
            AgentShellError::BackendUnavailable(
                "kwin ewmh event stream already subscribed".to_string(),
            )
        })?;
        Ok(Box::new(crate::event_ewmh::EwmhRawSource::new(rx)))
    }

    /// 懒启动 EWMH 监视器并取一次性接收端（`subscribe`/`subscribe_raw` 共享）。
    ///
    /// 监视器幂等启动；接收端仅其一可取走——近似映射流（`EwmhEventStream`）
    /// 与归一化流（`EwmhRawSource`）互斥，与 Scripting bridge 的一次性队列
    /// 语义一致。返回 `None` 表示已订阅过。
    async fn ewmh_take_rx(
        &self,
        x11: &Arc<X11DisplayServer>,
    ) -> agent_shell_core::error::Result<
        Option<tokio::sync::mpsc::UnboundedReceiver<event::RawEvent>>,
    > {
        let mut slot = self.ewmh_monitor.lock().await;
        if slot.is_none() {
            *slot = Some(crate::event_ewmh::spawn_ewmh_monitor(x11)?);
        }
        Ok(slot.as_mut().and_then(|m| m.take_rx()))
    }

    /// X11 会话近似事件流：懒启动 EWMH 监视器，取一次性接收端包装为
    /// core [`EventStream`]（与 Scripting `take_event_stream` 同款一次性语义）。
    async fn subscribe_ewmh(
        &self,
        x11: &Arc<X11DisplayServer>,
    ) -> agent_shell_core::error::Result<Box<dyn EventStream>> {
        let rx = self.ewmh_take_rx(x11).await?.ok_or_else(|| {
            AgentShellError::BackendUnavailable(
                "kwin ewmh event stream already subscribed".to_string(),
            )
        })?;
        Ok(Box::new(crate::event_ewmh::EwmhEventStream::new(rx)))
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

    /// 窗口列表：X11 会话走 EWMH（`_NET_CLIENT_LIST_STACKING`），Wayland
    /// 会话走 list_windows.js（一次 callDBus 批量取全量详情）。
    ///
    /// X11 分支优先于 Scripting——部分 KWin 5.x X11 会话不注册 `/Scripting`
    /// 且 EWMH 无需事件聚合即可给出完整 `WindowInfo`。
    /// Wayland 下协议 stacking-order 仅提供 uuid 列表，逐窗 get_window_by_uuid
    /// 仍需事件聚合才能取属性（本层 inert 不消费事件）——T3b 前纯协议路径
    /// 无法给出 WindowInfo，故仍走 Scripting。window_mgmt 短绑状态只影响
    /// focus/minimize/close 走协议还是 Scripting。
    async fn list_windows(&self) -> agent_shell_core::error::Result<Vec<WindowInfo>> {
        if let Some(x11) = self.x11.as_ref() {
            return self.x11_list_windows(x11);
        }
        let v = self.query(ScriptTemplate::ListWindows, &[]).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .enumerate()
            .filter_map(|(i, w)| Self::parse_window(w, i as u32))
            .collect())
    }

    /// 当前活动窗口（可能为空——桌面无焦点）。X11 会话走 `_NET_ACTIVE_WINDOW`，
    /// Wayland 会话走 get_active_window.js。
    async fn get_active_window(&self) -> agent_shell_core::error::Result<Option<WindowInfo>> {
        if let Some(x11) = self.x11.as_ref() {
            let Some(w) = x11.get_active_window()? else {
                return Ok(None);
            };
            // 活动窗口的 stacking_order 从 `_NET_CLIENT_LIST_STACKING` 查索引，
            // 保持「越大越靠上」契约（与 list_windows 索引口径一致）；不在表内
            // （枚举竞态）置 0。
            let order = self
                .x11_stacking_list(x11)?
                .iter()
                .position(|&x| x == w)
                .unwrap_or(0) as u32;
            return Ok(Some(Self::x11_build_window_info(x11, w, order)?));
        }
        let v = self.query(ScriptTemplate::GetActiveWindow, &[]).await?;
        Ok(if v.is_null() {
            None
        } else {
            Self::parse_window(&v, 0)
        })
    }

    /// 聚焦：X11 会话走 EWMH `_NET_ACTIVE_WINDOW`（ClientMessage 写入 root +
    /// 回读确认——EWMH 无回执，WM 忽略请求时 send 仍成功）；Wayland 会话协议
    /// activate 优先（请求发出即成功——wayland 请求无回执），未短绑时回退
    /// focus_window.js。
    async fn focus_window(&self, id: &WindowId) -> agent_shell_core::error::Result<()> {
        if let Some(x11) = self.x11.as_ref() {
            let window = Self::x11_window_id(id)?;
            // 确认轮询含同步 sleep（最长 500ms），走 spawn_blocking 避免阻塞
            // tokio worker（§19 审查项：同步段不得占执行器线程）。
            let x11 = Arc::clone(x11);
            return tokio::task::spawn_blocking(move || x11.activate_window_confirmed(window))
                .await
                .map_err(|e| {
                    AgentShellError::Other(format!("kwin focus blocking task join: {e}").into())
                })?;
        }
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

    /// 移动：X11 会话走 EWMH `_NET_MOVERESIZE_WINDOW`（只设位置，尺寸字段
    /// 标志位为 0）；Wayland 会话协议无 set_geometry（§7.2），始终 Scripting。
    async fn move_window(
        &self,
        id: &WindowId,
        x: i32,
        y: i32,
    ) -> agent_shell_core::error::Result<()> {
        if let Some(x11) = self.x11.as_ref() {
            let window = Self::x11_window_id(id)?;
            return x11.move_resize_window(window, Some(x), Some(y), None, None);
        }
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

    /// 工作区列表：X11 会话走 EWMH（`_NET_NUMBER_OF_DESKTOPS` +
    /// `_NET_DESKTOP_NAMES` + `_NET_CURRENT_DESKTOP`），Wayland 会话走
    /// list_workspaces.js。
    async fn list_workspaces(&self) -> agent_shell_core::error::Result<Vec<WorkspaceInfo>> {
        if let Some(x11) = self.x11.as_ref() {
            return Self::x11_list_workspaces(x11);
        }
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
    /// 返回的是**近似映射流**（`KWinEventStream`）：每条为 event_monitor.js
    /// 的 sendResult JSON（`{"event": "windowOpened", "id": ...}`）按最小集
    /// 近似映射为 core `DesktopEvent`。daemon 侧归一化管线走
    /// [`subscribe_raw`](Self::subscribe_raw) 取原始事件源；本方法保留给
    /// 直接消费近似流的调用方（如 DDE deepin-kwin 委托）。
    async fn subscribe(&self) -> agent_shell_core::error::Result<Box<dyn EventStream>> {
        // X11 会话走 EWMH 近似映射流（`EwmhEventStream`），不依赖 /Scripting；
        // Wayland 会话走长驻事件脚本。
        if let Some(x11) = self.x11.as_ref() {
            return self.subscribe_ewmh(x11).await;
        }
        // 与 `subscribe_raw` 同款顺序：先确保事件脚本在跑，成功后再取一次性
        // 接收端，避免脚本启动失败时接收端随栈销毁、事件队列永久丢失。
        self.ensure_event_monitor().await?;
        let stream = self.bridge.take_event_stream().await.ok_or_else(|| {
            AgentShellError::BackendUnavailable("kwin event stream already subscribed".to_string())
        })?;
        Ok(Box::new(stream))
    }
}

/// 测试与诊断：通道组合摘要 + `/Scripting` 探测三态迁移。
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
            // `kill()` 发 SIGKILL，跳过 dbus-daemon 正常退出路径，其 /tmp/dbus-*
            // socket 不 unlink、累积 stale 文件；SIGTERM 让其自行清理。
            // SAFETY: `_child.id()` 是存活的子进程 PID，发 SIGTERM 无内存安全风险。
            unsafe { libc::kill(self._child.id() as i32, libc::SIGTERM) };
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

    /// 手工构造的 EWMH 原子集（固定值，供纯函数单测；`atom_manager!` 无 Default）。
    fn test_ewmh_atoms() -> EwmhAtoms {
        EwmhAtoms {
            _NET_CLIENT_LIST: 1,
            _NET_CLIENT_LIST_STACKING: 2,
            _NET_ACTIVE_WINDOW: 3,
            _NET_CURRENT_DESKTOP: 4,
            _NET_NUMBER_OF_DESKTOPS: 5,
            _NET_DESKTOP_NAMES: 6,
            _NET_DESKTOP_GEOMETRY: 7,
            _NET_DESKTOP_VIEWPORT: 8,
            _NET_WORKAREA: 9,
            _NET_SUPPORTED: 10,
            _NET_SUPPORTING_WM_CHECK: 11,
            _NET_CLOSE_WINDOW: 12,
            _NET_MOVERESIZE_WINDOW: 13,
            _NET_WM_NAME: 14,
            _NET_WM_STATE: 15,
            _NET_WM_STATE_MAXIMIZED_VERT: 16,
            _NET_WM_STATE_MAXIMIZED_HORZ: 17,
            _NET_WM_STATE_HIDDEN: 18,
            _NET_WM_STATE_FULLSCREEN: 19,
            _NET_WM_STATE_ABOVE: 20,
            _NET_WM_DESKTOP: 21,
            _NET_WM_WINDOW_TYPE: 22,
            _NET_WM_WINDOW_TYPE_NORMAL: 23,
            _NET_WM_WINDOW_TYPE_DIALOG: 24,
            _NET_WM_WINDOW_TYPE_DOCK: 25,
            _NET_WM_WINDOW_TYPE_DESKTOP: 26,
            _NET_WM_WINDOW_TYPE_MENU: 27,
            _NET_WM_WINDOW_TYPE_TOOLTIP: 28,
            _NET_WM_WINDOW_TYPE_SPLASH: 29,
            _NET_WM_WINDOW_TYPE_UTILITY: 30,
            _NET_WM_WINDOW_TYPE_NOTIFICATION: 31,
            _NET_WM_PID: 32,
            UTF8_STRING: 33,
            WM_CLASS: 100,
        }
    }

    #[test]
    fn x11_states_from_atoms_maps_ewmh_states() {
        let atoms = test_ewmh_atoms();
        assert_eq!(
            KWinCompositor::x11_states_from_atoms(&[18], &atoms),
            vec![WindowState::Minimized]
        );
        assert_eq!(
            KWinCompositor::x11_states_from_atoms(&[17], &atoms),
            vec![WindowState::Maximized]
        );
        assert_eq!(
            KWinCompositor::x11_states_from_atoms(&[19], &atoms),
            vec![WindowState::FullScreen]
        );
        // 空状态 → Normal（无 _NET_WM_STATE 的普通窗口）。
        assert_eq!(
            KWinCompositor::x11_states_from_atoms(&[], &atoms),
            vec![WindowState::Normal]
        );
        // 多状态共存：minimized + maximized 两个都保留。
        let multi = KWinCompositor::x11_states_from_atoms(&[18, 16], &atoms);
        assert!(multi.contains(&WindowState::Minimized));
        assert!(multi.contains(&WindowState::Maximized));
    }

    #[test]
    fn x11_window_type_from_atoms_maps_known_and_unknown() {
        use WindowType::*;
        let atoms = test_ewmh_atoms();
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[23], &atoms),
            Normal
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[24], &atoms),
            Dialog
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[25], &atoms),
            Dock
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[26], &atoms),
            Desktop
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[27], &atoms),
            DropdownMenu
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[28], &atoms),
            Tooltip
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[29], &atoms),
            Splash
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[30], &atoms),
            Utility
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[31], &atoms),
            Notification
        );
        // 属性缺失（空列表）→ Normal；属性存在但类型未识别 → Unknown。
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[], &atoms),
            Normal
        );
        assert_eq!(
            KWinCompositor::x11_window_type_from_atoms(&[999], &atoms),
            Unknown
        );
    }

    #[test]
    fn x11_window_id_parses_decimal_and_rejects_non_numeric() {
        let win = |native_id: &str| WindowId {
            native_id: native_id.to_string(),
            de_type: DesktopEnvironment::KDE,
        };
        // 十进制往返：EWMH 枚举输出十进制 id，focus/move 按十进制回读。
        assert_eq!(KWinCompositor::x11_window_id(&win("1234")).unwrap(), 1234);
        assert_eq!(KWinCompositor::x11_window_id(&win("0")).unwrap(), 0);
        // 非十进制（hex 前缀 / 空 / 负数）→ WindowNotFound。
        assert!(matches!(
            KWinCompositor::x11_window_id(&win("0x1a0")),
            Err(AgentShellError::WindowNotFound(_))
        ));
        assert!(matches!(
            KWinCompositor::x11_window_id(&win("")),
            Err(AgentShellError::WindowNotFound(_))
        ));
    }

    #[test]
    fn x11_workspaces_builds_indexed_workspaces() {
        // _NET_DESKTOP_NAMES 仅提供 2 个名称，第 3 个工作区靠索引合成兜底名。
        let names = vec!["Web".to_string(), "Code".to_string()];
        let ws = KWinCompositor::x11_workspaces(3, 1, &names);
        assert_eq!(ws.len(), 3);
        // native_id 取 0 基索引（与 _NET_WM_DESKTOP/_NET_CURRENT_DESKTOP 同源）。
        assert_eq!(ws[0].id.native_id, "0");
        assert_eq!(ws[0].number, 1);
        assert_eq!(ws[0].name, "Web");
        assert!(!ws[0].is_active);
        assert_eq!(ws[1].id.native_id, "1");
        assert_eq!(ws[1].number, 2);
        assert_eq!(ws[1].name, "Code");
        assert!(ws[1].is_active);
        // 名称缺失（索引越界）→ 合成 "Desktop N" 兜底。
        assert_eq!(ws[2].id.native_id, "2");
        assert_eq!(ws[2].number, 3);
        assert_eq!(ws[2].name, "Desktop 3");
        assert!(!ws[2].is_active);
    }

    #[test]
    fn x11_workspaces_missing_names_fall_back_to_synthetic() {
        let ws = KWinCompositor::x11_workspaces(2, 0, &[]);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].name, "Desktop 1");
        assert_eq!(ws[1].name, "Desktop 2");
        assert!(ws[0].is_active);
    }

    /// 懒启动能力声明：事件脚本首次 `subscribe()` 才 load，`capabilities()`
    /// 的 `window_events`/`workspace_events` false 非永久不可用。
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

    /// `Some(false)`（PROBE_FAIL）必须重试，不能把
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

    /// doctor 事件脚本行如实标注为「未加载（懒启动）」，不再以「T3b 待办」
    /// 呈现（T3b 归一化管线已装配）。
    #[tokio::test]
    async fn doctor_event_script_line_marks_unloaded() {
        let bus = TestBus::start().await;
        let comp = KWinCompositor::for_test(bridge(&bus).await, None);
        let lines = comp.doctor_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("⚠ 事件脚本") && l.contains("未加载")),
            "event script line must mark unloaded: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("T3b")),
            "event script line must not mention T3b: {lines:#?}"
        );
    }

    /// 无等待者记录的 `__ready__`（残留/外部实例）须让 doctor 区分
    /// 「加载后零注册」与「未加载」——本组件未加载但已检测到外部注册。
    #[tokio::test]
    async fn doctor_event_script_line_reports_external_instance() {
        let bus = TestBus::start().await;
        let b = bridge(&bus).await;
        b.record_late_registration(RegistrationOutcome::Ready);
        let comp = KWinCompositor::for_test(b, None);
        let lines = comp.doctor_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("⚠ 事件脚本") && l.contains("外部实例已注册")),
            "doctor must report external instance registration: {lines:#?}"
        );
    }

    /// 无等待者记录的 `__error__` 须在 doctor 呈现失败诊断，而非静默丢弃
    /// 后显示「未加载」。
    #[tokio::test]
    async fn doctor_event_script_line_reports_external_failure() {
        let bus = TestBus::start().await;
        let b = bridge(&bus).await;
        b.record_late_registration(RegistrationOutcome::Failed("boom".to_string()));
        let comp = KWinCompositor::for_test(b, None);
        let lines = comp.doctor_lines();
        assert!(
            lines.iter().any(|l| {
                l.contains("✗ 事件脚本") && l.contains("外部实例信号注册失败: boom")
            }),
            "doctor must report external instance failure: {lines:#?}"
        );
    }

    /// 新探测（prepare_registration）开始须清除滞留的「外部实例」标记，
    /// doctor 恢复到「未加载」——残留实例停止/新一轮注册后不再永久误报。
    #[tokio::test]
    async fn doctor_event_script_line_restores_unloaded_after_new_probe() {
        let bus = TestBus::start().await;
        let b = bridge(&bus).await;
        b.record_late_registration(RegistrationOutcome::Ready);
        let comp = KWinCompositor::for_test(b, None);
        assert!(
            comp.doctor_lines()
                .iter()
                .any(|l| l.contains("外部实例已注册")),
            "external instance must be reported before a new probe"
        );

        drop(comp.bridge.prepare_registration().await);
        let lines = comp.doctor_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("⚠ 事件脚本") && l.contains("未加载")),
            "doctor must restore unloaded after a new probe: {lines:#?}"
        );
    }
}
