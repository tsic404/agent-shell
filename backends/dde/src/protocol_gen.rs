//! treeland_* 私有协议生成绑定（设计文档 §10.3，`treeland.rs` 的协议层）。
//!
//! XML vendored 自 `linuxdeepin/treeland-protocols`（MIT，`protocols/`），经
//! `wayland-scanner` 过程宏编译期生成客户端绑定；treeland 协议未收录进任何
//! wayland-protocols-* crate，只能本地生成。
//!
//! 上游声明协议为 EXPERIMENTAL（接口可能不升 major 就变更），故只绑定 §10.3
//! 点名的两个通道；生成请求不带 QueueHandle，事件由 `treeland.rs` 派发状态消费。

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
    // 与 wayland-protocols-* 的 protocol_macro! 一致）。
    use wayland_client;
    use wayland_client::protocol::*;

    // 两个协议各占一个接口子模块——generate_interfaces! 展开出的
    // SyncWrapper/types_null 是模块级项，同模块重复调用会重定义冲突。
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!(
            "./protocols/treeland-foreign-toplevel-manager-v1.xml"
        );
    }
    pub mod wm_interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/treeland-window-management-v1.xml");
    }
    pub use self::__interfaces::*;
    pub use self::wm_interfaces::*;

    wayland_scanner::generate_client_code!("./protocols/treeland-foreign-toplevel-manager-v1.xml");
    wayland_scanner::generate_client_code!("./protocols/treeland-window-management-v1.xml");
}

pub use client::{
    treeland_foreign_toplevel_handle_v1, treeland_foreign_toplevel_manager_v1,
    treeland_window_management_v1,
};
