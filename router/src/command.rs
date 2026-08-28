//! 统一命令模型（design/07 §15.1）。
//!
//! `Command` 是 CLI/daemon 与各组件之间唯一的命令表达形式；路由层
//! [`Executor`](crate::executor::Executor) 把它翻译为对
//! CompositorComponent / Input / Capture / A11y 的具体调用。

use std::time::Duration;

use agent_shell_core::security::{Operation, PermissionLevel};
use agent_shell_core::types::{
    CaptureTarget, KeyCombo, MouseButton, Rect, SemanticTarget, WindowFilter, WorkspaceId,
};

/// 路由层统一命令枚举——21 个变体逐一对应 design/07 §15.1。
#[derive(Clone, Debug)]
pub enum Command {
    // ── 窗口 ──
    /// 列出窗口（可选过滤）。
    ListWindows { filter: Option<WindowFilter> },
    /// 获取当前活动窗口。
    GetActiveWindow,
    /// 聚焦语义目标对应的窗口。
    FocusWindow { target: SemanticTarget },
    /// 移动窗口到指定坐标。
    MoveWindow {
        target: SemanticTarget,
        x: i32,
        y: i32,
    },
    /// 缩放窗口到指定尺寸。
    ResizeWindow {
        target: SemanticTarget,
        w: i32,
        h: i32,
    },
    /// 最小化窗口。
    MinimizeWindow { target: SemanticTarget },
    /// 关闭窗口。
    CloseWindow { target: SemanticTarget },
    /// 一次设定窗口几何。
    SetWindowGeometry { target: SemanticTarget, rect: Rect },

    // ── 工作区 ──
    /// 列出工作区。
    ListWorkspaces,
    /// 切换到指定工作区。
    SwitchWorkspace { id: WorkspaceId },
    /// 将窗口移动到工作区。
    MoveWindowToWorkspace {
        window: SemanticTarget,
        workspace: WorkspaceId,
    },

    // ── 输入 ──
    /// 发送按键组合。
    SendKey { combo: KeyCombo },
    /// 输入文本；带目标时优先 AT-SPI 语义输入，失败降级键盘模拟。
    TypeText {
        text: String,
        target: Option<SemanticTarget>,
    },
    /// 鼠标点击；带目标时先解析目标再点击其中心。
    MouseClick {
        button: MouseButton,
        target: Option<SemanticTarget>,
    },
    /// 移动鼠标到绝对坐标。
    MouseMove { x: i32, y: i32 },
    /// 滚动。
    Scroll { dx: i32, dy: i32 },

    // ── 截图 ──
    /// 截图并写入文件。
    Screenshot {
        target: CaptureTarget,
        output: Option<String>,
    },

    // ── 无障碍 ──
    /// 读取元素文本。
    GetElementText { target: SemanticTarget },
    /// 读取无障碍树。
    GetA11yTree { target: Option<SemanticTarget> },

    // ── 等待 ──
    /// 等待 app_id 对应的窗口出现（轮询实现，事件流就绪后切换订阅优先）。
    WaitForWindow { app_id: String, timeout: Duration },
    /// 等待文本出现（依赖 A11y 树查询，轮询实现）。
    WaitForText { text: String, timeout: Duration },

    // ── 系统 ──
    /// 获取桌面环境信息。
    GetDesktopInfo,
    /// 获取后端组件状态。
    GetBackendStatus,
}

impl Command {
    /// 本命令对应的权限操作（§22.7 D6：全部命令必经 SecurityManager）。
    ///
    /// 分级按 §21.21.1：L0 只读 / L1 低风险 / L2 中风险 / L3 高风险 /
    /// L4 系统级。窗口类写操作聚焦/移动/缩放/最小化/切换均为可逆小影响
    /// → L1；CloseWindow 关窗口数据丢失不可逆 → L2；截图内容敏感非只读
    /// → L2（与 daemon 权威 gate 一致）；无障碍读/等待轮询只读 → L0；
    /// 输入注入影响其它应用 → L1。
    pub fn operation(&self) -> Operation {
        use PermissionLevel::*;
        match self {
            Self::ListWindows { .. } => Operation::new("windows.list", L0),
            Self::GetActiveWindow => Operation::new("windows.info", L0),
            Self::FocusWindow { .. } => Operation::new("windows.focus", L1),
            Self::MoveWindow { .. } => Operation::new("windows.move", L1),
            Self::ResizeWindow { .. } => Operation::new("windows.resize", L1),
            Self::MinimizeWindow { .. } => Operation::new("windows.minimize", L1),
            Self::CloseWindow { .. } => Operation::new("windows.close", L2),
            Self::SetWindowGeometry { .. } => Operation::new("windows.geometry", L1),
            Self::ListWorkspaces => Operation::new("workspaces.list", L0),
            Self::SwitchWorkspace { .. } => Operation::new("workspaces.switch", L1),
            Self::MoveWindowToWorkspace { .. } => Operation::new("workspaces.move_window", L1),
            Self::SendKey { .. } => Operation::new("input.send", L1),
            Self::TypeText { .. } => Operation::new("input.type", L1),
            Self::MouseClick { .. } => Operation::new("input.click", L1),
            Self::MouseMove { .. } => Operation::new("input.move", L1),
            Self::Scroll { .. } => Operation::new("input.scroll", L1),
            Self::Screenshot { .. } => Operation::new("screenshot.capture", L2),
            Self::GetElementText { .. } => Operation::new("a11y.element_text", L0),
            Self::GetA11yTree { .. } => Operation::new("a11y.tree", L0),
            Self::WaitForWindow { .. } => Operation::new("wait.window", L0),
            Self::WaitForText { .. } => Operation::new("wait.text", L0),
            Self::GetDesktopInfo => Operation::new("system.desktop_info", L0),
            Self::GetBackendStatus => Operation::new("system.backend_status", L0),
        }
    }
}

/// 命令执行结果。
///
/// 覆盖执行引擎用到的三种形态：窗口查询结果、通用成功、落盘文件路径。
/// 设计文档未展开此类型，按最小必要定义（§15.1 备注），不得发明多余变体。
#[derive(Clone, Debug)]
pub enum CommandResult {
    /// 窗口类命令的返回值。
    Window(Box<agent_shell_core::types::WindowInfo>),
    /// 无具体载荷的成功。
    Success,
    /// 落盘文件路径（如截图输出）。
    File(String),
}
