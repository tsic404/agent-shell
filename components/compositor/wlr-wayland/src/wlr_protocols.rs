//! wlr 标准协议绑定集合与版本协商常量。
//!
//! 对应设计文档 §1 `wlr_protocols.rs` / §5.2–5.4。

use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;

/// 各协议的本模块支持版本区间（§5.2「请求版本 = min(规范最高版本, 公布版本)」）。
///
/// 上界必须 ≤ wayland-scanner 生成的接口最高版本——`GlobalList::bind` 在
/// 取 min 之前先断言 `version.end() <= interface.version`，超界 panic。
pub mod protocol_versions {
    /// zwlr_foreign_toplevel_manager_v1 支持到 v3（fullscreen 状态 + parent 事件）。
    pub const FOREIGN_TOPLEVEL: (u32, u32) = (3, 3);
    /// zwlr_output_management_v1 支持到 v4。
    pub const OUTPUT_MANAGEMENT: (u32, u32) = (1, 4);
    /// zwlr_screencopy_manager_v1 支持到 v3（damage 事件）。
    pub const SCREENCOPY: (u32, u32) = (1, 3);
    /// zwlr_virtual_pointer_manager_v1 接口规范最高 v2，绑定区间固定 2..=2。
    pub const VIRTUAL_POINTER_MIN: u32 = 2;
    pub const VIRTUAL_POINTER_MAX: u32 = 2;
    /// ext_workspace_manager_v1 当前 v1（staging）。
    pub const EXT_WORKSPACE: (u32, u32) = (1, 1);
    /// zwp_virtual_keyboard_manager_v1 接口规范最高 v1（misc crate）。
    pub const VIRTUAL_KEYBOARD: (u32, u32) = (1, 1);
    /// ext_data_control_manager_v1 当前 v1（staging，设计文档 §5.2）。
    pub const DATA_CONTROL: (u32, u32) = (1, 1);
}

/// wlr 标准协议绑定集合（设计文档 §5.2）。
///
/// 每个字段独立可选：绑定失败不抛出异常，仅标记 `None`，
/// 后续操作由调用方检查 availability 并触发降级链（§5.4）：
///
/// ```text
/// foreign-toplevel 可用 → 窗口管理全功能（list/focus/minimize/close）
/// foreign-toplevel 缺失 → 降级 D-Bus / AT-SPI（screencopy/virtual-pointer 仍可原生）
/// 全部 wlr 协议缺失     → 全链路 portal 降级
/// ```
#[derive(Debug, Default)]
pub struct WlrBindings {
    /// 窗口管理全功能（list/focus/minimize/maximize/close）
    pub foreign_toplevel: Option<ZwlrForeignToplevelManagerV1>,
    /// 输出配置（监视器布局）
    pub output_management: Option<ZwlrOutputManagerV1>,
    /// 屏幕内容捕获
    pub screencopy: Option<ZwlrScreencopyManagerV1>,
    /// 虚拟指针输入注入
    pub virtual_pointer: Option<ZwlrVirtualPointerManagerV1>,
    /// 工作区列表与激活（ext-workspace staging 协议）
    pub ext_workspace: Option<ExtWorkspaceManagerV1>,
    /// 虚拟键盘输入注入（misc crate，zwp 命名空间）
    pub virtual_keyboard: Option<ZwpVirtualKeyboardManagerV1>,
    /// 剪贴板/主选区控制（ext-data-control staging 协议）
    pub data_control: Option<ExtDataControlManagerV1>,
    /// 绑定失败的协议名 → 错误描述（doctor 报告用）。
    pub bind_failures: Vec<(&'static str, String)>,
}

#[cfg(test)]
mod tests {
    use super::protocol_versions;
    use wayland_client::Proxy as _;

    #[test]
    fn protocol_version_ranges_never_exceed_interface_versions() {
        // 阻塞项 #1 回归：bind 在取 min 前断言 end <= interface.version，
        // 超界即 panic（wayland-client 0.31 globals.rs:167）。
        // 各接口规范最高版本：foreign-toplevel=3, output-management=4,
        // screencopy=3, virtual-pointer=2。
        assert!(
            protocol_versions::FOREIGN_TOPLEVEL.0 <= 3
                && protocol_versions::FOREIGN_TOPLEVEL.1 <= 3
        );
        assert!(
            protocol_versions::OUTPUT_MANAGEMENT.0 <= 4
                && protocol_versions::OUTPUT_MANAGEMENT.1 <= 4
        );
        assert!(protocol_versions::SCREENCOPY.0 <= 3 && protocol_versions::SCREENCOPY.1 <= 3);
        // 阻塞项 #1 回归：绑定区间必须落在接口版本内（virtual-pointer 接口最高 v2），
        assert_eq!(protocol_versions::VIRTUAL_POINTER_MIN, 2);
        assert_eq!(protocol_versions::VIRTUAL_POINTER_MAX, 2);
        assert_eq!(
            protocol_versions::VIRTUAL_POINTER_MAX,
            wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1::interface().version
        );
        // TSI-2350：新增三个协议字段的绑定区间同样不得越界。
        assert!(
            protocol_versions::EXT_WORKSPACE.0 >= 1
                && protocol_versions::EXT_WORKSPACE.1
                    <= wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1::interface().version
        );
        assert!(
            protocol_versions::VIRTUAL_KEYBOARD.0 >= 1
                && protocol_versions::VIRTUAL_KEYBOARD.1
                    <= wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1::interface().version
        );
        assert!(
            protocol_versions::DATA_CONTROL.0 >= 1
                && protocol_versions::DATA_CONTROL.1
                    <= wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1::interface().version
        );
    }
}
