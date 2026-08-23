//! wl_display 连接与 registry 初始化。
//!
//! 对应设计文档 §1 `registry.rs` / §5.2：`registry_queue_init` 建立初始
//! globals 快照；本层不消费动态 global 变化（协议热插拔极少见，T3b 事件
//! 任务如需感知可在派发线程扩展）。

use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;

use crate::wlr_protocols::{protocol_versions, WaylandBindings};
use agent_shell_core::error::{AgentShellError, Result};

/// registry 初始化状态：本层不消费任何协议事件。
#[derive(Debug)]
pub struct RegistryState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for RegistryState {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _udata: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for RegistryState {
    fn event(
        _state: &mut Self,
        _seat: &WlSeat,
        _event: wayland_client::protocol::wl_seat::Event,
        _udata: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// 各 wlr 协议对象的事件在本层均不消费（事件流归 T3b / 各合成器 event 模块）。
macro_rules! inert_dispatch {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for RegistryState {
            fn event(
                _state: &mut Self,
                _proxy: &$ty,
                _event: <$ty as wayland_client::Proxy>::Event,
                _udata: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

inert_dispatch!(ZwlrForeignToplevelManagerV1);
inert_dispatch!(ZwlrOutputManagerV1);
inert_dispatch!(ZwlrScreencopyManagerV1);
inert_dispatch!(ZwlrVirtualPointerManagerV1);

/// Wayland 显示服务器（协议通道基础，设计文档 §5）。
///
/// 不实现 `CompositorComponent`——它是各 DE 合成器的协议通道基础；
/// 窗口/工作区语义由上层合成器组件实现。
#[derive(Debug)]
pub struct WaylandDisplayServer {
    conn: Connection,
    globals: wayland_client::globals::GlobalList,
    bindings: WaylandBindings,
    /// 绑定失败的协议名 → 错误描述（doctor 报告用，§5.5 验证输出数据源）。
    bind_failures: Vec<(&'static str, String)>,
}

impl WaylandDisplayServer {
    /// 连接 `$WAYLAND_DISPLAY` 并遍历 registry 绑定全部可用 wlr 标准协议。
    ///
    /// 任一协议缺失/版本过低/绑定失败都不会使连接失败——对应字段为 `None`
    /// 并记入 [`bind_failures`](Self::bind_failures)。
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .map_err(|e| AgentShellError::BackendUnavailable(format!("wayland connect: {e}")))?;
        Self::connect_with(conn)
    }

    /// 基于既有连接初始化（测试与多会话场景用）。
    pub fn connect_with(conn: Connection) -> Result<Self> {
        let (globals, mut queue) =
            wayland_client::globals::registry_queue_init::<RegistryState>(&conn).map_err(|e| {
                AgentShellError::BackendUnavailable(format!("wayland registry init: {e}"))
            })?;

        let qh = &queue.handle();
        let mut bindings = WaylandBindings::default();
        let mut bind_failures: Vec<(&'static str, String)> = Vec::new();

        // 类型标注的辅助宏展开目标：每个协议独立绑定并记录结果。
        macro_rules! bind_protocol {
            ($field:ident, $iface:literal, $ty:ty, $range:expr) => {{
                match globals.bind::<$ty, RegistryState, _>(qh, $range, ()) {
                    Ok(proxy) => bindings.$field = Some(proxy),
                    Err(e) => bind_failures.push(($iface, e.to_string())),
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
        match globals.bind::<ZwlrVirtualPointerManagerV1, RegistryState, _>(
            qh,
            protocol_versions::VIRTUAL_POINTER_MIN..=protocol_versions::VIRTUAL_POINTER_MAX,
            (),
        ) {
            Ok(proxy) => bindings.virtual_pointer = Some(proxy),
            Err(e) => bind_failures.push(("zwlr_virtual_pointer_manager_v1", e.to_string())),
        }

        // 冲刷初始 bind 序列并等待 compositor 确认（错误对象会在此浮现）。
        queue
            .roundtrip(&mut RegistryState)
            .map_err(|e| AgentShellError::BackendUnavailable(format!("wayland roundtrip: {e}")))?;

        Ok(Self {
            conn,
            globals,
            bindings,
            bind_failures,
        })
    }

    /// 底层连接句柄（子类叠加私有协议时复用同一 `wl_display`）。
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// registry 全局列表（子类按接口名自行绑定 DE 私有协议）。
    pub fn globals(&self) -> &wayland_client::globals::GlobalList {
        &self.globals
    }

    /// 当前绑定集合。
    pub fn bindings(&self) -> &WaylandBindings {
        &self.bindings
    }

    /// 可变的绑定集合（子类在私有协议探测后回填共享字段）。
    pub fn bindings_mut(&mut self) -> &mut WaylandBindings {
        &mut self.bindings
    }

    /// 直接访问 foreign-toplevel 管理器（窗口操作通道）。
    pub fn foreign_toplevel(&self) -> Option<&ZwlrForeignToplevelManagerV1> {
        self.bindings.foreign_toplevel.as_ref()
    }

    /// 绑定失败明细（doctor 报告）。
    pub fn bind_failures(&self) -> &[(&'static str, String)] {
        &self.bind_failures
    }

    /// 已绑定协议计数（doctor 报告「wlr 协议 : N/M」）。
    pub fn bound_count(&self) -> usize {
        usize::from(self.bindings.foreign_toplevel.is_some())
            + usize::from(self.bindings.output_management.is_some())
            + usize::from(self.bindings.screencopy.is_some())
            + usize::from(self.bindings.virtual_pointer.is_some())
    }

    /// 为 `foreign-toplevel activate` 等请求懒绑定默认 seat。
    ///
    /// seat 缺失返回 `None`：activate 无 seat 无法发出，调用方降级。
    pub fn default_seat(
        &self,
        qh: &QueueHandle<impl Dispatch<WlSeat, ()> + 'static>,
    ) -> Option<WlSeat> {
        self.globals
            .bind(qh, protocol_versions::SEAT..=protocol_versions::SEAT, ())
            .ok()
    }

    /// 冲刷请求队列。
    pub fn flush(&self) -> Result<()> {
        self.conn
            .flush()
            .map_err(|e| AgentShellError::DBus(format!("wayland flush: {e}")))
    }
}

/// 快速检查某接口是否被当前 compositor 公布（子类私有协议探测复用）。
pub fn global_is_advertised(
    globals: &wayland_client::globals::GlobalList,
    interface: &str,
) -> bool {
    globals
        .contents()
        .clone_list()
        .iter()
        .any(|g| g.interface == interface)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_fails_cleanly_without_wayland_display() {
        // 无 WAYLAND_DISPLAY 的 CI/容器环境：connect 必须返回结构化错误而非 panic，
        // 且错误类别为 BackendUnavailable（降级链据此切换 X11/D-Bus 通道）。
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some()
        {
            // 存在真实 display 的环境无法断言失败，跳过（本地开发机）。
            return;
        }
        let err = WaylandDisplayServer::connect().unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got: {err:?}"
        );
    }
}
