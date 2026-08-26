//! hyprland_* 私有协议生成绑定（设计文档 §9.4，`wayland.rs` 的协议层）。
//!
//! XML 取自上游 `hyprwm/hyprland-protocols`（MIT，vendored 于 `protocols/`），
//! 经 `wayland-scanner` 过程宏在编译期生成客户端绑定——与
//! `wayland-protocols-wlr` 对官方协议的处理方式一致，但 hyprland 协议未
//! 收录进任何 wayland-protocols-* crate，只能本地生成。
//!
//! 绑定设计文档 §9.4 点名的四个通道：
//!
//! | 接口 | 版本 | 用途 |
//! |------|:----:|------|
//! | `hyprland_toplevel_export_manager_v1` | 2 | 窗口级内容捕获 |
//! | `hyprland_focus_grab_manager_v1` | 1 | 输入焦点白名单限制 |
//! | `hyprland_global_shortcuts_manager_v1` | 1 | 全局快捷键注册 |
//! | `hyprland_toplevel_mapping_manager_v1` | 1 | toplevel → 窗口地址映射 |

#[allow(
    dead_code,
    non_camel_case_types,
    unused_variables,
    unused_unsafe,
    unused_imports
)]
pub mod client {
    // `use wayland_client;` 并非冗余：generate_client_code! 展开体以
    // `::wayland_client` 之外的裸路径 `wayland_client::…` 引用 crate，
    // 需要 mod 内的显式导入把名字带入作用域（scanner 宏的既知形态，
    // 与 wayland-protocols-* 的 protocol_macro! 一致，见 dde/protocol_gen.rs）。
    use wayland_client;
    use wayland_client::protocol::*;

    // hyprland 协议引用了外部接口（zwlr_/ext_foreign_toplevel_handle_v1）：
    // scanner 生成的接口常量与模块路径要求这些外部接口在作用域可见。
    pub use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1;
    pub use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1;

    pub mod export_interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/hyprland-toplevel-export-v1.xml");
    }
    pub mod focus_grab_interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/hyprland-focus-grab-v1.xml");
    }
    pub mod global_shortcuts_interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/hyprland-global-shortcuts-v1.xml");
    }
    pub mod toplevel_mapping_interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/hyprland-toplevel-mapping-v1.xml");
    }
    pub use self::{
        export_interfaces::*, focus_grab_interfaces::*, global_shortcuts_interfaces::*,
        toplevel_mapping_interfaces::*,
    };

    wayland_scanner::generate_client_code!("./protocols/hyprland-toplevel-export-v1.xml");
    wayland_scanner::generate_client_code!("./protocols/hyprland-focus-grab-v1.xml");
    wayland_scanner::generate_client_code!("./protocols/hyprland-global-shortcuts-v1.xml");
    wayland_scanner::generate_client_code!("./protocols/hyprland-toplevel-mapping-v1.xml");
}

pub use client::{
    hyprland_focus_grab_manager_v1, hyprland_global_shortcuts_manager_v1,
    hyprland_toplevel_export_frame_v1, hyprland_toplevel_export_manager_v1,
    hyprland_toplevel_mapping_manager_v1,
};
