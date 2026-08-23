//! 统一错误类型。
//!
//! 对应设计文档 `design/01-core-types/` §2 与 `design/11-error-handling/` §19：
//! 所有模块共享 `AgentShellError`，降级链据此判断是否回退到下一级实现。

/// agent-shell 统一错误。
#[derive(Debug, thiserror::Error)]
pub enum AgentShellError {
    /// 桌面环境不受支持（检测结果为 Unknown 或未实现的后端）。
    #[error("DE not supported: {0}")]
    UnsupportedDE(String),

    /// 后端不可用（如 Wayland 会话下无 X11 通道）。
    #[error("Backend not available: {0}")]
    BackendUnavailable(String),

    /// 窗口未找到（按 id/标题/语义目标定位失败）。
    #[error("Window not found: {0}")]
    WindowNotFound(String),

    /// D-Bus 调用错误（session/system bus）。
    ///
    /// core 保持协议无关：携带归一化描述字符串；具体传输层（zbus）在
    /// 未来 `components/dbus` crate 中通过 `.map_err(|e| AgentShellError::DBus(e.to_string()))`
    /// 或 `#[from] zbus::Error` 的下游包装转换到此变体。
    #[error("D-Bus error: {0}")]
    DBus(String),

    /// 输入后端错误（libei/ydotool/XTest 注入失败）。
    #[error("Input backend error: {0}")]
    Input(String),

    /// 截图/捕获错误（portal ScreenCast/MIT-SHM 失败）。
    #[error("Capture error: {0}")]
    Capture(String),

    /// 权限不足（polkit 拒绝、portal 用户取消授权等）。
    #[error("Permission denied: {0}")]
    Permission(String),

    /// 操作超时（等待窗口出现/portal 应答超时）。
    #[error("Timeout: {0}")]
    Timeout(String),

    /// 功能未实现（trait 预留方法、当前后端不支持的能力）。
    #[error("Not implemented: {0}")]
    NotImplemented(String),

    /// 其它错误（透传底层错误链）。
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// 便捷别名：所有可能失败的组件方法统一返回此类型。
pub type Result<T, E = AgentShellError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_readable() {
        let e = AgentShellError::WindowNotFound("firefox".into());
        assert_eq!(e.to_string(), "Window not found: firefox");

        let e = AgentShellError::UnsupportedDE("Unknown".into());
        assert_eq!(e.to_string(), "DE not supported: Unknown");
    }

    #[test]
    fn dbus_error_carries_description() {
        let e = AgentShellError::DBus("service unknown: org.example.Missing".into());
        assert_eq!(
            e.to_string(),
            "D-Bus error: service unknown: org.example.Missing"
        );
        assert!(matches!(e, AgentShellError::DBus(_)));
    }

    #[test]
    fn boxed_error_converts_via_from() {
        let inner: Box<dyn std::error::Error + Send + Sync> = String::from("boom").into();
        let e = AgentShellError::from(inner);
        assert_eq!(e.to_string(), "boom");
    }
}
