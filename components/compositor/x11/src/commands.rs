//! xdotool/wmctrl 命令封装（设计文档 §11 模块结构表 `commands.rs`，降级层）。
//!
//! **仅当原生 EWMH/XTest 路径失败时**兜底使用。CLI 工具为非必需依赖：
//! [`X11Commands::new`] 用 `which` 探测可执行文件，缺失返回 `None` 字段，
//! 不阻塞 [`crate::X11Compositor`] 初始化。
//!
//! xdotool 自身走 XTest（独立进程 + 独立连接），对扩展版本协商差异容忍度
//! 更高；wmctrl 走 EWMH ClientMessage。二者覆盖原生路径的同类操作，
//! 作为原生失败后的最后手段。

use std::time::Duration;

use agent_shell_core::error::{AgentShellError, Result};

/// CLI 工具保底封装（`cmd: Option<X11Commands>`）。
///
/// `xdotool` / `wmctrl` 各自独立探测：某个缺失不影响另一个的使用。
#[derive(Debug, Default)]
pub struct X11Commands {
    xdotool: Option<std::path::PathBuf>,
    wmctrl: Option<std::path::PathBuf>,
}

impl X11Commands {
    /// 探测 xdotool/wmctrl 可执行路径；缺失的工具对应字段为 `None`。
    pub fn new() -> Self {
        Self {
            xdotool: which::which("xdotool").ok(),
            wmctrl: which::which("wmctrl").ok(),
        }
    }

    /// xdotool 是否可用。
    pub fn has_xdotool(&self) -> bool {
        self.xdotool.is_some()
    }

    /// wmctrl 是否可用。
    pub fn has_wmctrl(&self) -> bool {
        self.wmctrl.is_some()
    }

    /// 至少一个 CLI 工具可用。
    pub fn is_usable(&self) -> bool {
        self.has_xdotool() || self.has_wmctrl()
    }

    // ───────────────────────── 窗口操作（wmctrl 优先，xdotool 补充） ─────────────────────────

    /// 聚焦窗口（wmctrl `-a`，失败回退 xdotool `windowactivate`）。
    pub async fn activate_window(&self, window_id: &str) -> Result<()> {
        if let Some(bin) = &self.wmctrl {
            run_cmd(bin, &["-i", "-a", window_id]).await?;
            return Ok(());
        }
        if let Some(bin) = &self.xdotool {
            run_cmd(bin, &["windowactivate", window_id]).await?;
            return Ok(());
        }
        Err(AgentShellError::BackendUnavailable(
            "no CLI fallback (xdotool/wmctrl) available".into(),
        ))
    }

    /// 关闭窗口（wmctrl `-c`，失败回退 xdotool `windowclose`）。
    pub async fn close_window(&self, window_id: &str) -> Result<()> {
        if let Some(bin) = &self.wmctrl {
            run_cmd(bin, &["-i", "-c", window_id]).await?;
            return Ok(());
        }
        if let Some(bin) = &self.xdotool {
            run_cmd(bin, &["windowclose", window_id]).await?;
            return Ok(());
        }
        Err(AgentShellError::BackendUnavailable(
            "no CLI fallback (xdotool/wmctrl) available".into(),
        ))
    }

    /// 移动/缩放窗口（xdotool `windowsize --sync` + `windowmove --sync`）。
    pub async fn move_resize_window(
        &self,
        window_id: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<()> {
        let bin = self.xdotool.as_deref().ok_or_else(|| {
            AgentShellError::BackendUnavailable("xdotool not available for move/resize".into())
        })?;
        let w = w.to_string();
        let h = h.to_string();
        run_cmd(bin, &["windowsize", "--sync", window_id, &w, &h]).await?;
        let x = x.to_string();
        let y = y.to_string();
        run_cmd(bin, &["windowmove", "--sync", window_id, &x, &y]).await
    }

    /// 最小化窗口（xdotool `windowminimize`）。
    pub async fn minimize_window(&self, window_id: &str) -> Result<()> {
        let bin = self.xdotool.as_deref().ok_or_else(|| {
            AgentShellError::BackendUnavailable("xdotool not available for minimize".into())
        })?;
        run_cmd(bin, &["windowminimize", window_id]).await
    }

    /// 切换工作区（wmctrl `-s`，失败回退 xdotool `set_desktop`）。
    pub async fn switch_workspace(&self, desktop: u32) -> Result<()> {
        let d = desktop.to_string();
        if let Some(bin) = &self.wmctrl {
            run_cmd(bin, &["-s", &d]).await?;
            return Ok(());
        }
        if let Some(bin) = &self.xdotool {
            run_cmd(bin, &["set_desktop", &d]).await?;
            return Ok(());
        }
        Err(AgentShellError::BackendUnavailable(
            "no CLI fallback for switch_workspace".into(),
        ))
    }

    /// 移窗口到工作区（wmctrl `-r` + `-t`）。
    pub async fn move_to_workspace(&self, window_id: &str, desktop: u32) -> Result<()> {
        let d = desktop.to_string();
        let bin = self.wmctrl.as_deref().ok_or_else(|| {
            AgentShellError::BackendUnavailable("wmctrl not available for move_to_workspace".into())
        })?;
        run_cmd(bin, &["-i", "-r", window_id, "-t", &d]).await
    }
}

const CLI_TIMEOUT: Duration = Duration::from_secs(3);

/// 运行 CLI 子命令（超时 3s，失败返回 stderr 首行）。
async fn run_cmd(bin: &std::path::Path, args: &[&str]) -> Result<()> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let out = tokio::time::timeout(CLI_TIMEOUT, cmd.output())
        .await
        .map_err(|_| AgentShellError::Timeout(format!("cli timeout: {}", bin.display())))?
        .map_err(|e| AgentShellError::Other(Box::new(e)))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.lines().next().unwrap_or("unknown cli error");
        return Err(AgentShellError::Other(
            format!("{}: {msg}", bin.display()).into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_detects_tools_without_panic() {
        // 无论环境是否安装 xdotool/wmctrl，探测必须不 panic。
        let cmds = X11Commands::new();
        // is_usable 取决于环境，但字段访问安全。
        let _ = cmds.has_xdotool();
        let _ = cmds.has_wmctrl();
        let _ = cmds.is_usable();
    }

    #[tokio::test]
    async fn activate_without_tools_returns_unavailable() {
        // 两个工具都不存在时，激活必须返回结构化错误而非 panic。
        let cmds = X11Commands {
            xdotool: None,
            wmctrl: None,
        };
        let err = cmds.activate_window("0x1234567").await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got: {err:?}"
        );
    }
}
