//! Mutter 合成器特有错误类型（设计文档 §8.4 `error.rs`）。
//!
//! 在统一错误 [`AgentShellError`] 之上补充 GNOME 双通道语义：Eval 被禁用、
//! Extension 未安装/未注册、DisplayConfig 序列号过期等场景需要可区分的
//! 错误信息，供双路径降级链（Eval → Extension）与 doctor 报告使用。

use agent_shell_core::error::AgentShellError;

/// org.gnome.Shell D-Bus 服务常量（§3.5 调研记录）。
pub const SHELL_SERVICE: &str = "org.gnome.Shell";
/// org.gnome.Shell 根对象路径（Eval / ShellVersion 所在）。
pub const SHELL_PATH: &str = "/org/gnome/Shell";
/// Shell Extension 注册接口（extension.js 侧实现）。
pub const AGENTSHELL_IFACE: &str = "org.gnome.Shell.AgentShell";
/// Shell Extension 对象路径（extension.js 侧导出）。
pub const AGENTSHELL_PATH: &str = "/org/gnome/Shell/AgentShell";
/// Shell Extension 唯一标识。
pub const EXTENSION_ID: &str = "agent-shell-bridge@multica.dev";
/// Mutter DisplayConfig 接口所在服务。
pub const DISPLAY_CONFIG_SERVICE: &str = "org.gnome.Mutter.DisplayConfig";
/// DisplayConfig 对象路径。
pub const DISPLAY_CONFIG_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

/// GNOME Eval 允许开关（gsettings key，GNOME 41+ 默认 false）。
///
/// `gsettings set org.gnome.shell developer-tools true` 打开；本 crate
/// 不代改用户设置——探测失败时如实报错并走降级链。
pub const EVAL_DISABLED_HINT: &str = "org.gnome.Shell.Eval unavailable (enable via: gsettings set \
     org.gnome.shell developer-tools true)";

/// Mutter 特有错误。
#[derive(Debug, thiserror::Error)]
pub enum MutterError {
    /// Eval 被禁用或调用失败（success=false / 服务拒绝 / unsafe-mode 限制）。
    #[error("gnome eval failed: {0}")]
    Eval(String),

    /// Shell Extension 未安装或 `org.gnome.Shell.AgentShell` 未注册。
    #[error("shell extension not available ({EXTENSION_ID}): {0}")]
    Extension(String),

    /// Wayland 协议通道连接失败（wl_display connect / registry 初始化）。
    #[error("wayland connect failed: {0}")]
    Wayland(String),

    /// DisplayConfig 调用失败（不可达 / 序列号过期 InvalidArgs）。
    #[error("display config error: {0}")]
    DisplayConfig(String),

    /// 版本探测失败（ShellVersion 缺失或格式不识别）。
    #[error("gnome version probe failed: {0}")]
    Version(String),
}

impl From<MutterError> for AgentShellError {
    fn from(e: MutterError) -> Self {
        match e {
            MutterError::Eval(msg) => AgentShellError::DBus(format!("gnome eval: {msg}")),
            MutterError::Extension(msg) => AgentShellError::BackendUnavailable(format!(
                "gnome extension {EXTENSION_ID}: {msg}"
            )),
            MutterError::DisplayConfig(msg) => {
                AgentShellError::DBus(format!("mutter display config: {msg}"))
            }
            MutterError::Wayland(msg) => {
                AgentShellError::BackendUnavailable(format!("mutter wayland: {msg}"))
            }
            MutterError::Version(msg) => {
                AgentShellError::BackendUnavailable(format!("gnome version: {msg}"))
            }
        }
    }
}

/// Mutter crate 内部 Result 别名。
pub type Result<T, E = MutterError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_error_maps_to_dbus_with_hint_context() {
        let e = AgentShellError::from(MutterError::Eval("success=false".into()));
        assert!(matches!(e, AgentShellError::DBus(_)));
        let s = e.to_string();
        // 提示文案必须可从最终错误链看到——运维据此自助开启 Eval。
        assert!(s.contains("eval"), "missing context in {s}");
    }

    #[test]
    fn extension_error_maps_to_backend_unavailable() {
        let e = AgentShellError::from(MutterError::Extension("interface not on bus".into()));
        assert!(matches!(e, AgentShellError::BackendUnavailable(_)));
        assert!(e.to_string().contains(EXTENSION_ID));
    }

    #[test]
    fn display_config_error_maps_to_dbus() {
        let e = AgentShellError::from(MutterError::DisplayConfig("stale serial".into()));
        assert!(matches!(e, AgentShellError::DBus(_)));
        assert!(e.to_string().contains("stale serial"));
    }

    #[test]
    fn version_error_maps_to_backend_unavailable() {
        let e = AgentShellError::from(MutterError::Version("no ShellVersion".into()));
        assert!(matches!(e, AgentShellError::BackendUnavailable(_)));
    }

    #[test]
    fn wayland_error_maps_to_backend_unavailable() {
        let e = AgentShellError::from(MutterError::Wayland("NoCompositor".into()));
        assert!(matches!(e, AgentShellError::BackendUnavailable(_)));
        assert!(e.to_string().contains("mutter wayland"));
    }
}
