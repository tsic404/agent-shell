//! KWin 合成器特有错误类型（设计文档 §7.7 `error.rs`）。
//!
//! 在统一错误 [`AgentShellError`] 之上补充 KWin 通道语义：Wayland 私有协议
//! 绑定失败、Scripting 桥接超时/回传失败等场景需要可区分的错误信息，
//! 供降级链（协议 → Scripting）与 doctor 报告使用。

use agent_shell_core::error::AgentShellError;

/// KWin 特有错误。
#[derive(Debug, thiserror::Error)]
pub enum KWinError {
    /// Wayland 私有协议绑定失败（接口未公布 / 版本过低 / 单客户端被占用）。
    #[error("KWin protocol bind failed ({interface}): {reason}")]
    ProtocolBind {
        /// 协议接口名（如 `org_kde_plasma_window_management`）。
        interface: &'static str,
        /// 失败原因描述。
        reason: String,
    },

    /// fake_input 未通过 authenticate 即注入（协议要求先声明用途）。
    #[error("fake_input not authenticated")]
    FakeInputNotAuthenticated,

    /// KWin Scripting D-Bus 调用失败（org.kde.KWin 不可达等）。
    #[error("KWin scripting error: {0}")]
    Scripting(String),

    /// Scripting 脚本执行后未在时限内收到 callDBus 回传。
    #[error("KWin script response timeout after {timeout_secs}s: {script}")]
    ResponseTimeout {
        /// 脚本标识（名称或内联脚本前缀）。
        script: String,
        /// 超时秒数。
        timeout_secs: u64,
    },

    /// 脚本回传结果不是合法 JSON（callDBus sendResult payload 解析失败）。
    #[error("KWin script returned invalid JSON: {0}")]
    InvalidScriptOutput(String),
}

impl From<AgentShellError> for KWinError {
    fn from(e: AgentShellError) -> Self {
        // 构造路径上把 core 错误原样包进 Scripting 语义（保留消息）。
        KWinError::Scripting(e.to_string())
    }
}

impl From<KWinError> for AgentShellError {
    fn from(e: KWinError) -> Self {
        match e {
            KWinError::ProtocolBind { interface, reason } => {
                AgentShellError::BackendUnavailable(format!("kwin {interface}: {reason}"))
            }
            KWinError::FakeInputNotAuthenticated => {
                AgentShellError::Permission("kwin fake_input not authenticated".into())
            }
            KWinError::Scripting(msg) => AgentShellError::DBus(format!("kwin scripting: {msg}")),
            KWinError::ResponseTimeout {
                script,
                timeout_secs,
            } => AgentShellError::Timeout(format!(
                "kwin script `{script}` no response in {timeout_secs}s"
            )),
            KWinError::InvalidScriptOutput(msg) => {
                AgentShellError::DBus(format!("kwin script output: {msg}"))
            }
        }
    }
}

/// KWin crate 内部 Result 别名。
pub type Result<T, E = KWinError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_bind_maps_to_backend_unavailable() {
        let e = AgentShellError::from(KWinError::ProtocolBind {
            interface: "org_kde_plasma_window_management",
            reason: "already bound by another client".into(),
        });
        assert!(matches!(e, AgentShellError::BackendUnavailable(_)));
        assert!(e.to_string().contains("org_kde_plasma_window_management"));
    }

    #[test]
    fn response_timeout_carries_script_and_limit() {
        let e: AgentShellError = KWinError::ResponseTimeout {
            script: "list_windows.js".into(),
            timeout_secs: 5,
        }
        .into();
        assert!(matches!(e, AgentShellError::Timeout(_)));
        assert!(e.to_string().contains("list_windows.js"));
        assert!(e.to_string().contains('5'));
    }

    #[test]
    fn fake_input_auth_error_is_permission() {
        let e: AgentShellError = KWinError::FakeInputNotAuthenticated.into();
        assert!(matches!(e, AgentShellError::Permission(_)));
    }
}
