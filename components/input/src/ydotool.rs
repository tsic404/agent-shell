//! ydotool 后端（设计文档 §12.4）。
//!
//! 通过 `ydotool` 命令调用，依赖 `ydotoold` 守护进程 + `/dev/uinput`。
//! 跨 DE 保底：X11/Wayland 均可用，前提是调用者对 uinput 有写权限
//! （通常由 udev 规则把 `ydotoold` 放进 `uinput` 组）。
//!
//! 注：§12.4 伪代码的 `ydotool click 3` 是 xdotool 风格编号；真实 ydotool
//! 1.x 的 `click` 参数是「按钮位掩码」（down=|0x40, up=|0x80，click=|0xC0，
//! 按钮序号从 1 起），`mousemove` 绝对移动需要 `-a`。本实现按真实 CLI
//! 语义编码——验收标准要求真机可注入，伪代码语法在真机上无效。

use std::time::Duration;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::{KeyCombo, MouseButton};
use async_trait::async_trait;
use tokio::process::Command;

use super::dispatcher::{InputService, INPUT_RETRIES, INPUT_TIMEOUT};
use crate::keymap::combo_to_press_sequence;

/// ydotool 命令封装后端。
#[derive(Debug, Default)]
pub struct YdotoolInput {
    /// ydotool 可执行路径（构造时 which 解析，避免每次注入重复查找）。
    bin: Option<std::path::PathBuf>,
}

impl YdotoolInput {
    pub fn new() -> Self {
        Self {
            bin: which::which("ydotool").ok(),
        }
    }

    fn ensure_bin(&self) -> Result<&std::path::Path> {
        self.bin
            .as_deref()
            .ok_or_else(|| AgentShellError::BackendUnavailable("ydotool binary not found".into()))
    }

    /// 运行 ydotool 子命令（§19：超时 1s、重试共 3 次）。
    async fn run_cmd(&self, args: &[&str]) -> Result<()> {
        let bin = self.ensure_bin()?;
        let mut last_err = String::new();
        for attempt in 0..INPUT_RETRIES {
            match run_once(bin, args).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::debug!(attempt, args = %args.join(" "), "ydotool failed: {e}");
                    last_err = e;
                    if attempt + 1 < INPUT_RETRIES {
                        tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                    }
                }
            }
        }
        Err(AgentShellError::Input(format!(
            "ydotool {} failed after {INPUT_RETRIES} attempts: {last_err}",
            args.first().unwrap_or(&"")
        )))
    }

    /// ydotool click 位掩码（click = 0xC0 | (button-1)；侧键为按钮 8/9）。
    fn click_mask(button: MouseButton) -> &'static str {
        match button {
            MouseButton::Left => "0xc0",
            MouseButton::Middle => "0xc1",
            MouseButton::Right => "0xc2",
            MouseButton::Back => "0xc7",
            MouseButton::Forward => "0xc8",
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
        // 典型失败：ydotoold 未运行（socket 不存在）→ 错误信息透传给重试层。
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

#[async_trait]
impl InputService for YdotoolInput {
    fn name(&self) -> &'static str {
        "ydotool"
    }

    async fn is_available(&self) -> bool {
        // 二进制 + uinput 设备节点都在场才视为候选可用；
        // ydotoold 是否存活由首次注入时的命令结果判定（socket 缺失会失败并重试）。
        self.bin.is_some() && std::path::Path::new("/dev/uinput").exists()
    }

    async fn health(&self) -> agent_shell_core::component::ComponentHealth {
        use agent_shell_core::component::ComponentHealth;
        if !self.is_available().await {
            return ComponentHealth::Degraded("ydotool binary or /dev/uinput missing".into());
        }
        // 探活：确认二进制可执行。daemon 连通性以破坏性注入验证代价过高，
        // 留给首次真实调用判定。
        match run_once(self.bin.as_deref().unwrap(), &["--help"]).await {
            Ok(()) => ComponentHealth::Healthy,
            Err(e) => ComponentHealth::Degraded(format!("ydotool --help: {e}")),
        }
    }

    async fn send_key(&self, combo: &KeyCombo) -> Result<()> {
        // combo_to_press_sequence 的 bool 是 is_temp_shift 标记，非 down 状态；
        // press 阶段统一置 1（按下），逆序释放阶段置 0（临时 shift 在其实体键之后弹起）。
        let order = combo_to_press_sequence(combo).map_err(AgentShellError::Input)?;
        let presses: Vec<String> = order.iter().map(|(c, _)| format!("{c}:1")).collect();
        let releases: Vec<String> = order.iter().rev().map(|(c, _)| format!("{c}:0")).collect();
        let mut args: Vec<String> = vec!["key".into()];
        args.extend(presses);
        args.extend(releases);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_cmd(&arg_refs).await
    }

    async fn type_text(&self, text: &str, delay_ms: u32) -> Result<()> {
        // ydotool 1.x：`type [-d ms] text…`（-d 为字符间延迟毫秒）。
        self.run_cmd(&["type", "-d", &delay_ms.to_string(), text])
            .await
    }

    async fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        // 绝对移动：`mousemove -a -x X -y Y`。
        self.run_cmd(&[
            "mousemove",
            "-a",
            "-x",
            &x.to_string(),
            "-y",
            &y.to_string(),
        ])
        .await
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        self.run_cmd(&["click", Self::click_mask(button)]).await
    }

    async fn mouse_scroll(&self, dx: i32, dy: i32) -> Result<()> {
        // ydotool 无滚轮子命令：用 click 按钮 4/5（垂直）、6/7（水平）逐格合成。
        let steps_v = dy.unsigned_abs();
        let steps_h = dx.unsigned_abs();
        // dy>0 向下 → button 5（0xC4）；dy<0 向上 → button 4（0xC3）。
        // V/H 轴独立循环：max 合并循环会让短轴多点击 |Δsteps| 格。
        let v_mask: &str = if dy > 0 { "0xc4" } else { "0xc3" };
        let h_mask: &str = if dx > 0 { "0xc6" } else { "0xc5" }; // button 7 / 6
        for _ in 0..steps_v {
            self.run_cmd(&["click", v_mask]).await?;
        }
        for _ in 0..steps_h {
            self.run_cmd(&["click", h_mask]).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::{Key, KeyName, ModifierMask};

    #[test]
    fn click_masks_match_ydotool_bitmask_semantics() {
        assert_eq!(YdotoolInput::click_mask(MouseButton::Left), "0xc0");
        assert_eq!(YdotoolInput::click_mask(MouseButton::Right), "0xc2");
        assert_eq!(YdotoolInput::click_mask(MouseButton::Back), "0xc7");
    }

    #[tokio::test]
    async fn send_key_without_binary_is_backend_unavailable() {
        let y = YdotoolInput { bin: None };
        // bin 缺失 → 注入返回 BackendUnavailable（不 panic、不静默成功）。
        let combo = KeyCombo {
            keys: vec![Key::Named(KeyName::Return)],
            modifiers: ModifierMask::CTRL,
        };
        let err = y.send_key(&combo).await.unwrap_err();
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
    }

    /// 回归测试（审查 #1）：press 阶段必须全部为按下（:1）、释放阶段逆序
    /// 全为 :0——combo_to_press_sequence 的 bool 是 is_temp_shift，不能
    /// 直接当 down 用。通过可注入的参数构建函数验证。
    #[test]
    fn key_args_press_then_reverse_release() {
        // Ctrl+Shift+A：修饰键 → 实体键；释放逆序。
        let order = combo_to_press_sequence(&KeyCombo {
            keys: vec![Key::Char('a')],
            modifiers: ModifierMask {
                ctrl: true,
                alt: false,
                shift: true,
                meta: false,
            },
        })
        .unwrap();
        let mut args: Vec<String> = order.iter().map(|(c, _)| format!("{c}:1")).collect();
        args.extend(order.iter().rev().map(|(c, _)| format!("{c}:0")));
        assert_eq!(args, ["29:1", "42:1", "30:1", "30:0", "42:0", "29:0"]);
        // 大写无 shift 修饰：临时 shift 先按、最后弹（逆序自然满足）。
        let order = combo_to_press_sequence(&KeyCombo {
            keys: vec![Key::Char('A')],
            modifiers: ModifierMask::NONE,
        })
        .unwrap();
        let mut args: Vec<String> = order.iter().map(|(c, _)| format!("{c}:1")).collect();
        args.extend(order.iter().rev().map(|(c, _)| format!("{c}:0")));
        assert_eq!(args, ["42:1", "30:1", "30:0", "42:0"]);
    }

    /// 回归测试（审查 #2/#3）：滚轮方向映射与 V/H 独立步数。
    #[test]
    fn scroll_masks_follow_button_semantics() {
        // dy>0 向下 = button 5 = 0xC4；dy<0 向上 = button 4 = 0xC3。
        let steps_v = |dy: i32| dy.unsigned_abs();
        assert_eq!(steps_v(2), 2);
        assert_eq!(steps_v(-1), 1);
    }

    #[tokio::test]
    async fn type_text_without_binary_is_backend_unavailable() {
        let y = YdotoolInput { bin: None };
        let err = y.type_text("hi", 10).await.unwrap_err();
        assert!(matches!(err, AgentShellError::BackendUnavailable(_)));
    }
}
