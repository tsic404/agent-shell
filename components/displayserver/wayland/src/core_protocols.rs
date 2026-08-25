//! 纯 Wayland core 的协议版本常量与绑定集合占位。
//!
//! wlr 标准协议（foreign-toplevel / screencopy / virtual-pointer /
//! output-management）的绑定已下移至
//! `agent-shell-compositor-wlr-wayland`（`components/compositor/wlr-wayland/`）。
//! 本层只保留 `wl_*` 核心 接口所需的版本信息。

/// 纯 core 接口的版本区间（§5.2「请求版本 = min(规范最高版本, 公布版本)」）。
pub mod protocol_versions {
    /// wl_seat 仅作 activate 等请求参数，绑最低 v1。
    pub const SEAT: u32 = 1;
}

/// 纯 Wayland core 层的绑定集合。
///
/// 当前没有本层负责的协议对象（registry 快照即全部产物）；保留结构体以
/// 维持 doctor 报告接口与子类回填路径稳定——DE 私有协议合成器可经
/// [`WaylandDisplayServer::bindings_mut`](crate::WaylandDisplayServer::bindings_mut)
/// 存放共享状态。
#[derive(Debug, Default)]
pub struct WaylandCoreBindings {
    /// 预留：纯 core 协议对象（当前为空）。
    #[allow(dead_code)]
    reserved: (),
}

#[cfg(test)]
mod tests {
    use super::protocol_versions;

    #[test]
    fn seat_version_within_core_spec() {
        // wl_seat 当前规范最高 v9；绑最低 v1 永远可行。
        const { assert!(protocol_versions::SEAT >= 1) }
    }
}
