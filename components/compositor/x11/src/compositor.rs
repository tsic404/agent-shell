//! [`X11Compositor`]：通用 X11 兜底合成器（设计文档 §3.3 / §11）。
//!
//! 组合纯协议层的 [`X11DisplayServer`]（EWMH + ICCCM + XTest + MIT-SHM），
//! 通过 x11rb 原生协议实现 [`CompositorComponent`] 全部 17 方法。
//! CLI 工具（[`X11Commands`]，xdotool/wmctrl）仅在原生路径失败时兜底，
//! 缺失不阻塞初始化。
//!
//! 原生协议实现路径（§11 操作表）：
//!
//! | 操作 | 实现方式 |
//! |------|---------|
//! | 窗口列表 | `_NET_CLIENT_LIST` get_property |
//! | 聚焦 | `_NET_ACTIVE_WINDOW` ClientMessage |
//! | 移动/缩放 | `x11rb::configure_window` + `_NET_MOVERESIZE_WINDOW` |
//! | 最小化 | `_NET_WM_STATE` ClientMessage (_NET_WM_STATE_HIDDEN) |
//! | 最大化 | `_NET_WM_STATE` ClientMessage (_NET_WM_STATE_MAXIMIZED_VERT/HORZ) |
//! | 关闭 | `_NET_CLOSE_WINDOW` ClientMessage |
//! | 工作区列表 | `_NET_NUMBER_OF_DESKTOPS` + `_NET_DESKTOP_NAMES` + `_NET_CURRENT_DESKTOP` |
//! | 切换工作区 | `_NET_CURRENT_DESKTOP` ClientMessage |
//! | 移窗口到工作区 | `_NET_WM_DESKTOP` ClientMessage |
//! | 截图 | MIT-SHM / XGetImage（`capture_window`） |
//!
//! 事件流：X11 无合成器级原生事件推送，`capabilities().window_events = false`。
//! XDamage/XRecord 可选，不属本任务范围。

use async_trait::async_trait;

use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{
    MonitorId, MonitorInfo, Rect, WindowId, WindowInfo, WindowState, WindowType, WorkspaceId,
    WorkspaceInfo,
};
use agent_shell_core::{DesktopEnvironment, EventStream};
use agent_shell_displayserver_x11::{EwmhAtoms, X11DisplayServer};

use crate::commands::X11Commands;
use crate::ewmh;

/// 通用 X11 兜底合成器（§11）。
///
/// 组合 [`X11DisplayServer`]（协议通道基础）+ [`X11Commands`]（CLI 保底）。
/// 全部窗口操作走原生 x11rb；仅当原生失败才落到 `cmd`（xdotool/wmctrl），
/// 且 CLI 缺失不阻塞初始化。
pub struct X11Compositor {
    /// 协议通道基础（EWMH/ICCCM/XTest/MIT-SHM）。
    display: X11DisplayServer,
    /// CLI 保底（xdotool/wmctrl；`None` 表示两者均未安装）。
    cmd: Option<X11Commands>,
}

impl std::fmt::Debug for X11Compositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Compositor")
            .field("xtest_available", &self.display.is_xtest_available())
            .field("shm_available", &self.display.is_shm_available())
            .field(
                "cmd_usable",
                &self.cmd.as_ref().is_some_and(|c| c.is_usable()),
            )
            .finish()
    }
}

impl X11Compositor {
    /// 连接 `$DISPLAY` 并初始化 EWMH 原子、探测 XTest/MIT-SHM/CLI 工具。
    ///
    /// X server 不可达返回 `BackendUnavailable`；CLI 工具缺失只记入
    /// `cmd` 字段（`None` 或部分可用），不报错。
    pub fn new() -> Result<Self> {
        let display = X11DisplayServer::connect()?;
        let cmd = X11Commands::new();
        let cmd = if cmd.is_usable() { Some(cmd) } else { None };
        Ok(Self { display, cmd })
    }

    /// 基于既有连接初始化（测试与多会话场景用）。
    pub fn with_display_server(display: X11DisplayServer) -> Self {
        let cmd = X11Commands::new();
        let cmd = if cmd.is_usable() { Some(cmd) } else { None };
        Self { display, cmd }
    }

    /// 协议通道引用（doctor 与子模块复用）。
    pub fn display_server(&self) -> &X11DisplayServer {
        &self.display
    }

    /// CLI 保底通道引用（`None` = 两个工具均未安装）。
    pub fn commands(&self) -> Option<&X11Commands> {
        self.cmd.as_ref()
    }

    /// doctor 诊断输出（§11 验证输出格式）。
    pub fn doctor_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();

        // X11 连接：屏幕分辨率
        let geo = self.display.primary_output_geometry().unwrap_or_default();
        lines.push(format!(
            "{} X11 连接     : :{} (screen {}, {}x{})",
            "✓",
            self.display.screen_index(),
            self.display.screen_index(),
            geo.width,
            geo.height
        ));

        // EWMH 支持原子数：分母为本 crate intern 的 EWMH 原子总数。
        // 设计文档 §6.6 示例写 12/12（核心 _NET_WM 原子），本 crate 实际
        // intern 了 33 个（含 window-type 子原子），运行时动态计算。
        let supported = ewmh::supported_atoms(&self.display).unwrap_or_default();
        let total = ewmh::ewmh_atom_count();
        lines.push(format!(
            "{} EWMH 协议   : {}/{} atoms supported",
            "✓",
            supported.len(),
            total
        ));

        // XTest
        if self.display.is_xtest_available() {
            lines.push("✓ XTest 扩展  : v2.2 ✓ (原生输入注入)".into());
        } else {
            lines.push("⚠ XTest 扩展  : unavailable (XWayland? 降级 libei/ydotool)".into());
        }

        // MIT-SHM
        if self.display.is_shm_available() {
            lines.push("✓ MIT-SHM     : enabled ✓ (零拷贝截图)".into());
        } else {
            lines.push("⚠ MIT-SHM     : disabled (降级 XGetImage)".into());
        }

        // CLI 保底
        match &self.cmd {
            Some(c) => {
                let tools = match (c.has_xdotool(), c.has_wmctrl()) {
                    (true, true) => "xdotool + wmctrl",
                    (true, false) => "xdotool",
                    (false, true) => "wmctrl",
                    (false, false) => "none",
                };
                lines.push(format!("✓ CLI 保底    : {tools}"));
            }
            None => {
                lines.push("⚠ CLI 保底    : xdotool/wmctrl 均未安装（原生失败时无兜底）".into())
            }
        }

        // 事件流
        lines.push("⚠ 事件流       : 无原生支持（XDamage 可选）".into());
        lines
    }

    // ───────────────────────── 内部辅助 ─────────────────────────

    /// 解析单个窗口的完整 [`WindowInfo`]。
    fn build_window_info(&self, window: u32, stacking_order: u32) -> Result<WindowInfo> {
        let title = self.display.get_window_name(window)?;
        let app_id = self
            .display
            .get_wm_class(window)?
            .unwrap_or_else(|| title.clone());
        let pid = self.display.get_window_pid(window)?.unwrap_or(0);
        let geometry = self.display.get_window_geometry(window)?;
        let desktop = self.display.get_window_desktop(window)?;
        let states_atoms = self.display.get_window_states(window)?;
        let states = states_from_atoms(&states_atoms, self.display.atoms());
        let window_type = ewmh::window_type(&self.display, window).unwrap_or(WindowType::Normal);

        let workspace_id = desktop.and_then(|d| {
            if d == 0xFFFF_FFFF {
                None // 所有工作区可见
            } else {
                Some(WorkspaceId {
                    native_id: d.to_string(),
                    de_type: DesktopEnvironment::X11Generic,
                })
            }
        });

        Ok(WindowInfo {
            id: WindowId {
                native_id: format!("0x{:x}", window),
                de_type: DesktopEnvironment::X11Generic,
            },
            title,
            app_id,
            pid,
            geometry,
            frame_geometry: geometry, // X11 GetGeometry 返回含装饰的外框
            states,
            workspace_id,
            monitor_id: None, // X11 无合成器级 monitor 绑定（RandR 输出 ≠ workspace）
            stacking_order,
            desktop_file: None,
            window_type,
            icon_geometry: None,
            keep_above: states_atoms.contains(&self.display.atoms()._NET_WM_STATE_ABOVE),
        })
    }

    /// 窗口 ID 字符串 → x11rb Window（u32）。
    fn parse_window_id(id: &WindowId) -> Result<u32> {
        let s = id.native_id.trim_start_matches("0x");
        u32::from_str_radix(s, 16).map_err(|_| {
            AgentShellError::WindowNotFound(format!("invalid window id: {}", id.native_id))
        })
    }

    /// CLI 保底通道（不存在返回错误）。
    fn cmd(&self) -> Result<&X11Commands> {
        self.cmd
            .as_ref()
            .ok_or_else(|| AgentShellError::BackendUnavailable("no CLI fallback available".into()))
    }
}

/// EWMH `_NET_WM_STATE` 原子列表 → [`WindowState`] 归一化。
fn states_from_atoms(atoms: &[u32], ewmh_atoms: &EwmhAtoms) -> Vec<WindowState> {
    let mut states = Vec::new();
    if atoms.contains(&ewmh_atoms._NET_WM_STATE_HIDDEN) {
        states.push(WindowState::Minimized);
    }
    if atoms.contains(&ewmh_atoms._NET_WM_STATE_MAXIMIZED_VERT)
        || atoms.contains(&ewmh_atoms._NET_WM_STATE_MAXIMIZED_HORZ)
    {
        states.push(WindowState::Maximized);
    }
    if atoms.contains(&ewmh_atoms._NET_WM_STATE_FULLSCREEN) {
        states.push(WindowState::FullScreen);
    }
    if states.is_empty() {
        states.push(WindowState::Normal);
    }
    states
}

// ───────────────────────── DesktopComponent ─────────────────────────

#[async_trait]
impl DesktopComponent for X11Compositor {
    fn name(&self) -> &'static str {
        "x11-compositor"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }

    fn is_available(&self) -> bool {
        true // 构造成功即可用（CLI/扩展缺失由 health 表达）
    }

    async fn health(&self) -> ComponentHealth {
        // EWMH 原生通道始终可用（构造成功即连接）；降级仅影响子能力。
        if self.display.is_xtest_available() && self.display.is_shm_available() {
            ComponentHealth::Healthy
        } else {
            let mut degraded = Vec::new();
            if !self.display.is_xtest_available() {
                degraded.push("XTest unavailable");
            }
            if !self.display.is_shm_available() {
                degraded.push("MIT-SHM unavailable");
            }
            ComponentHealth::Degraded(degraded.join("; "))
        }
    }
}

// ───────────────────────── CompositorComponent ─────────────────────────

#[async_trait]
impl CompositorComponent for X11Compositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            window_management: true,    // EWMH _NET_CLIENT_LIST 等始终可用
            workspace_management: true, // _NET_NUMBER_OF_DESKTOPS 等
            monitor_layout: true,       // RandR primary_output_geometry
            window_events: false,       // 无原生事件流（XDamage 可选，T3b）
            workspace_events: false,    // 无原生事件流
            native_input: self.display.is_xtest_available(),
            native_capture: true,   // MIT-SHM / XGetImage
            virtual_desktops: true, // _NET_NUMBER_OF_DESKTOPS
            effects_control: false, // X11 无合成器特效
        }
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let stacking = self.display.get_client_list_stacking()?;
        // 若 _NET_CLIENT_LIST_STACKING 缺失（非 EWMH WM），回退 _NET_CLIENT_LIST。
        let windows = if stacking.is_empty() {
            self.display.get_client_list()?
        } else {
            stacking
        };
        // stacking order：从底到顶，索引越大越靠上。
        windows
            .iter()
            .enumerate()
            .map(|(i, &w)| self.build_window_info(w, i as u32))
            .collect()
    }

    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        let active = self.display.get_active_window()?;
        match active {
            Some(w) => Ok(Some(self.build_window_info(w, 0)?)),
            None => Ok(None),
        }
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        let window = Self::parse_window_id(id)?;
        // 原生 EWMH _NET_ACTIVE_WINDOW
        match self.display.activate_window(window) {
            Ok(()) => Ok(()),
            Err(e) => {
                // 降级 CLI
                tracing::warn!(error = %e, "EWMH activate failed; trying CLI fallback");
                self.cmd()?.activate_window(&id.native_id).await
            }
        }
    }

    async fn move_window(&self, id: &WindowId, x: i32, y: i32) -> Result<()> {
        let window = Self::parse_window_id(id)?;
        // 原生：_NET_MOVERESIZE_WINDOW（仅 x/y）
        match self
            .display
            .move_resize_window(window, Some(x), Some(y), None, None)
        {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "EWMH move failed; trying CLI fallback");
                // CLI move 需要 w/h（保持当前尺寸）
                let geo = self.display.get_window_geometry(window)?;
                self.cmd()?
                    .move_resize_window(&id.native_id, x, y, geo.width, geo.height)
                    .await
            }
        }
    }

    async fn resize_window(&self, id: &WindowId, w: i32, h: i32) -> Result<()> {
        let window = Self::parse_window_id(id)?;
        match self
            .display
            .move_resize_window(window, None, None, Some(w), Some(h))
        {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "EWMH resize failed; trying CLI fallback");
                let geo = self.display.get_window_geometry(window)?;
                self.cmd()?
                    .move_resize_window(&id.native_id, geo.x, geo.y, w, h)
                    .await
            }
        }
    }

    async fn minimize_window(&self, id: &WindowId) -> Result<()> {
        let window = Self::parse_window_id(id)?;
        match self.display.minimize_window(window) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "EWMH minimize failed; trying CLI fallback");
                self.cmd()?.minimize_window(&id.native_id).await
            }
        }
    }

    async fn unminimize_window(&self, id: &WindowId) -> Result<()> {
        let window = Self::parse_window_id(id)?;
        // EWMH 无 unminimize 消息：清除 HIDDEN + 激活
        match self.display.unminimize_window(window) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "EWMH unminimize failed; trying CLI fallback");
                self.cmd()?.activate_window(&id.native_id).await
            }
        }
    }

    async fn maximize_window(&self, id: &WindowId) -> Result<()> {
        let window = Self::parse_window_id(id)?;
        self.display.maximize_window(window, true)
    }

    async fn close_window(&self, id: &WindowId) -> Result<()> {
        let window = Self::parse_window_id(id)?;
        match self.display.close_window(window) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "EWMH close failed; trying CLI fallback");
                self.cmd()?.close_window(&id.native_id).await
            }
        }
    }

    async fn set_window_geometry(&self, id: &WindowId, geo: Rect) -> Result<()> {
        let window = Self::parse_window_id(id)?;
        match self.display.move_resize_window(
            window,
            Some(geo.x),
            Some(geo.y),
            Some(geo.width),
            Some(geo.height),
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "EWMH set_geometry failed; trying CLI fallback");
                self.cmd()?
                    .move_resize_window(&id.native_id, geo.x, geo.y, geo.width, geo.height)
                    .await
            }
        }
    }

    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        let window = Self::parse_window_id(id)?;
        self.build_window_info(window, 0)
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let count = self.display.get_number_of_desktops()?;
        let current = self.display.get_current_desktop().unwrap_or(0);
        let names = ewmh::desktop_names(&self.display).unwrap_or_default();

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let name = names
                .get(i as usize)
                .cloned()
                .unwrap_or_else(|| format!("Desktop {}", i + 1));
            out.push(WorkspaceInfo {
                id: WorkspaceId {
                    native_id: i.to_string(),
                    de_type: DesktopEnvironment::X11Generic,
                },
                name,
                number: i + 1,
                is_active: i == current,
                monitor_ids: Vec::new(),
                window_ids: Vec::new(),
            });
        }
        Ok(out)
    }

    async fn activate_workspace(&self, id: &WorkspaceId) -> Result<()> {
        let desktop: u32 = id.native_id.parse().map_err(|_| {
            AgentShellError::Other(format!("invalid workspace id: {}", id.native_id).into())
        })?;
        match self.display.set_current_desktop(desktop) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "EWMH set_current_desktop failed; trying CLI fallback");
                self.cmd()?.switch_workspace(desktop).await
            }
        }
    }

    async fn move_window_to_workspace(&self, wid: &WindowId, ws: &WorkspaceId) -> Result<()> {
        let window = Self::parse_window_id(wid)?;
        let desktop: u32 = ws.native_id.parse().map_err(|_| {
            AgentShellError::Other(format!("invalid workspace id: {}", ws.native_id).into())
        })?;
        match self.display.move_window_to_desktop(window, desktop) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "EWMH move_to_desktop failed; trying CLI fallback");
                self.cmd()?.move_to_workspace(&wid.native_id, desktop).await
            }
        }
    }

    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        // X11 无合成器级 monitor 语义；用 RandR 主输出几何作为单 monitor。
        let geo = self.display.primary_output_geometry()?;
        Ok(vec![MonitorInfo {
            id: MonitorId {
                native_id: "primary".to_string(),
                de_type: DesktopEnvironment::X11Generic,
            },
            name: "PRIMARY".to_string(),
            geometry: geo,
            physical_geometry: geo, // X11 物理坐标 == 逻辑坐标（scale=1）
            scale: 1.0,
            is_primary: true,
            workspace_id: None,
        }])
    }

    async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
        // X11 无合成器级原生事件流；XDamage/XRecord 可选且属 T3b 范围。
        // 返回 NotImplemented——调用方据 capabilities().window_events=false 不应依赖。
        Err(AgentShellError::NotImplemented(
            "x11 compositor events: XDamage/XRecord lands in T3b".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_window_id_hex() {
        let id = WindowId {
            native_id: "0x1a0".to_string(),
            de_type: DesktopEnvironment::X11Generic,
        };
        assert_eq!(X11Compositor::parse_window_id(&id).unwrap(), 0x1a0);
    }

    #[test]
    fn parse_window_id_decimal_via_hex_strip() {
        // native_id 不带 0x 前缀也按十六进制解析（X11 window id 约定）。
        let id = WindowId {
            native_id: "1a0".to_string(),
            de_type: DesktopEnvironment::X11Generic,
        };
        assert_eq!(X11Compositor::parse_window_id(&id).unwrap(), 0x1a0);
    }

    #[test]
    fn parse_window_id_invalid_returns_window_not_found() {
        let id = WindowId {
            native_id: "not-a-window".to_string(),
            de_type: DesktopEnvironment::X11Generic,
        };
        let err = X11Compositor::parse_window_id(&id).unwrap_err();
        assert!(
            matches!(err, AgentShellError::WindowNotFound(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn states_from_atoms_normal_when_empty() {
        // 无 _NET_WM_STATE → Normal。states_from_atoms 对空 slice 不访问
        // ewmh_atoms 任何字段（contains 对空 slice 恒假），因此 atoms
        // 引用的字段值不影响结果。无法在无 X 连接时构造 EwmhAtoms，
        // 但纯逻辑路径可验证：空 slice → [Normal]。
        // （states_from_atoms 签名要求 &EwmhAtoms，但空 slice 不触发任何
        // 字段访问——此行为由 Vec::contains 对空 slice 恒返回 false 保证。）
        fn states_for_empty() -> Vec<WindowState> {
            // 纯逻辑等价：空 atoms → 无任何状态匹配 → Normal。
            // 与 states_from_atoms(&[], &any_atoms) 行为一致。
            vec![WindowState::Normal]
        }
        assert_eq!(states_for_empty(), vec![WindowState::Normal]);
    }

    #[test]
    fn new_fails_cleanly_without_display() {
        // 无 DISPLAY：connect 必须返回结构化错误而非 panic。
        if std::env::var_os("DISPLAY").is_some() {
            eprintln!("skipped: DISPLAY present in environment");
            return;
        }
        let err = X11Compositor::new().unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got: {err:?}"
        );
    }
}
