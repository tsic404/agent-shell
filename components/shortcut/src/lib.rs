//! 全局快捷键组件（设计文档 §21.33「全局快捷键」）。
//!
//! [`ShortcutOps`] 是能力契约（daemon 持 `dyn ShortcutOps`，测试注入 fake）。
//! 公开实现 [`ShortcutBinder`] 把组合键 + 命令动作落到 DE 的**持久化**接口：
//! KDE 写 `kglobalaccel` 命令快捷键服务（desktop 文件 + `doRegister` +
//! `setForeignShortcut`，见 [`kde`]）、GNOME 写 gsettings 自定义快捷键
//! （见 [`gnome`]）、Hyprland 写 `hyprland.conf` 片段并 `hyprctl reload`
//! （见 [`hyprland`]）。
//!
//! 组合键到各 DE 语法的转换（Qt keycode / GTK accelerator / Hyprland bind）
//! 集中在 [`combo`]：语法错误只会在用户按键时暴露，必须逐个钉住。
//!
//! **后端选择**：按检测到的 DE 选唯一后端——跨 DE 回退会写出目标桌面根本
//! 不监听的绑定。仅在 DE 未知时依次探测三家。portal
//! `org.freedesktop.portal.GlobalShortcuts` 属会话级绑定，与「CLI 一次调用
//! 一个瞬态 daemon」（§22.2 D1）的进程模型冲突，列为路线图项而非静默降级。

pub mod combo;
mod gnome;
mod hyprland;
mod kde;
#[cfg(test)]
mod testsupport;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::ShortcutBinding;
use agent_shell_core::types::{DesktopEnvironment, KeyCombo};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// 全局快捷键契约（§21.33）。
#[async_trait]
pub trait ShortcutOps: Send + Sync {
    /// 绑定组合键到命令动作，返回生效后端与绑定结果。
    ///
    /// `combo` 须为单个按键 + 修饰键（多键序列无对应后端语法，返回错误）；
    /// `action` 为后端触发时执行的命令行（shell 语义）。
    async fn bind(&self, combo: &KeyCombo, action: &str) -> Result<ShortcutBinding>;
}

/// 后端候选（与 DE 一一对应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Kde,
    Gnome,
    Hyprland,
}

/// 全局快捷键绑定器（按 DE 选后端）。
pub struct ShortcutBinder {
    de: DesktopEnvironment,
    conn: Option<zbus::Connection>,
    data_home: PathBuf,
    config_home: PathBuf,
    gsettings: PathBuf,
    hyprctl: PathBuf,
}

impl ShortcutBinder {
    /// 生产装配：探测 DE、session bus 与各后端二进制路径。
    pub async fn new() -> Self {
        Self {
            de: agent_shell_core::de_detection::detect_desktop_environment(),
            conn: zbus::Connection::session().await.ok(),
            data_home: xdg_dir("XDG_DATA_HOME", ".local/share"),
            config_home: xdg_dir("XDG_CONFIG_HOME", ".config"),
            gsettings: PathBuf::from("gsettings"),
            hyprctl: PathBuf::from("hyprctl"),
        }
    }

    /// 注入 DE / 连接 / 路径（集成测试用；不读进程环境）。
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)] // 测试缝：逐个后端路径可注入
    pub fn for_test(
        de: DesktopEnvironment,
        conn: Option<zbus::Connection>,
        data_home: PathBuf,
        config_home: PathBuf,
        gsettings: PathBuf,
        hyprctl: PathBuf,
    ) -> Self {
        Self {
            de,
            conn,
            data_home,
            config_home,
            gsettings,
            hyprctl,
        }
    }

    /// 后端优先级：DE 命中者唯一，DE 未知时依次探测三家。
    fn order(&self) -> Vec<Backend> {
        match self.de {
            DesktopEnvironment::KDE => vec![Backend::Kde],
            DesktopEnvironment::GNOME => vec![Backend::Gnome],
            DesktopEnvironment::Hyprland => vec![Backend::Hyprland],
            _ => vec![Backend::Kde, Backend::Gnome, Backend::Hyprland],
        }
    }
}

#[async_trait]
impl ShortcutOps for ShortcutBinder {
    async fn bind(&self, combo: &KeyCombo, action: &str) -> Result<ShortcutBinding> {
        validate_action(action).map_err(|e| AgentShellError::Other(e.into()))?;
        combo::single_key(combo).map_err(|e| AgentShellError::Other(e.into()))?;

        let mut reasons: Vec<String> = Vec::new();
        for backend in self.order() {
            match backend {
                Backend::Kde => {
                    let Some(conn) = self.conn.as_ref() else {
                        reasons.push("kde: no session bus connection".into());
                        continue;
                    };
                    if let Err(reason) = kde::ready(conn).await {
                        reasons.push(format!("kde: {reason}"));
                        continue;
                    }
                    return kde::bind(conn, &self.data_home, combo, action).await;
                }
                Backend::Gnome => {
                    if let Err(reason) = gnome::ready(&self.gsettings).await {
                        reasons.push(format!("gnome: {reason}"));
                        continue;
                    }
                    return gnome::bind(&self.gsettings, combo, action).await;
                }
                Backend::Hyprland => {
                    if let Err(reason) = hyprland::ready(&self.hyprctl).await {
                        reasons.push(format!("hyprland: {reason}"));
                        continue;
                    }
                    return hyprland::bind(&self.hyprctl, &self.config_home, combo, action).await;
                }
            }
        }
        Err(AgentShellError::BackendUnavailable(format!(
            "no global-shortcut backend available ({})",
            reasons.join("; ")
        )))
    }
}

/// 动作串校验：非空且不含控制字符。
///
/// 动作会写入被 DE 解析的配置文件（Hyprland 片段 / desktop `Exec=`），换行与
/// 其它控制字符能开启新的配置指令；三个后端的格式转换都假定此处已拦下非法输入。
pub(crate) fn validate_action(action: &str) -> std::result::Result<(), String> {
    if action.trim().is_empty() {
        return Err("empty shortcut action".into());
    }
    if action.chars().any(char::is_control) {
        return Err("action contains control characters".into());
    }
    Ok(())
}

/// XDG 目录：环境变量优先，回退 `$HOME/<fallback>`，再回退当前目录。
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    if let Some(value) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return PathBuf::from(value);
    }
    match std::env::var_os("HOME") {
        Some(home) => Path::new(&home).join(fallback),
        None => PathBuf::from(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::error::AgentShellError;
    use agent_shell_core::types::{Key, ModifierMask};

    fn meta_t() -> KeyCombo {
        KeyCombo {
            keys: vec![Key::Char('t')],
            modifiers: ModifierMask {
                meta: true,
                ..ModifierMask::NONE
            },
        }
    }

    fn binder(de: DesktopEnvironment) -> ShortcutBinder {
        ShortcutBinder::for_test(
            de,
            None,
            PathBuf::from("/nonexistent/data"),
            PathBuf::from("/nonexistent/config"),
            PathBuf::from("/nonexistent/gsettings"),
            PathBuf::from("/nonexistent/hyprctl"),
        )
    }

    #[test]
    fn detected_de_selects_a_single_backend() {
        assert_eq!(binder(DesktopEnvironment::KDE).order(), vec![Backend::Kde]);
        assert_eq!(
            binder(DesktopEnvironment::GNOME).order(),
            vec![Backend::Gnome]
        );
        assert_eq!(
            binder(DesktopEnvironment::Hyprland).order(),
            vec![Backend::Hyprland]
        );
        // DE 未知/其它：三家依次探测。
        assert_eq!(
            binder(DesktopEnvironment::Unknown).order(),
            vec![Backend::Kde, Backend::Gnome, Backend::Hyprland]
        );
    }

    #[tokio::test]
    async fn bind_rejects_multi_key_combos_before_touching_backends() {
        let combo = KeyCombo {
            keys: vec![Key::Char('k'), Key::Char('c')],
            modifiers: ModifierMask::NONE,
        };
        let err = binder(DesktopEnvironment::KDE)
            .bind(&combo, "notify-send hi")
            .await
            .expect_err("multi-key combo must be rejected");
        assert!(matches!(err, AgentShellError::Other(_)), "got {err:?}");
        assert!(err.to_string().contains("exactly one key"));
    }

    #[tokio::test]
    async fn bind_rejects_empty_action() {
        let err = binder(DesktopEnvironment::KDE)
            .bind(&meta_t(), "   ")
            .await
            .expect_err("empty action must be rejected");
        assert!(err.to_string().contains("empty shortcut action"));
    }

    #[tokio::test]
    async fn bind_rejects_control_characters_in_action() {
        for action in ["touch a\ntouch b", "touch a\rmonitor=x", "\ta"] {
            let err = binder(DesktopEnvironment::KDE)
                .bind(&meta_t(), action)
                .await
                .expect_err("control characters must be rejected");
            assert!(
                err.to_string().contains("control characters"),
                "unexpected error for {action:?}: {err}"
            );
        }
    }

    #[test]
    fn validate_action_accepts_plain_commands_and_rejects_blanks_and_controls() {
        assert!(validate_action("notify-send 你好 world").is_ok());
        assert!(validate_action("sh -c 'echo hi'").is_ok());
        assert!(validate_action("").is_err());
        assert!(validate_action("   ").is_err());
        assert!(validate_action("a\nb").is_err());
    }

    #[tokio::test]
    async fn bind_reports_each_backend_reason_when_none_is_available() {
        let err = binder(DesktopEnvironment::Unknown)
            .bind(&meta_t(), "notify-send hi")
            .await
            .expect_err("no backend on this environment");
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got {err:?}"
        );
        let message = err.to_string();
        for segment in ["kde:", "gnome:", "hyprland:"] {
            assert!(message.contains(segment), "missing {segment} in: {message}");
        }
    }
}
