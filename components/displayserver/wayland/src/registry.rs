//! wl_display 连接与 registry 初始化。
//!
//! 对应设计文档 §1 `registry.rs` / §5.2：`registry_queue_init` 建立初始
//! globals 快照；本层不消费动态 global 变化（协议热插拔极少见，T3b 事件
//! 任务如需感知可在派发线程扩展）。

use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, QueueHandle};

use crate::core_protocols::protocol_versions;
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

/// Wayland 显示服务器（**纯 Wayland core 协议通道基础**，设计文档 §5）。
///
/// 只负责 `wl_display` 连接生命周期、registry 遍历与 global 接口探测，
/// **不绑定 wlr 协议**——wlr 标准协议绑定已下移至
/// `WlrWaylandCompositor`（`components/compositor/wlr-wayland/`），各 DE
/// 私有协议属于具体合成器。不实现 `CompositorComponent`——它是
/// `WaylandCompositor`（`components/compositor/wayland-core`）系合成器的
/// 协议通道基础；窗口/工作区语义由上层合成器组件实现。
#[derive(Debug)]
pub struct WaylandDisplayServer {
    conn: Connection,
    globals: wayland_client::globals::GlobalList,
    /// 绑定失败的协议名 → 错误描述（doctor 报告用，§5.5 验证输出数据源）。
    bind_failures: Vec<(&'static str, String)>,
}

impl WaylandDisplayServer {
    /// 连接 `$WAYLAND_DISPLAY` 并遍历 registry 建立 globals 快照。
    ///
    /// 纯 Wayland core：只做连接管理与接口探测。wlr 标准协议与各 DE 私有
    /// 协议由上层（`WlrWaylandCompositor` / 各合成器）基于
    /// [`connection`](Self::connection) + [`globals`](Self::globals) 叠加绑定。
    /// 绑定失败不视为连接失败——`bind_failures` 始终为空，仅为 doctor
    /// 报告接口保留（纯 core 层没有可失败的协议绑定）。
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

        // registry 快照建立即完成——本层不绑定任何协议扩展。
        queue
            .roundtrip(&mut RegistryState)
            .map_err(|e| AgentShellError::BackendUnavailable(format!("wayland roundtrip: {e}")))?;

        Ok(Self {
            conn,
            globals,
            bind_failures: Vec::new(),
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

    /// 绑定失败明细（doctor 报告）。
    pub fn bind_failures(&self) -> &[(&'static str, String)] {
        &self.bind_failures
    }

    /// registry 中已探测到的 global 接口数（doctor 报告「globals : N」）。
    pub fn global_count(&self) -> usize {
        self.globals.contents().clone_list().len()
    }

    /// 懒绑定默认 seat（`wl_seat`，纯 core 接口；activate 等请求的参数）。
    ///
    /// seat 缺失返回 `None`：请求无 seat 无法发出，调用方降级。
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
