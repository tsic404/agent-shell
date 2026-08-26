//! `WlrWaylandCompositor`：wlroots 系合成器基类（设计文档 §3.3 / §11）。
//!
//! 继承 [`WaylandCompositor`]（组合纯 core 的 [`WaylandDisplayServer`]），
//! 在其上叠加 **wlr 标准协议**与相关扩展协议绑定：foreign-toplevel-management /
//! output-management / screencopy / virtual-pointer / ext-workspace /
//! virtual-keyboard / data-control。
//!
//! 自身就是完整实现，可直接装配使用（未知 Wayland compositor 兜底，
//! §3.3 调用优先级矩阵「WLRWayland」行）；Treeland / Hyprland / Sway
//! 组合本类型并叠加各自私有协议。
//!
//! # 实现策略
//!
//! foreign-toplevel-management 是事件驱动协议：合成器推送 `toplevel` /
//! `title` / `app_id` / `state` / `closed` 事件，客户端聚合后才有窗口列表。
//! 本实现维护一个 [`ToplevelCache`]（`Arc<Mutex<…>>`），在 roundtrip 时
//! 填充；窗口操作方法（list/focus/close 等）基于缓存中的 handle 发起请求。
//!
//! ext-workspace 同理：`workspace_group` / `workspace` / `name` / `state`
//! 事件聚合为 [`WorkspaceCache`]。
//!
//! 降级策略（§5.4）：foreign-toplevel 缺失 → 窗口操作返回 NotImplemented
//! （portal/AT-SPI 降级由上层 CaptureComponent/InputComponent 负责，不
//! 在本合成器范围）；screencopy 缺失 → 截图降级 portal（由调用方处理）。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex as SyncMutex;
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::{
    Event as WsHandleEvent, ExtWorkspaceHandleV1,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::{
    Event as WsMgrEvent, ExtWorkspaceManagerV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    Event as ToplevelEvent, State as ToplevelState, ZwlrForeignToplevelHandleV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    Event as MgrEvent, ZwlrForeignToplevelManagerV1,
};

use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::event::{DesktopEvent, EventStream};
use agent_shell_core::types::{
    DesktopEnvironment, MonitorInfo, Rect, WindowId, WindowInfo, WindowState, WindowType,
    WorkspaceId, WorkspaceInfo,
};
use agent_shell_displayserver_wayland::WaylandDisplayServer;

use crate::wlr_protocols::{protocol_versions, WlrBindings};
use agent_shell_compositor_wayland_core::WaylandCompositor;

// ───────────────────────── foreign-toplevel 事件缓存 ─────────────────────────

/// 单个 toplevel 的聚合信息（事件驱动填充）。
#[derive(Clone, Debug, Default)]
struct ToplevelInfo {
    title: String,
    app_id: String,
    /// state 事件的原始 u8 数组（对应 State 枚举的位值）。
    state_bytes: Vec<u8>,
    /// wayland 对象 id（用于 stable native_id）。
    object_id: String,
}

impl ToplevelInfo {
    fn is_maximized(&self) -> bool {
        self.state_bytes.contains(&(ToplevelState::Maximized as u8))
    }
    fn is_minimized(&self) -> bool {
        self.state_bytes.contains(&(ToplevelState::Minimized as u8))
    }
    fn is_activated(&self) -> bool {
        self.state_bytes.contains(&(ToplevelState::Activated as u8))
    }
    fn is_fullscreen(&self) -> bool {
        self.state_bytes
            .contains(&(ToplevelState::Fullscreen as u8))
    }
}

/// foreign-toplevel 事件缓存（线程安全共享）。
///
/// 在 roundtrip 时填充；list_windows 等方法读取快照。
type ToplevelCache = Arc<SyncMutex<HashMap<u32, (ZwlrForeignToplevelHandleV1, ToplevelInfo)>>>;

// ───────────────────────── ext-workspace 事件缓存 ─────────────────────────

/// 单个工作区的聚合信息。
#[derive(Clone, Debug, Default)]
struct WorkspaceEntry {
    name: String,
    is_active: bool,
}

/// ext-workspace 事件缓存。
type WorkspaceCacheData = Arc<SyncMutex<HashMap<String, WorkspaceEntry>>>;

// ───────────────────────── 派发状态 ─────────────────────────

/// wlr 派发状态：聚合 foreign-toplevel 与 ext-workspace 事件到共享缓存。
///
/// 所有字段为 Arc，clone 为浅拷贝——派发队列与组件各持一份。
#[derive(Debug, Clone)]
pub struct WlrState {
    /// foreign-toplevel handle 列表（id → (handle, info)）。
    toplevels: ToplevelCache,
    /// ext-workspace 列表（id → entry）。
    workspaces: WorkspaceCacheData,
}

impl WlrState {
    fn new() -> Self {
        Self {
            toplevels: Arc::new(SyncMutex::new(HashMap::new())),
            workspaces: Arc::new(SyncMutex::new(HashMap::new())),
        }
    }

    /// 缓存快照（handle id → (handle, info)）。
    fn snapshot(&self) -> Vec<(ZwlrForeignToplevelHandleV1, ToplevelInfo)> {
        let cache = self.toplevels.lock();
        let mut out: Vec<_> = cache
            .values()
            .map(|(h, i)| (h.clone(), i.clone()))
            .collect();
        // stacking order：unknown（协议不提供），按 id 排序保证稳定。
        out.sort_by_key(|(_, i)| i.object_id.clone());
        out
    }

    /// 按 object_id 查找 handle。
    fn find_handle(&self, native_id: &str) -> Option<ZwlrForeignToplevelHandleV1> {
        self.toplevels
            .lock()
            .values()
            .find(|(_, i)| i.object_id == native_id)
            .map(|(h, _)| h.clone())
    }
}

impl Default for WlrState {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for WlrState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WlrState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// foreign-toplevel manager 事件：toplevel (new) / finished
impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        _mgr: &ZwlrForeignToplevelManagerV1,
        event: <ZwlrForeignToplevelManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            MgrEvent::Toplevel { toplevel } => {
                let id = toplevel.id();
                let object_id = format!("{id:?}");
                let numeric_id = id.protocol_id();
                state.toplevels.lock().insert(
                    numeric_id,
                    (
                        toplevel.clone(),
                        ToplevelInfo {
                            object_id,
                            ..Default::default()
                        },
                    ),
                );
            }
            MgrEvent::Finished => {}
            _ => {}
        }
    }
}

// foreign-toplevel handle 事件：title / app_id / state / done / closed
impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: <ZwlrForeignToplevelHandleV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let numeric_id = handle.id().protocol_id();
        let mut cache = state.toplevels.lock();
        match event {
            ToplevelEvent::Title { title } => {
                if let Some((_, info)) = cache.get_mut(&numeric_id) {
                    info.title = title;
                }
            }
            ToplevelEvent::AppId { app_id } => {
                if let Some((_, info)) = cache.get_mut(&numeric_id) {
                    info.app_id = app_id;
                }
            }
            ToplevelEvent::State { state: st } => {
                if let Some((_, info)) = cache.get_mut(&numeric_id) {
                    info.state_bytes = st;
                }
            }
            ToplevelEvent::Closed => {
                cache.remove(&numeric_id);
                handle.destroy();
            }
            ToplevelEvent::Done => {}
            ToplevelEvent::OutputEnter { .. } | ToplevelEvent::OutputLeave { .. } => {}
            _ => {}
        }
    }
}

// ext-workspace manager 事件：workspace_group / workspace / done / finished
impl Dispatch<ExtWorkspaceManagerV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        _mgr: &ExtWorkspaceManagerV1,
        event: <ExtWorkspaceManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            WsMgrEvent::Workspace { workspace } => {
                let id = format!("{:?}", workspace.id());
                state.workspaces.lock().entry(id).or_default();
                // workspace handle 事件由 Dispatch<ExtWorkspaceHandleV1> 处理
                // 但 ExtWorkspaceHandleV1 不是独立 global——它是 manager 的 new_id。
                // wayland-client 0.31 要求 handle 的 Dispatch 实现存在即可。
                let _ = workspace;
            }
            WsMgrEvent::Done | WsMgrEvent::Finished => {}
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: <ExtWorkspaceHandleV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = format!("{:?}", handle.id());
        match event {
            WsHandleEvent::Name { name } => {
                let mut cache = state.workspaces.lock();
                cache.entry(id).or_default().name = name;
            }
            WsHandleEvent::State { state: st } => {
                // ext-workspace State 被 wayland-client 包装为 WEnum<State>。
                use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::State as WsState;
                let is_active = matches!(st, wayland_client::WEnum::Value(WsState::Active));
                let mut cache = state.workspaces.lock();
                cache.entry(id).or_default().is_active = is_active;
            }
            WsHandleEvent::Removed => {
                let mut cache = state.workspaces.lock();
                cache.remove(&id);
                handle.destroy();
            }
            _ => {}
        }
    }
}

// inert dispatch for protocols we don't aggregate events from
macro_rules! inert_dispatch {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for WlrState {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

inert_dispatch!(wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1);
inert_dispatch!(wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
inert_dispatch!(wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
inert_dispatch!(wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
inert_dispatch!(wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1);

// ───────────────────────── WlrWaylandCompositor ─────────────────────────

/// WlrWaylandCompositor（§11.1）：纯 core 通道 + wlr 标准协议绑定。
///
/// 「继承」表达：struct 内嵌基类通道字段 + 实现 [`WaylandCompositor`]，
/// 使任何接受 Wayland 系合成器的抽象位置都能容纳它。
pub struct WlrWaylandCompositor {
    /// 基类协议通道（同一 `wl_display`，私有协议叠加复用）。
    display_server: WaylandDisplayServer,
    /// wlr 标准协议 + 扩展协议绑定集合。
    bindings: WlrBindings,
    /// foreign-toplevel / ext-workspace 事件派发状态（共享缓存）。
    state: WlrState,
    /// 事件派发队列（roundtrip 用）。
    queue: SyncMutex<wayland_client::EventQueue<WlrState>>,
}

impl std::fmt::Debug for WlrWaylandCompositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WlrWaylandCompositor")
            .field("display_server", &self.display_server)
            .field("bindings", &self.bindings)
            .field("bound_count", &self.bound_count())
            .finish()
    }
}

impl WlrWaylandCompositor {
    /// 连接 `$WAYLAND_DISPLAY` 并绑定全部可用 wlr 标准协议。
    ///
    /// 任一协议缺失/版本过低/绑定失败都不会使连接失败——对应字段为
    /// `None` 并记入 [`bind_failures`](Self::bind_failures)。
    pub fn connect() -> Result<Self> {
        Self::connect_with(WaylandDisplayServer::connect()?)
    }

    /// 基于既有显示服务器初始化（子类叠加私有协议前先构造本类型）。
    pub fn connect_with(ds: WaylandDisplayServer) -> Result<Self> {
        let mut state = WlrState::new();
        let mut queue = ds.connection().new_event_queue::<WlrState>();
        let qh = &queue.handle();
        let mut bindings = WlrBindings::default();

        // 类型标注的辅助宏：每个协议独立绑定并记录结果。
        macro_rules! bind_protocol {
            ($field:ident, $iface:literal, $ty:ty, $range:expr) => {{
                match ds.globals().bind::<$ty, WlrState, _>(qh, $range, ()) {
                    Ok(proxy) => bindings.$field = Some(proxy),
                    Err(e) => bindings.bind_failures.push(($iface, e.to_string())),
                }
            }};
        }

        bind_protocol!(
            foreign_toplevel,
            "zwlr_foreign_toplevel_manager_v1",
            ZwlrForeignToplevelManagerV1,
            protocol_versions::FOREIGN_TOPLEVEL.0..=protocol_versions::FOREIGN_TOPLEVEL.1
        );
        bind_protocol!(
            output_management,
            "zwlr_output_manager_v1",
            wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1,
            protocol_versions::OUTPUT_MANAGEMENT.0..=protocol_versions::OUTPUT_MANAGEMENT.1
        );
        bind_protocol!(
            screencopy,
            "zwlr_screencopy_manager_v1",
            wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
            protocol_versions::SCREENCOPY.0..=protocol_versions::SCREENCOPY.1
        );
        // virtual-pointer 接口规范最高 v2，上界必须写死为接口版本。
        match ds.globals().bind::<
            wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
            WlrState,
            _,
        >(
            qh,
            protocol_versions::VIRTUAL_POINTER_MIN..=protocol_versions::VIRTUAL_POINTER_MAX,
            (),
        ) {
            Ok(proxy) => bindings.virtual_pointer = Some(proxy),
            Err(e) => bindings
                .bind_failures
                .push(("zwlr_virtual_pointer_manager_v1", e.to_string())),
        }

        bind_protocol!(
            ext_workspace,
            "ext_workspace_manager_v1",
            ExtWorkspaceManagerV1,
            protocol_versions::EXT_WORKSPACE.0..=protocol_versions::EXT_WORKSPACE.1
        );
        bind_protocol!(
            virtual_keyboard,
            "zwp_virtual_keyboard_manager_v1",
            wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
            protocol_versions::VIRTUAL_KEYBOARD.0..=protocol_versions::VIRTUAL_KEYBOARD.1
        );
        bind_protocol!(
            data_control,
            "ext_data_control_manager_v1",
            wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1,
            protocol_versions::DATA_CONTROL.0..=protocol_versions::DATA_CONTROL.1
        );

        // 用 roundtrip 冲刷 bind + 收集初始事件（toplevel 列表等）。
        // 两次 roundtrip：第一次处理 bind 确认，第二次收集 toplevel 事件。
        queue
            .roundtrip(&mut state)
            .map_err(|e| AgentShellError::BackendUnavailable(format!("wlr roundtrip 1: {e}")))?;
        queue
            .roundtrip(&mut state)
            .map_err(|e| AgentShellError::BackendUnavailable(format!("wlr roundtrip 2: {e}")))?;

        Ok(Self {
            display_server: ds,
            bindings,
            state,
            queue: SyncMutex::new(queue),
        })
    }

    /// 当前 wlr 绑定集合。
    pub fn bindings(&self) -> &WlrBindings {
        &self.bindings
    }

    /// 可变的绑定集合（子类在私有协议探测后回填共享字段）。
    pub fn bindings_mut(&mut self) -> &mut WlrBindings {
        &mut self.bindings
    }

    /// 直接访问 foreign-toplevel 管理器（窗口操作通道）。
    pub fn foreign_toplevel(&self) -> Option<&ZwlrForeignToplevelManagerV1> {
        self.bindings.foreign_toplevel.as_ref()
    }

    /// 绑定失败明细（doctor 报告）。
    pub fn bind_failures(&self) -> &[(&'static str, String)] {
        &self.bindings.bind_failures
    }

    /// 已绑定的协议计数（doctor 报告「wlr 协议 : N/7」）。
    pub fn bound_count(&self) -> usize {
        usize::from(self.bindings.foreign_toplevel.is_some())
            + usize::from(self.bindings.output_management.is_some())
            + usize::from(self.bindings.screencopy.is_some())
            + usize::from(self.bindings.virtual_pointer.is_some())
            + usize::from(self.bindings.ext_workspace.is_some())
            + usize::from(self.bindings.virtual_keyboard.is_some())
            + usize::from(self.bindings.data_control.is_some())
    }

    /// doctor 诊断输出（§5.5 验证输出格式）。
    pub fn doctor_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();

        // Wayland 连接
        let gc = self.display_server.global_count();
        lines.push(format!(
            "{} Wayland 显示  : wayland-0 (wl_display connected, {gc} globals)",
            "✓"
        ));

        // wlr 协议绑定
        let bound = self.bound_count();
        lines.push(format!(
            "{} wlr 协议      : {bound}/7 bound",
            if bound > 0 { "✓" } else { "⚠" }
        ));

        // 逐协议明细
        let detail: [(&str, bool, &str); 7] = [
            (
                "foreign-toplevel",
                self.bindings.foreign_toplevel.is_some(),
                "v3",
            ),
            (
                "output-mgmt",
                self.bindings.output_management.is_some(),
                "v4",
            ),
            ("screencopy", self.bindings.screencopy.is_some(), "v3"),
            (
                "virtual-pointer",
                self.bindings.virtual_pointer.is_some(),
                "v2",
            ),
            ("ext-workspace", self.bindings.ext_workspace.is_some(), "v1"),
            (
                "virtual-keyboard",
                self.bindings.virtual_keyboard.is_some(),
                "v1",
            ),
            ("data-control", self.bindings.data_control.is_some(), "v1"),
        ];
        for (name, bound, ver) in detail {
            match bound {
                true => lines.push(format!("  ├─ {name:<20} : {ver} ✓")),
                false => lines.push(format!("  ├─ {name:<20} : not bound ⚠")),
            }
        }

        // 事件流
        if self.bindings.foreign_toplevel.is_some() {
            lines.push("✓ 事件流       : foreign-toplevel events aggregated".into());
        } else {
            lines.push("⚠ 事件流       : foreign-toplevel unbound (portal/AT-SPI 降级)".into());
        }

        lines
    }

    // ───────────────────────── 内部辅助 ─────────────────────────

    /// 刷新事件缓存（roundtrip 收集最新的 toplevel/workspace 事件）。
    fn refresh(&self) -> Result<()> {
        let mut queue = self.queue.lock();
        // WlrState 全 Arc 字段，clone 为浅拷贝——roundtrip 事件写入
        // 共享缓存，组件方的 self.state 与队列方的 clone 指向同一份。
        let mut st = self.state.clone();
        queue.roundtrip(&mut st).map_err(|e| {
            AgentShellError::BackendUnavailable(format!("wlr refresh roundtrip: {e}"))
        })?;
        Ok(())
    }

    /// foreign-toplevel 是否可用。
    fn has_foreign_toplevel(&self) -> bool {
        self.bindings.foreign_toplevel.is_some()
    }

    /// 将 ToplevelInfo + handle 转为 WindowInfo。
    fn to_window_info(info: &ToplevelInfo, stacking_order: u32) -> WindowInfo {
        let mut states = Vec::new();
        if info.is_minimized() {
            states.push(WindowState::Minimized);
        }
        if info.is_maximized() {
            states.push(WindowState::Maximized);
        }
        if info.is_fullscreen() {
            states.push(WindowState::FullScreen);
        }
        if states.is_empty() {
            states.push(WindowState::Normal);
        }

        WindowInfo {
            id: WindowId {
                native_id: info.object_id.clone(),
                de_type: DesktopEnvironment::WLRWayland,
            },
            title: info.title.clone(),
            app_id: info.app_id.clone(),
            pid: 0,                    // wlr foreign-toplevel 不提供 PID
            geometry: Rect::default(), // 协议不提供窗口几何
            frame_geometry: Rect::default(),
            states,
            workspace_id: None, // ext-workspace 关联需要额外事件聚合
            monitor_id: None,
            stacking_order,
            desktop_file: None,
            window_type: WindowType::Normal,
            icon_geometry: None,
            keep_above: false,
        }
    }
}

impl WaylandCompositor for WlrWaylandCompositor {
    fn display_server(&self) -> &WaylandDisplayServer {
        &self.display_server
    }
}

// ───────────────────────── DesktopComponent ─────────────────────────

#[async_trait]
impl DesktopComponent for WlrWaylandCompositor {
    fn name(&self) -> &'static str {
        "wlr-wayland-compositor"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Compositor
    }

    fn is_available(&self) -> bool {
        true // 构造成功即可用；协议缺失由 health 表达
    }

    async fn health(&self) -> ComponentHealth {
        match (self.bound_count(), self.bindings.foreign_toplevel.as_ref()) {
            (_, Some(_)) if self.bindings.bind_failures.is_empty() => ComponentHealth::Healthy,
            (_, Some(_)) => {
                ComponentHealth::Degraded("wlr protocols partial; portal fallback".into())
            }
            (0, None) => {
                ComponentHealth::Degraded("no wlr protocols; full portal/AT-SPI fallback".into())
            }
            _ => ComponentHealth::Degraded("foreign-toplevel unbound; degraded window ops".into()),
        }
    }
}

// ───────────────────────── CompositorComponent ─────────────────────────

#[async_trait]
impl CompositorComponent for WlrWaylandCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        let ft = self.has_foreign_toplevel();
        BackendCapabilities {
            window_management: ft,
            workspace_management: self.bindings.ext_workspace.is_some(),
            monitor_layout: self.bindings.output_management.is_some(),
            window_events: false, // foreign-toplevel 事件已聚合到缓存，但 subscribe() 尚未推送；T3b 归一化落地后改回 true
            workspace_events: self.bindings.ext_workspace.is_some(),
            native_input: self.bindings.virtual_pointer.is_some()
                && self.bindings.virtual_keyboard.is_some(),
            native_capture: self.bindings.screencopy.is_some(),
            virtual_desktops: self.bindings.ext_workspace.is_some(),
            effects_control: false,
        }
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        self.refresh()?;
        let snapshot = self.state.snapshot();
        Ok(snapshot
            .iter()
            .enumerate()
            .map(|(i, (_, info))| Self::to_window_info(info, i as u32))
            .collect())
    }

    async fn get_active_window(&self) -> Result<Option<WindowInfo>> {
        self.refresh()?;
        let snapshot = self.state.snapshot();
        Ok(snapshot
            .iter()
            .find(|(_, info)| info.is_activated())
            .map(|(_, info)| Self::to_window_info(info, 0)))
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        self.refresh()?;
        let handle = self
            .state
            .find_handle(&id.native_id)
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))?;
        // activate 需要 seat；绑定默认 seat（纯 core wl_seat）。
        let qh = self.queue.lock().handle();
        let seat = self.display_server.default_seat(&qh);
        if let Some(seat) = seat {
            handle.activate(&seat);
        } else {
            return Err(AgentShellError::BackendUnavailable(
                "no wl_seat available for activate".into(),
            ));
        }
        if let Err(e) = self.display_server.flush() {
            tracing::warn!(error = %e, "wlr flush failed after protocol request");
        }
        Ok(())
    }

    async fn move_window(&self, _id: &WindowId, _x: i32, _y: i32) -> Result<()> {
        // wlr foreign-toplevel 不提供窗口移动能力（无 set_geometry 请求）。
        // 窗口几何操作属于各 DE 私有协议（如 hyprland_toplevel_export）。
        Err(AgentShellError::NotImplemented(
            "wlr foreign-toplevel has no move_window; DE private protocol required".into(),
        ))
    }

    async fn resize_window(&self, _id: &WindowId, _w: i32, _h: i32) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "wlr foreign-toplevel has no resize_window; DE private protocol required".into(),
        ))
    }

    async fn minimize_window(&self, id: &WindowId) -> Result<()> {
        self.refresh()?;
        let handle = self
            .state
            .find_handle(&id.native_id)
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))?;
        handle.set_minimized();
        if let Err(e) = self.display_server.flush() {
            tracing::warn!(error = %e, "wlr flush failed after protocol request");
        }
        Ok(())
    }

    async fn unminimize_window(&self, id: &WindowId) -> Result<()> {
        self.refresh()?;
        let handle = self
            .state
            .find_handle(&id.native_id)
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))?;
        handle.unset_minimized();
        if let Err(e) = self.display_server.flush() {
            tracing::warn!(error = %e, "wlr flush failed after protocol request");
        }
        Ok(())
    }

    async fn maximize_window(&self, id: &WindowId) -> Result<()> {
        self.refresh()?;
        let handle = self
            .state
            .find_handle(&id.native_id)
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))?;
        handle.set_maximized();
        if let Err(e) = self.display_server.flush() {
            tracing::warn!(error = %e, "wlr flush failed after protocol request");
        }
        Ok(())
    }

    async fn close_window(&self, id: &WindowId) -> Result<()> {
        self.refresh()?;
        let handle = self
            .state
            .find_handle(&id.native_id)
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))?;
        handle.close();
        if let Err(e) = self.display_server.flush() {
            tracing::warn!(error = %e, "wlr flush failed after protocol request");
        }
        Ok(())
    }

    async fn set_window_geometry(&self, _id: &WindowId, _geo: Rect) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "wlr foreign-toplevel has no set_window_geometry; DE private protocol required".into(),
        ))
    }

    async fn get_window_info(&self, id: &WindowId) -> Result<WindowInfo> {
        self.refresh()?;
        let snapshot = self.state.snapshot();
        snapshot
            .iter()
            .find(|(_, info)| info.object_id == id.native_id)
            .map(|(_, info)| Self::to_window_info(info, 0))
            .ok_or_else(|| AgentShellError::WindowNotFound(id.native_id.clone()))
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        self.refresh()?;
        let cache = self.state.workspaces.lock();
        let out: Vec<WorkspaceInfo> = cache
            .iter()
            .enumerate()
            .map(|(i, (id, entry))| WorkspaceInfo {
                id: WorkspaceId {
                    native_id: id.clone(),
                    de_type: DesktopEnvironment::WLRWayland,
                },
                name: entry.name.clone(),
                number: i as u32 + 1,
                is_active: entry.is_active,
                monitor_ids: Vec::new(),
                window_ids: Vec::new(),
            })
            .collect();
        Ok(out)
    }

    async fn activate_workspace(&self, _id: &WorkspaceId) -> Result<()> {
        // ext-workspace v1 的 activate 请求在 ExtWorkspaceHandleV1 上。
        // 需要从缓存中找到对应的 handle 并调用 activate。
        // 当前缓存只存 name/is_active，handle 本身需要额外保存。
        Err(AgentShellError::NotImplemented(
            "ext-workspace activate requires handle aggregation (T3b events)".into(),
        ))
    }

    async fn move_window_to_workspace(&self, _wid: &WindowId, _ws: &WorkspaceId) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "wlr foreign-toplevel has no move_to_workspace; ext-workspace assign (T3b)".into(),
        ))
    }

    async fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        // output-management 协议事件聚合在 T3b 范围。
        // 当前返回空列表——调用方据 capabilities().monitor_layout 判断。
        Ok(Vec::new())
    }

    async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
        // foreign-toplevel 事件已聚合到缓存；subscribe 返回一个从缓存
        // 变化推送事件的流。T3b 归一化任务将完善事件映射。
        Ok(Box::new(WlrEventStream::new(self.state.toplevels.clone())))
    }
}

// ───────────────────────── 事件流 ─────────────────────────

/// WlrWaylandCompositor 事件流（从 toplevel 缓存变化推送 DesktopEvent）。
///
/// 当前实现为轮询式：每次 `next_event` 做 roundtrip 后比较缓存变化。
/// T3b 归一化任务将替换为事件驱动推送。
struct WlrEventStream {
    #[allow(dead_code)]
    toplevels: ToplevelCache,
    /// 已推送过的 toplevel id 集合（用于检测 opened/closed）。
    #[allow(dead_code)]
    seen: SyncMutex<HashMap<u32, WindowId>>,
}

impl WlrEventStream {
    fn new(toplevels: ToplevelCache) -> Self {
        Self {
            toplevels,
            seen: SyncMutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl EventStream for WlrEventStream {
    async fn next_event(&self) -> Option<DesktopEvent> {
        // 事件流为空：foreign-toplevel 事件变化检测需要轮询 roundtrip，
        // 但 roundtrip 在 async 上下文中需要 spawn_blocking。T3b 事件
        // 归一化任务将提供真正的事件驱动推送。当前返回 None（流结束）。
        None
    }
}

// ───────────────────────── 测试 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_fails_cleanly_without_wayland_display() {
        // 无 WAYLAND_DISPLAY：connect 必须返回结构化错误而非 panic。
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some()
        {
            return;
        }
        let err = WlrWaylandCompositor::connect().unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn toplevel_info_state_parsing() {
        // 验证 state_bytes → is_maximized/is_minimized 等的位匹配逻辑。
        let mut info = ToplevelInfo::default();
        info.state_bytes = vec![ToplevelState::Maximized as u8];
        assert!(info.is_maximized());
        assert!(!info.is_minimized());

        info.state_bytes = vec![
            ToplevelState::Minimized as u8,
            ToplevelState::Activated as u8,
        ];
        assert!(info.is_minimized());
        assert!(info.is_activated());
        assert!(!info.is_maximized());

        info.state_bytes = vec![ToplevelState::Fullscreen as u8];
        assert!(info.is_fullscreen());
    }

    #[test]
    fn to_window_info_normal_when_no_states() {
        let info = ToplevelInfo {
            title: "Test".into(),
            app_id: "test.app".into(),
            object_id: "wl_toplevel@1".into(),
            state_bytes: vec![],
        };
        let wi = WlrWaylandCompositor::to_window_info(&info, 0);
        assert_eq!(wi.states, vec![WindowState::Normal]);
        assert_eq!(wi.title, "Test");
        assert_eq!(wi.app_id, "test.app");
        assert_eq!(wi.id.de_type, DesktopEnvironment::WLRWayland);
        assert_eq!(wi.id.native_id, "wl_toplevel@1");
    }

    #[test]
    fn wlr_state_snapshot_empty() {
        let mut state = WlrState::new();
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn wlr_state_find_handle_empty() {
        let mut state = WlrState::new();
        assert!(state.find_handle("nonexistent").is_none());
    }
}
