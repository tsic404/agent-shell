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

    /// 操作需要用户交互确认（L3/L4 或确认覆盖命中）；纯后端阶段由
    /// router 短路返回，真正弹窗在 `daemon/safety.rs`。
    #[error("Confirmation required: {0}")]
    ConfirmationRequired(String),

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

/// polkit 授权类 D-Bus 错误名（`:` 分隔段全量比对）。
///
/// logind 与 systemd 共用：缺 polkit 授权（含交互授权请求）是环境状态而非
/// 组件缺陷，需统一归一为 [`AgentShellError::Permission`]，供降级链/测试判断。
const PERMISSION_ERROR_NAMES: &[&str] = &[
    "org.freedesktop.DBus.Error.AccessDenied",
    "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired",
    "org.freedesktop.DBus.Error.AuthenticationRequisite",
    "org.freedesktop.DBus.Error.UnixFD.AccessDenied",
];

/// D-Bus 错误归一化：权限类错误名 → [`AgentShellError::Permission`]，
/// 其余错误 → [`AgentShellError::DBus`]。
///
/// 错误名取自消息 `:` 分隔段——zbus 的 [`std::fmt::Display`] 渲染为
/// `<错误名>: <描述>`，调用点可能再加 `CanReboot:` 这类上下文前缀，
/// 故遍历全部 `:` 段与集合比对。
pub fn dbus_error<E: std::fmt::Display>(e: E) -> AgentShellError {
    let msg = e.to_string();
    let denied = msg
        .split(':')
        .map(str::trim)
        .any(|segment| PERMISSION_ERROR_NAMES.contains(&segment));
    if denied {
        AgentShellError::Permission(msg)
    } else {
        AgentShellError::DBus(msg)
    }
}

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

    #[test]
    fn dbus_error_maps_permission_variants() {
        for msg in [
            "CanReboot: org.freedesktop.DBus.Error.AccessDenied: Permission denied",
            "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired: Access denied as the requested operation requires interactive authentication",
            "CanPowerOff: org.freedesktop.DBus.Error.AuthenticationRequisite: Authentication is required",
            "org.freedesktop.DBus.Error.UnixFD.AccessDenied: fd passing denied",
        ] {
            assert!(
                matches!(dbus_error(msg), AgentShellError::Permission(_)),
                "expected Permission for {msg:?}"
            );
        }
    }

    #[test]
    fn dbus_error_keeps_other_messages_as_dbus() {
        for msg in [
            "list sessions failed",
            "CanReboot: Access denied as the requested operation requires interactive authentication",
            "CanReboot: org.freedesktop.DBus.Error.AccessDeniedEvil: Permission denied",
        ] {
            assert!(
                matches!(dbus_error(msg), AgentShellError::DBus(_)),
                "expected DBus for {msg:?}"
            );
        }
    }
}
