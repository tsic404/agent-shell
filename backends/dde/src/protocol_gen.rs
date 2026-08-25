//! treeland_* 私有协议生成绑定（设计文档 §10.3，`treeland.rs` 的协议层）。
//!
//! XML 取自上游 `linuxdeepin/treeland-protocols`（MIT，vendored 于
//! `protocols/`），经 `wayland-scanner` 过程宏在编译期生成客户端绑定——
//! 与 `wayland-protocols-plasma`/`wayland-protocols-wlr` 对官方协议的
//! 处理方式一致，但 treeland 协议未收录进这两个 crate，只能本地生成。
//!
//! ⚠️ 上游声明（XML description 原文）：treeland 协议为 **EXPERIMENTAL**，
//! 接口名/请求/事件可能在不升 major version 的情况下变更。本模块只绑定
//! 设计文档 §10.3 点名的两个通道：
//!
//! | 接口 | 版本 | 用途 |
//! |------|:----:|------|
//! | `treeland_foreign_toplevel_manager_v1` | 2 | 窗口管理：activate / close / set(unset)_maximized / set(unset)_minimized / set_fullscreen / set_rectangle |
//! | `treeland_window_management_v1` | 1 | 桌面状态：normal / show / preview_show |
//!
//! 生成代码的请求方法**不带 QueueHandle**（scanner 0.31 风格：请求无
//! 回执事件时不要求句柄）；事件消费由 `treeland.rs` 的派发状态实现。

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
