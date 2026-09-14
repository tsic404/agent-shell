//! `WaylandCompositor` 抽象基类 trait（设计文档 §3.3 继承层次 / §5）。
//!
//! Rust 无实现继承：基类以「supertrait + 组合访问器」表达——所有 Wayland 系合成器
//! 实现本 trait 即自动满足 CompositorComponent，并通过
//! [`WaylandCompositor::display_server`] 暴露共享协议通道。继承树（WlrWayland /
//! KWin / Mutter / Treeland / Hyprland / Sway）见 §3.3。

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
    /// trait-bound 求解，不构造显示服务器连接。
    ///
    /// 若 `WaylandCompositor` 不再是 `CompositorComponent` 的子 trait，
    /// 下方 `_requires_compositor_component::<T>()` 无法通过类型检查——
    /// 这正是要守住的契约。
    #[allow(dead_code)]
    fn _assert<T: WaylandCompositor>() {
        fn _requires_compositor_component<T: CompositorComponent>() {}
        _requires_compositor_component::<T>();
    }
}
