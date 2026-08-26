//! Wayland 协议通道（设计文档 §9.4）：wlr 标准协议（基类已绑）+
//! hyprland_* 私有协议客户端。
//!
//! 在共享的 [`WlrWaylandCompositor`] 基类连接之上绑定 Hyprland 私有协议
//! 族（XML vendored 自 `hyprwm/hyprland-protocols`，经 wayland-scanner
//! 生成，见 [`crate::protocol_gen`]）：
//!
//! | 协议 | 版本 | 用途 |
//! |------|:----:|------|
//! | `hyprland_toplevel_export_manager_v1` | 2 | 窗口级内容捕获 |
//! | `hyprland_focus_grab_manager_v1` | 1 | 输入焦点白名单限制 |
//! | `hyprland_global_shortcuts_manager_v1` | 1 | 全局快捷键注册 |
//! | `hyprland_toplevel_mapping_manager_v1` | 1 | toplevel → 窗口地址映射 |
//!
//! 绑定策略与 wlr 基类一致：任一私有协议缺失/版本过低不致命——字段为
//! `None` 并记入 `bind_failures`（doctor 报告），能力按 §9.5 选择矩阵
//! 回退 hyprctl。事件在本层不消费（inert dispatch）；窗口管理走基类的
//! wlr-foreign-toplevel，本模块只负责私有通道的存在性与生命周期。

use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, QueueHandle};

use agent_shell_core::error::Result;
use agent_shell_displayserver_wayland::WaylandDisplayServer;

use crate::protocol_gen::client::hyprland_focus_grab_manager_v1::HyprlandFocusGrabManagerV1;
use crate::protocol_gen::client::hyprland_global_shortcuts_manager_v1::HyprlandGlobalShortcutsManagerV1;
use crate::protocol_gen::client::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;
use crate::protocol_gen::client::hyprland_toplevel_mapping_manager_v1::HyprlandToplevelMappingManagerV1;

/// 私有协议版本区间（接口规范版本即上界；越界 bind 会 panic，
/// 见 wlr_protocols.rs「阻塞项 #1」注释）。
pub mod protocol_versions {
    /// hyprland_toplevel_export_manager_v1 上游 v2。
    pub const TOPLEVEL_EXPORT: (u32, u32) = (1, 2);
    /// hyprland_focus_grab_manager_v1 上游 v1。
    pub const FOCUS_GRAB: (u32, u32) = (1, 1);
    /// hyprland_global_shortcuts_manager_v1 上游 v1。
    pub const GLOBAL_SHORTCUTS: (u32, u32) = (1, 1);
    /// hyprland_toplevel_mapping_manager_v1 上游 v1。
    pub const TOPLEVEL_MAPPING: (u32, u32) = (1, 1);
}

/// 私有协议派发状态：本层不消费任何协议事件（截图帧/快捷键按下等事件
/// 归 T3b / T2 功能模块任务）。
#[derive(Debug)]
pub struct HyprlandState;

macro_rules! inert_dispatch {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for HyprlandState {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

inert_dispatch!(HyprlandToplevelExportManagerV1);
inert_dispatch!(HyprlandFocusGrabManagerV1);
inert_dispatch!(HyprlandGlobalShortcutsManagerV1);
inert_dispatch!(HyprlandToplevelMappingManagerV1);

impl Dispatch<WlRegistry, wayland_client::globals::GlobalListContents> for HyprlandState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// hyprland_* 私有协议绑定集合（§9.4 表；全字段可选，回退语义）。
#[derive(Debug, Default)]
pub struct HyprlandBindings {
    /// 私有协议派发队列（probe 时创建，组件生命周期内保活）。
    ///
    /// Option 仅服务测试构造（doctor 不读队列）；生产路径恒 `Some`。
    queue: std::sync::Mutex<Option<wayland_client::EventQueue<HyprlandState>>>,
    /// 窗口级内容捕获（截图首选通道，操作选择矩阵 §9.5「截图」行）。
    pub toplevel_export: Option<HyprlandToplevelExportManagerV1>,
    /// 输入焦点白名单限制。
    pub focus_grab: Option<HyprlandFocusGrabManagerV1>,
    /// 全局快捷键注册。
    pub global_shortcuts: Option<HyprlandGlobalShortcutsManagerV1>,
    /// foreign-toplevel handle → 窗口地址映射（wlr 与 hyprctl 两套 id 的桥）。
    pub toplevel_mapping: Option<HyprlandToplevelMappingManagerV1>,
    /// 绑定失败明细（doctor 报告用）。
    pub bind_failures: Vec<(&'static str, String)>,
}

impl HyprlandBindings {
    /// 在既有显示服务器上探测并绑定全部 hyprland_* globals。
    ///
    /// 由 [`super::HyprlandCompositor::connect`] 调用；wlr 标准协议由基类
    /// `WlrWaylandCompositor::connect_with` 先行绑定，本函数只叠加私有层。
    pub fn probe(wl: &WaylandDisplayServer) -> Result<Self> {
        let globals = wl.globals();
        let mut queue = wl.connection().new_event_queue::<HyprlandState>();
        let qh = &queue.handle();
        let mut bindings = Self::default();

        macro_rules! bind_protocol {
            ($field:ident, $iface:literal, $ty:ty, $range:expr) => {{
                match globals.bind::<$ty, HyprlandState, _>(qh, $range, ()) {
                    Ok(proxy) => bindings.$field = Some(proxy),
                    Err(e) => bindings.bind_failures.push(($iface, e.to_string())),
                }
            }};
        }

        bind_protocol!(
            toplevel_export,
            "hyprland_toplevel_export_manager_v1",
            HyprlandToplevelExportManagerV1,
            protocol_versions::TOPLEVEL_EXPORT.0..=protocol_versions::TOPLEVEL_EXPORT.1
        );
        bind_protocol!(
            focus_grab,
            "hyprland_focus_grab_manager_v1",
            HyprlandFocusGrabManagerV1,
            protocol_versions::FOCUS_GRAB.0..=protocol_versions::FOCUS_GRAB.1
        );
        bind_protocol!(
            global_shortcuts,
            "hyprland_global_shortcuts_manager_v1",
            HyprlandGlobalShortcutsManagerV1,
            protocol_versions::GLOBAL_SHORTCUTS.0..=protocol_versions::GLOBAL_SHORTCUTS.1
        );
        bind_protocol!(
            toplevel_mapping,
            "hyprland_toplevel_mapping_manager_v1",
            HyprlandToplevelMappingManagerV1,
            protocol_versions::TOPLEVEL_MAPPING.0..=protocol_versions::TOPLEVEL_MAPPING.1
        );

        // 冲刷初始 bind 序列并等待 compositor 确认（错误对象在此浮现）。
        queue.roundtrip(&mut HyprlandState).map_err(|e| {
            agent_shell_core::error::AgentShellError::BackendUnavailable(format!(
                "hyprland roundtrip: {e}"
            ))
        })?;

        bindings.queue = std::sync::Mutex::new(Some(queue));
        Ok(bindings)
    }

    /// 冲刷新发出的请求到 compositor（测试态队列缺席时为 no-op）。
    pub fn flush_queue(&self) -> Result<()> {
        if let Some(queue) = self.queue.lock().expect("hyprland queue poisoned").as_mut() {
            queue.roundtrip(&mut HyprlandState).map_err(|e| {
                agent_shell_core::error::AgentShellError::BackendUnavailable(format!(
                    "hyprland roundtrip: {e}"
                ))
            })?;
        }
        Ok(())
    }

    /// 已绑定私有协议计数（doctor「N/M」的 N 的私有部分）。
    pub fn bound_count(&self) -> usize {
        usize::from(self.toplevel_export.is_some())
            + usize::from(self.focus_grab.is_some())
            + usize::from(self.global_shortcuts.is_some())
            + usize::from(self.toplevel_mapping.is_some())
    }
}

/// 基类复用校验：HyprlandCompositor 的 wlr 层就是
/// [`WlrWaylandCompositor`]（设计文档 fallback.md §11.1）——类型层面
/// 固化「组合基类 + 叠加私有通道」的继承表达。
///
/// 占位断言（不构造真实连接）：保证 wlr crate 作为公共依赖可见。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_protocol_version_ranges_within_interface_versions() {
        use wayland_client::Proxy as _;
        assert_eq!(
            protocol_versions::TOPLEVEL_EXPORT.1,
            HyprlandToplevelExportManagerV1::interface().version
        );
        assert_eq!(
            protocol_versions::FOCUS_GRAB.1,
            HyprlandFocusGrabManagerV1::interface().version
        );
        assert_eq!(
            protocol_versions::GLOBAL_SHORTCUTS.1,
            HyprlandGlobalShortcutsManagerV1::interface().version
        );
        assert_eq!(
            protocol_versions::TOPLEVEL_MAPPING.1,
            HyprlandToplevelMappingManagerV1::interface().version
        );
    }

    #[test]
    fn default_bindings_report_zero_bound() {
        let b = HyprlandBindings::default();
        assert_eq!(b.bound_count(), 0);
        assert!(b.bind_failures.is_empty());
        // flush_queue 对测试态（无队列）必须是无害 no-op。
        assert!(b.flush_queue().is_ok());
    }
}
