//! 路由层对输入、截图、AT-SPI 元素动作、语义定位四个依赖的薄封装。
//!
//! T3d 阶段以 trait object + 可注入 mock 的形式落地接口边界；
//! 具体实现由 T2 功能模块（input / capture / a11y）提供。

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::SemanticTarget;
use async_trait::async_trait;

/// AT-SPI 元素句柄（路由层视角的最小形状）。
///
/// 具体元素模型由 a11y crate 提供；此处仅暴露 Executor 所需的
/// 文本写入与向上找窗口两个动作。
#[async_trait]
pub trait A11yElement: Send + Sync {
    /// 设置元素文本（Set 接口，Text 角色）。
    async fn set_text(&self, text: &str) -> Result<()>;

    /// 读取元素文本。
    async fn text(&self) -> Result<String>;

    /// 找到元素所属的窗口名（沿祖先链上溯到 role == "frame"/"window"）。
    async fn find_parent_window_name(&self) -> Result<String>;
}

/// AT-SPI 元素动作 + 语义定位封装。
#[async_trait]
pub trait ElementActions: Send + Sync {
    /// 按语义目标定位元素，命中顺序即语义优先级（§15.3）。
    async fn locate(&self, target: &SemanticTarget)
        -> Result<Vec<std::sync::Arc<dyn A11yElement>>>;
}

/// 输入注入封装（libei → ydotool → XTest 降级链在 T2 内部完成，路由层只见此接口）。
#[async_trait]
pub trait InputDispatcher: Send + Sync {
    /// 键盘模拟输入文本，`delay_us` 为键间延迟（微秒）。
    async fn type_text(&self, text: &str, delay_us: u64) -> Result<()>;

    /// 发送按键组合。
    async fn send_key_combo(&self, keys: &[agent_shell_core::types::Key]) -> Result<()>;

    /// 移动鼠标到绝对坐标。
    async fn mouse_move(&self, x: i32, y: i32) -> Result<()>;

    /// 点击鼠标按键。
    async fn mouse_click(&self, button: agent_shell_core::types::MouseButton) -> Result<()>;

    /// 滚动。
    async fn scroll(&self, dx: i32, dy: i32) -> Result<()>;
}

/// 截图捕获封装（ScreenCast → portal → X11 降级链在 T2 内部完成）。
#[async_trait]
pub trait CaptureDispatcher: Send + Sync {
    /// 按 PNG 编码字节流返回指定目标的截图。
    ///
    /// `output` 仅作为日志/审计提示传递；文件落盘由 Executor 负责，
    /// 保持「捕获」与「写盘」职责分离。
    async fn capture_png(&self, target: &agent_shell_core::types::CaptureTarget)
        -> Result<Vec<u8>>;
}

/// 语义定位失败时的统一错误构造。
pub(crate) fn not_implemented(what: &str) -> AgentShellError {
    AgentShellError::NotImplemented(format!("router: {what}"))
}
