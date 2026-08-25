//! `WaylandCompositor` 抽象基类 trait（设计文档 §3.3 继承层次 / §5）。
//!
//! Rust 无实现继承：基类以「supertrait + 组合访问器」表达——所有 Wayland 系
//! 合成器实现本 trait，即自动满足 `CompositorComponent`，并通过
//! [`WaylandCompositor::display_server`] 暴露共享的协议通道。
//!
//! ```text
//! CompositorComponent（trait）
//! └── WaylandCompositor（本层——抽象基类，组合 WaylandDisplayServer）
//!     ├── WlrWaylandCompositor（components/compositor/wlr-wayland，叠加 wlr 标准协议）
//!     │   └── TreelandCompositor / HyprlandCompositor / SwayCompositor
//!     ├── KWinCompositor（org_kde_* 私有协议）
//!     └── MutterCompositor（D-Bus Eval/Extension）
//! ```

use async_trait::async_trait;

use crate::WaylandDisplayServer;
use agent_shell_core::component::CompositorComponent;

/// Wayland 系合成器的中间抽象层（§3.3：组合 `WaylandDisplayServer`）。
///
/// 只约束纯 Wayland core 语义：`wl_display` 连接生命周期、registry 遍历、
/// global 接口探测。**不涉及任何 wlr 协议**——wlr 标准协议绑定属于
/// `WlrWaylandCompositor`，各 DE 私有协议属于具体合成器。
#[async_trait]
pub trait WaylandCompositor: CompositorComponent {
    /// 共享的 Wayland 显示服务器协议通道（同一 `wl_display`）。
    ///
    /// 子类叠加私有协议时复用其 connection/globals；doctor 输出据此报告
    /// 「协议通道基础」一节（§5.5 验证输出）。
    fn display_server(&self) -> &WaylandDisplayServer;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编译期契约检查：任一 `WaylandCompositor` 实现必然也是
    /// `CompositorComponent`（继承层次表第一行）。占位类型只参与
    /// trait-bound 断言，不构造显示服务器连接。
    #[test]
    fn wayland_compositor_requires_compositor_component() {
        trait Assert<T: CompositorComponent> {}
        impl<T: WaylandCompositor> Assert<T> for T {}
        // 若 WaylandCompositor 不再是 CompositorComponent 的子 trait，
        // 上行 impl 将编译失败——这正是要守住的契约。
    }
}
