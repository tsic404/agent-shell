//! 输入组件（设计文档 §12 输入子系统、§3.1 输入服务行）。
//!
//! [`InputDispatcher`] 按降级链探测并选择注入后端：
//!
//! ```text
//! 首选 libei   ──► libei (EIS)  ──► compositor (Wayland 原生)
//! 降级 ydotool ──► ydotool      ──► /dev/uinput ──► 内核
//! X11 原生     ──► XTest 扩展   ──► x11rb::xtest_fake_input（仅原生 X11 会话）
//! 再降级       ──► xdotool      ──► XTest（仅保底）
//! ```
//!
//! 装配（§4.2）：`let input = InputComponentHandle::detect(de).await?;`
//! 结果放入 `ComponentRegistry.input`。TTY 环境探测不到任何后端时
//! dispatcher `active = None`，对外方法返回 `BackendUnavailable` 错误而非 panic。
mod dispatcher;
pub(crate) mod keymap;
mod libei;
mod xdotool;
mod xtest;
mod ydotool;

pub use dispatcher::{InputDispatcher, InputService};
pub use libei::LibeiInput;
pub use xdotool::XdotoolInput;
pub use xtest::XTestInput;
pub use ydotool::YdotoolInput;

use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, InputComponent,
};
use agent_shell_core::error::Result;
use agent_shell_core::types::DesktopEnvironment;
use async_trait::async_trait;

/// `ComponentRegistry.input` 的具体实现：dispatcher + `DesktopComponent` 契约。
///
/// backend 装配时调用 [`InputComponentHandle::detect`]；`active == None`
/// （如 TTY）时 `is_available() == false`，registry 存 `None` 即可，
/// 因此本类型只在探测出至少一个候选后端时才构造成功。
pub struct InputComponentHandle {
    dispatcher: InputDispatcher,
    de_type: DesktopEnvironment,
}

impl InputComponentHandle {
    /// 按 DE 探测降级链并选出 active 后端。
    ///
    /// 全部后端不可用（如 TTY）返回 `Err(BackendUnavailable)`——与设计一致：
    /// registry 对 TTY 会话存 `None`，不构造本类型。
    pub async fn detect(de_type: DesktopEnvironment) -> Result<Self> {
        let dispatcher = InputDispatcher::new(de_type).await?;
        if dispatcher.active_backend_name().is_none() {
            return Err(
                agent_shell_core::error::AgentShellError::BackendUnavailable(format!(
                    "input: no usable input backend in {de_type} session"
                )),
            );
        }
        Ok(Self {
            dispatcher,
            de_type,
        })
    }

    /// 当前激活的注入后端名（doctor / 日志用）。
    pub fn backend_name(&self) -> Option<&'static str> {
        self.dispatcher.active_backend_name()
    }

    /// 统一注入入口。
    pub fn dispatcher(&self) -> &InputDispatcher {
        &self.dispatcher
    }

    /// 装配时传入的桌面环境（诊断用）。
    pub fn desktop_environment(&self) -> DesktopEnvironment {
        self.de_type
    }
}

#[async_trait]
impl DesktopComponent for InputComponentHandle {
    fn name(&self) -> &'static str {
        "input-dispatcher"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Input
    }

    fn is_available(&self) -> bool {
        self.dispatcher.active_backend_name().is_some()
    }

    async fn health(&self) -> ComponentHealth {
        match self.dispatcher.active_backend_name() {
            Some(name) => match self.dispatcher.active_health().await {
                ComponentHealth::Healthy => ComponentHealth::Healthy,
                ComponentHealth::Degraded(reason) => {
                    ComponentHealth::Degraded(format!("{name}: {reason}"))
                }
                ComponentHealth::Unavailable => ComponentHealth::Degraded(format!(
                    "{name}: selected but unavailable (stale probe?)"
                )),
            },
            None => ComponentHealth::Unavailable,
        }
    }
}

#[async_trait]
impl InputComponent for InputComponentHandle {
    async fn active_backend(&self) -> Result<&'static str> {
        self.dispatcher.active_backend_name().ok_or_else(|| {
            agent_shell_core::error::AgentShellError::BackendUnavailable(
                "input: no active backend".into(),
            )
        })
    }
}

/// 装配入口别名（§4.2 `InputComponent::detect(self.session_type)` 语义）。
///
/// 命名对齐 core 契约注释中的调用形态，避免与 trait `InputComponent` 混淆。
pub async fn detect(de_type: DesktopEnvironment) -> Result<InputComponentHandle> {
    InputComponentHandle::detect(de_type).await
}
