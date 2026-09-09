//! xdotool 后端（设计文档 §12.1「再降级」行）。
//!
//! 最后兜底命令封装：XTest 扩展探测失败（或 x11rb 连接异常）但 `xdotool`
//! 可执行存在时的最后通道。xdotool 自身走 XTest，但独立进程 + 独立连接，
//! 对扩展版本协商差异容忍度更高。
//!
//! `DISPLAY` 存在即由 dispatcher 压入（原生 X11 或 XWayland 会话）。
//! XWayland 下经 XTest 注入（部分合成器如 KWin 允许），注入失败错误透传给
//! 调用方——不 panic、不重复降级重放。

use std::time::Duration;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{Key, KeyCombo, KeyName, MouseButton};
use async_trait::async_trait;
use tokio::process::Command;

use super::dispatcher::{InputService, INPUT_RETRIES, INPUT_TIMEOUT};

/// xdotool 命令封装后端。
#[derive(Debug, Default)]
pub struct XdotoolInput {
    /// xdotool 可执行路径（构造时 which 解析）。
    bin: Option<std::path::PathBuf>,
}

impl XdotoolInput {
    pub fn new() -> Self {
        Self {
            bin: which::which("xdotool").ok(),
        }
    }

    fn ensure_bin(&self) -> Result<&std::path::Path> {
        self.bin
            .as_deref()
            .ok_or_else(|| AgentShellError::BackendUnavailable("xdotool binary not found".into()))
    }

    /// 运行 xdotool 子命令（§19：超时 1s、重试共 3 次）。
    async fn run_cmd(&self, args: &[&str]) -> Result<()> {
        let bin = self.ensure_bin()?;
        let mut last_err = String::new();
        for attempt in 0..INPUT_RETRIES {
            match run_once(bin, args).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::debug!(attempt, args = %args.join(" "), "xdotool failed: {e}");
                    last_err = e;
                    if attempt + 1 < INPUT_RETRIES {
                        tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                    }
                }
            }
        }
        Err(AgentShellError::Input(format!(
            "xdotool {} failed after {INPUT_RETRIES} attempts: {last_err}",
            args.first().unwrap_or(&"")
        )))
    }

    /// 命名键 → xdotool keysym 名。
    fn named_keysym(name: KeyName) -> Option<&'static str> {
        use KeyName::*;
        Some(match name {
            Return => "Return",
            Escape => "Escape",
            BackSpace => "BackSpace",
            Tab => "Tab",
            Space => "space",
            Left => "Left",
            Right => "Right",
            Up => "Up",
            Down => "Down",
            Home => "Home",
            End => "End",
            PageUp => "Page_Up",
            PageDown => "Page_Down",
            Insert => "Insert",
            Delete => "Delete",
            Menu => "Menu",
            F1 => "F1",
            F2 => "F2",
            F3 => "F3",
            F4 => "F4",
            F5 => "F5",
            F6 => "F6",
            F7 => "F7",
            F8 => "F8",
            F9 => "F9",
            F10 => "F10",
            F11 => "F11",
            F12 => "F12",
        })
    }

    /// 修饰键前缀（`key`/`type` 的 modifier 参数序）。
    fn modifier_prefix(combo: &KeyCombo) -> Vec<String> {
        let mut m = Vec::new();
        if combo.modifiers.ctrl {
            m.push("ctrl".into());
        }
        if combo.modifiers.alt {
            m.push("alt".into());
        }
        if combo.modifiers.shift {
            m.push("shift".into());
        }
        if combo.modifiers.meta {
            m.push("meta".into());
        }
        m
    }

    fn mouse_button_code(b: MouseButton) -> &'static str {
        match b {
            MouseButton::Left => "1",
            MouseButton::Middle => "2",
            MouseButton::Right => "3",
            MouseButton::Back => "8",
            MouseButton::Forward => "9",
        }
    }
}

async fn run_once(bin: &std::path::Path, args: &[&str]) -> std::result::Result<(), String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let out = tokio::time::timeout(INPUT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "timed out after 1s".to_string())?
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

#[async_trait]
impl InputService for XdotoolInput {
    fn name(&self) -> &'static str {
        "xdotool"
    }

    async fn is_available(&self) -> bool {
        self.bin.is_some() && std::env::var_os("DISPLAY").is_some()
    }

    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        let mut args: Vec<String> = vec!["key".into()];
        args.extend(Self::modifier_prefix(combo));
        for key in &combo.keys {
            match key {
                Key::Named(n) => {
                    let sym = Self::named_keysym(*n).ok_or_else(|| {
                        AgentShellError::Input(format!("no keysym mapped for {n:?}"))
                    })?;
                    args.push(sym.into());
                }
                Key::Char(c) => args.push(c.to_string()),
            }
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_cmd(&arg_refs).await
    }

    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        // `type --delay <ms> text…`；文本作为单参数传递避免 shell 解析。
        let delay = delay_ms.to_string();
        self.run_cmd(&["type", "--delay", &delay, text]).await
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.run_cmd(&["mousemove", &x.to_string(), &y.to_string()])
            .await
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.run_cmd(&["click", Self::mouse_button_code(button)])
            .await
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        // dy>0 向下（button 5）、dy<0 向上（4）；dx 走 7/6。
        let steps_v = dy.unsigned_abs();
        let steps_h = dx.unsigned_abs();
        let v_btn: &str = if dy > 0 { "5" } else { "4" };
        let h_btn: &str = if dx > 0 { "7" } else { "6" };
        // V/H 轴独立循环：max 合并会让短轴多点击 |Δsteps| 格。
        for _ in 0..steps_v {
            self.run_cmd(&["click", v_btn]).await?;
        }
        for _ in 0..steps_h {
            self.run_cmd(&["click", h_btn]).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::ModifierMask;

    #[test]
    fn mouse_button_codes_match_xdotool_convention() {
        assert_eq!(XdotoolInput::mouse_button_code(MouseButton::Left), "1");
        assert_eq!(XdotoolInput::mouse_button_code(MouseButton::Right), "3");
        assert_eq!(XdotoolInput::mouse_button_code(MouseButton::Back), "8");
        assert_eq!(XdotoolInput::mouse_button_code(MouseButton::Forward), "9");
    }

    #[test]
    fn named_keysym_mapping_is_total_over_keyname() {
        use KeyName::*;
        let all = [
            Return, Escape, BackSpace, Tab, Space, Left, Right, Up, Down, Home, End, PageUp,
            PageDown, Insert, Delete, Menu, F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
        ];
        for n in all {
            assert!(XdotoolInput::named_keysym(n).is_some(), "{n:?} unmapped");
        }
    }

    #[test]
    fn modifier_prefix_order_is_stable() {
        let combo = KeyCombo {
            keys: vec![],
            modifiers: ModifierMask {
                ctrl: true,
                alt: false,
                shift: true,
                meta: false,
            },
        };
        assert_eq!(XdotoolInput::modifier_prefix(&combo), vec!["ctrl", "shift"]);
    }

    #[tokio::test]
    async fn injection_without_binary_reports_backend_unavailable() {
        let x = XdotoolInput { bin: None };
        let err = x.type_text("hi", 5).await.unwrap_err();
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
        assert!(!x.is_available().await);
    }
}
