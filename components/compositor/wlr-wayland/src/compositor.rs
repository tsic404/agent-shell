//! `WlrWaylandCompositor`：wlroots 系合成器基类（设计文档 §3.3 / §11）。
//!
//! 继承 [`WaylandCompositor`]（组合纯 core 的 [`WaylandDisplayServer`]），
//! 在其上叠加 **wlr 标准协议**绑定：foreign-toplevel-management /
//! output-management / screencopy / virtual-pointer。
//!
//! 自身就是完整实现，可直接装配使用（未知 Wayland compositor 兜底，
//! §3.3 调用优先级矩阵「WLRWayland」行）；Treeland / Hyprland / Sway
//! 组合本类型并叠加各自私有协议。

use async_trait::async_trait;

use agent_shell_compositor_wayland_core::{WaylandCompositor, WaylandDisplayServer};
use agent_shell_core::component::{
    BackendCapabilities, ComponentHealth, ComponentType, CompositorComponent, DesktopComponent,
};
use agent_shell_core::error::Result;
use agent_shell_core::EventStream;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;

use crate::wlr_protocols::{protocol_versions, WlrBindings};

/// wlr 派发状态：本层不消费任何协议事件（事件流归 T3b / 各合成器 event 模块）。
#[derive(Debug)]
pub struct WlrState;

impl
    wayland_client::Dispatch<
        wayland_client::protocol::wl_registry::WlRegistry,
        wayland_client::globals::GlobalListContents,
    > for WlrState
{
    fn event(
        _state: &mut Self,
        _registry: &wayland_client::protocol::wl_registry::WlRegistry,
        _event: wayland_client::protocol::wl_registry::Event,
        _udata: &wayland_client::globals::GlobalListContents,
        _conn: &wayland_client::Connection,
        _qh: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

/// 各 wlr 协议对象的事件在本层均不消费。
macro_rules! inert_dispatch {
    ($ty:ty) => {
        impl wayland_client::Dispatch<$ty, ()> for WlrState {
            fn event(
                _state: &mut Self,
                _proxy: &$ty,
                _event: <$ty as wayland_client::Proxy>::Event,
                _udata: &(),
                _conn: &wayland_client::Connection,
                _qh: &wayland_client::QueueHandle<Self>,
            ) {
            }
        }
    };
}

inert_dispatch!(ZwlrForeignToplevelManagerV1);
inert_dispatch!(ZwlrOutputManagerV1);
inert_dispatch!(ZwlrScreencopyManagerV1);
inert_dispatch!(ZwlrVirtualPointerManagerV1);

/// WlrWaylandCompositor（§11.1）：纯 core 通道 + wlr 标准协议绑定。
///
/// 「继承」表达：struct 内嵌基类通道字段 + 实现 [`WaylandCompositor`]，
/// 使任何接受 Wayland 系合成器的抽象位置都能容纳它。
pub struct WlrWaylandCompositor {
    /// 基类协议通道（同一 `wl_display`，私有协议叠加复用）。
    display_server: WaylandDisplayServer,
    /// wlr 标准协议绑定集合。
    bindings: WlrBindings,
}

impl std::fmt::Debug for WlrWaylandCompositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WlrWaylandCompositor")
            .field("display_server", &self.display_server)
            .field("bindings", &self.bindings)
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
        let globals = ds.globals();
        let mut queue = ds.connection().new_event_queue::<WlrState>();
        let qh = &queue.handle();
        let mut bindings = WlrBindings::default();

        // 类型标注的辅助宏展开目标：每个协议独立绑定并记录结果。
        macro_rules! bind_protocol {
            ($field:ident, $iface:literal, $ty:ty, $range:expr) => {{
                match globals.bind::<$ty, WlrState, _>(qh, $range, ()) {
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
            ZwlrOutputManagerV1,
            protocol_versions::OUTPUT_MANAGEMENT.0..=protocol_versions::OUTPUT_MANAGEMENT.1
        );
        bind_protocol!(
            screencopy,
            "zwlr_screencopy_manager_v1",
            ZwlrScreencopyManagerV1,
            protocol_versions::SCREENCOPY.0..=protocol_versions::SCREENCOPY.1
        );
        // 阻塞项 #1：virtual-pointer 接口规范最高 v2——`GlobalList::bind` 在取
        // min 之前先断言 `version.end() <= interface.version`，超界直接 panic
        // （wayland-client 0.31 globals.rs:167），因此上界必须写死为接口版本，
        // 不能传 compositor 公布值或 u32::MAX。
        match globals.bind::<ZwlrVirtualPointerManagerV1, WlrState, _>(
            qh,
            protocol_versions::VIRTUAL_POINTER_MIN..=protocol_versions::VIRTUAL_POINTER_MAX,
            (),
        ) {
            Ok(proxy) => bindings.virtual_pointer = Some(proxy),
            Err(e) => bindings
                .bind_failures
                .push(("zwlr_virtual_pointer_manager_v1", e.to_string())),
        }

        // 冲刷初始 bind 序列并等待 compositor 确认（错误对象会在此浮现）。
        queue.roundtrip(&mut WlrState).map_err(|e| {
            agent_shell_core::error::AgentShellError::BackendUnavailable(format!(
                "wlr roundtrip: {e}"
            ))
        })?;

        Ok(Self {
            display_server: ds,
            bindings,
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

    /// 已绑定的 wlr 协议计数（doctor 报告「wlr 协议 : N/M」）。
    pub fn bound_count(&self) -> usize {
        usize::from(self.bindings.foreign_toplevel.is_some())
            + usize::from(self.bindings.output_management.is_some())
            + usize::from(self.bindings.screencopy.is_some())
            + usize::from(self.bindings.virtual_pointer.is_some())
    }
    /// 占位方法的统一错误（T3b / TSI-2314 落地前显式 NotImplemented）。
    fn not_impl(&self) -> agent_shell_core::error::AgentShellError {
        agent_shell_core::error::AgentShellError::NotImplemented(
            "wlr foreign-toplevel aggregation lands with TSI-2314 (T3b events)".into(),
        )
    }
}

impl WaylandCompositor for WlrWaylandCompositor {
    fn display_server(&self) -> &WaylandDisplayServer {
        &self.display_server
    }
}

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

/// CompositorComponent 实现占位说明：
///
/// 本 crate 只落位**层次结构**与 wlr 协议绑定（本任务范围）；17 个窗口/
/// 工作区方法的完整实现依赖 foreign-toplevel 事件聚合（T3b），与设计文档
/// §11.1 的实现节奏一致。TSI-2314（T1g 兜底合成器）在本类型之上补全。
#[async_trait]
impl CompositorComponent for WlrWaylandCompositor {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }

    async fn list_windows(&self) -> Result<Vec<agent_shell_core::types::WindowInfo>> {
        Err(self.not_impl())
    }
    async fn get_active_window(&self) -> Result<Option<agent_shell_core::types::WindowInfo>> {
        Err(self.not_impl())
    }
    async fn focus_window(&self, _id: &agent_shell_core::types::WindowId) -> Result<()> {
        Err(self.not_impl())
    }
    async fn move_window(
        &self,
        _id: &agent_shell_core::types::WindowId,
        _x: i32,
        _y: i32,
    ) -> Result<()> {
        Err(self.not_impl())
    }
    async fn resize_window(
        &self,
        _id: &agent_shell_core::types::WindowId,
        _w: i32,
        _h: i32,
    ) -> Result<()> {
        Err(self.not_impl())
    }
    async fn minimize_window(&self, _id: &agent_shell_core::types::WindowId) -> Result<()> {
        Err(self.not_impl())
    }
    async fn unminimize_window(&self, _id: &agent_shell_core::types::WindowId) -> Result<()> {
        Err(self.not_impl())
    }
    async fn maximize_window(&self, _id: &agent_shell_core::types::WindowId) -> Result<()> {
        Err(self.not_impl())
    }
    async fn close_window(&self, _id: &agent_shell_core::types::WindowId) -> Result<()> {
        Err(self.not_impl())
    }
    async fn set_window_geometry(
        &self,
        _id: &agent_shell_core::types::WindowId,
        _geo: agent_shell_core::types::Rect,
    ) -> Result<()> {
        Err(self.not_impl())
    }
    async fn get_window_info(
        &self,
        _id: &agent_shell_core::types::WindowId,
    ) -> Result<agent_shell_core::types::WindowInfo> {
        Err(self.not_impl())
    }
    async fn list_workspaces(&self) -> Result<Vec<agent_shell_core::types::WorkspaceInfo>> {
        Err(self.not_impl())
    }
    async fn activate_workspace(&self, _id: &agent_shell_core::types::WorkspaceId) -> Result<()> {
        Err(self.not_impl())
    }
    async fn move_window_to_workspace(
        &self,
        _wid: &agent_shell_core::types::WindowId,
        _ws: &agent_shell_core::types::WorkspaceId,
    ) -> Result<()> {
        Err(self.not_impl())
    }
    async fn list_monitors(&self) -> Result<Vec<agent_shell_core::types::MonitorInfo>> {
        Err(self.not_impl())
    }
    async fn subscribe(&self) -> Result<Box<dyn EventStream>> {
        Err(self.not_impl())
    }
}
