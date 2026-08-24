//! 剪贴板组件（设计文档 §21.3 剪贴板、§21.4 ClipboardComponent）。
//!
//! 路由（§3.1 剪贴板行）：Wayland 会话 → `wl-copy`/`wl-paste`；
//! X11 会话 → `xclip -selection clipboard`（`xsel -b` 备选）。
//!
//! portal `org.freedesktop.portal.Clipboard` 为会话绑定流式接口
//! （RequestClipboard → fd），KDE/GNOME ✓、DDE/Hyprland ✗，适合常驻
//! 同步场景；本组件的按需读写走 CLI 工具路径，portal 通道由后续
//! 常驻同步任务接入。DDE `org.deepin.dde.Clipboard1` 是历史记录管理器，
//! 非标准内容通道，不在此使用。

use agent_shell_core::component::{
    ClipboardComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use async_trait::async_trait;

/// 会话类型探测结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Wayland,
    X11,
    Unknown,
}

fn detect_session() -> SessionKind {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("WAYLAND_SOCKET").is_some()
    {
        return SessionKind::Wayland;
    }
    if std::env::var_os("DISPLAY").is_some() {
        return SessionKind::X11;
    }
    SessionKind::Unknown
}

/// wl-clipboard / xclip 公共剪贴板组件。
pub struct WlClipboard {
    session: SessionKind,
}

impl Default for WlClipboard {
    fn default() -> Self {
        Self {
            session: detect_session(),
        }
    }
}

impl WlClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 显式指定会话类型（backend 装配时已知会话类型时使用）。
    pub fn with_session(session: SessionKind) -> Self {
        Self { session }
    }

    async fn run(
        &self,
        wayland: (&str, Vec<&str>),
        x11: (&str, Vec<&str>),
        stdin: Option<&str>,
    ) -> Result<String> {
        let (bin, args) = match self.session {
            SessionKind::Wayland => wayland,
            SessionKind::X11 => x11,
            SessionKind::Unknown => {
                return Err(AgentShellError::BackendUnavailable(
                    "no WAYLAND_DISPLAY or DISPLAY in environment; cannot pick clipboard tool"
                        .into(),
                ))
            }
        };
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(&args);
        // 关键：wl-copy/xclip 写入后会 fork 后台进程持有选区继续服务；
        // 若 stdout/stderr 是管道，守护进程继承管道写端，`wait_with_output()`
        // 读到 EOF 前会永久阻塞（QA 实测死锁）。写入路径必须置 Stdio::null()
        // 让子进程与管道解耦；读取路径保留 piped 以取回内容。
        if stdin.is_some() {
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
            let mut child = cmd
                .spawn()
                .map_err(|e| AgentShellError::BackendUnavailable(format!("{bin}: {e}")))?;
            {
                use tokio::io::AsyncWriteExt;
                let mut si = child
                    .stdin
                    .take()
                    .ok_or_else(|| AgentShellError::Other("stdin unavailable".into()))?;
                si.write_all(stdin.unwrap_or_default().as_bytes()).await.map_err(|e| {
                    AgentShellError::Other(format!("clipboard stdin write: {e}").into())
                })?;
                // 关闭 stdin 后等待首进程退出（wl-copy 首进程写完即退出，
                // fork 出的选区服务进程已与我们的管道无关）。
                drop(si);
            }
            let status = child
                .wait()
                .await
                .map_err(|e| AgentShellError::Other(format!("{bin}: {e}").into()))?;
            if !status.success() {
                return Err(AgentShellError::BackendUnavailable(format!(
                    "{bin} {:?}: exited with {status}",
                    args
                )));
            }
            return Ok(String::new());
        }
        // 读取路径：stdout piped 取回文本，stderr piped 仅收集错误信息。
        // wl-paste/xclip -o 即读即退，不会驻留，wait_with_output 安全。
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let out = cmd
            .spawn()
            .map_err(|e| AgentShellError::BackendUnavailable(format!("{bin}: {e}")))?
            .wait_with_output()
            .await
            .map_err(|e| AgentShellError::Other(format!("{bin}: {e}").into()))?;
        if !out.status.success() {
            return Err(AgentShellError::BackendUnavailable(format!(
                "{bin} {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

#[async_trait]
impl DesktopComponent for WlClipboard {
    fn name(&self) -> &'static str {
        "wl-x-clipboard"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Clipboard
    }

    fn is_available(&self) -> bool {
        self.session != SessionKind::Unknown
    }

    async fn health(&self) -> ComponentHealth {
        match self.session {
            SessionKind::Wayland => {
                probe_tool("wl-paste", &["--version"]).await
            }
            SessionKind::X11 => probe_tool("xclip", &["-version"]).await,
            SessionKind::Unknown => ComponentHealth::Degraded(
                "no graphical session detected (WAYLAND_DISPLAY/DISPLAY unset)".into(),
            ),
        }
    }
}

#[async_trait]
impl ClipboardComponent for WlClipboard {
    async fn clipboard_read(&self) -> Result<String> {
        self.run(
            ("wl-paste", vec!["--no-newline"]),
            ("xclip", vec!["-selection", "clipboard", "-o"]),
            None,
        )
        .await
    }

    async fn clipboard_write(&self, text: &str) -> Result<()> {
        self.run(
            ("wl-copy", vec![]),
            ("xclip", vec!["-selection", "clipboard"]),
            Some(text),
        )
        .await?;
        Ok(())
    }
}

/// X11 专用实例（§3.1 X11Generic 行：xclip）。
pub struct XClipboard(pub WlClipboard);

impl XClipboard {
    pub fn new() -> Self {
        Self(WlClipboard::with_session(SessionKind::X11))
    }
}

impl Default for XClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for XClipboard {
    type Target = WlClipboard;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl DesktopComponent for XClipboard {
    fn name(&self) -> &'static str {
        "x-clipboard"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Clipboard
    }

    fn is_available(&self) -> bool {
        self.0.is_available()
    }

    async fn health(&self) -> ComponentHealth {
        self.0.health().await
    }
}

#[async_trait]
impl ClipboardComponent for XClipboard {
    async fn clipboard_read(&self) -> Result<String> {
        self.0.clipboard_read().await
    }

    async fn clipboard_write(&self, text: &str) -> Result<()> {
        self.0.clipboard_write(text).await
    }
}

async fn probe_tool(bin: &str, args: &[&str]) -> ComponentHealth {
    match tokio::process::Command::new(bin).args(args).output().await {
        Ok(o) if o.status.success() => ComponentHealth::Healthy,
        Ok(_) => ComponentHealth::Degraded(format!("{bin} exited non-zero")),
        Err(e) => ComponentHealth::Degraded(format!("{bin}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_session_kind() {
        // 本进程至少有一种探测结果，不 panic；显式构造优先。
        assert_eq!(
            WlClipboard::with_session(SessionKind::X11).session,
            SessionKind::X11
        );
    }
}
