//! 触控板与键盘布局组件（设计文档 §21.31「触控板 / 键盘布局」）。
//!
//! [`PeripheralsOps`] 是能力契约（daemon 持 `dyn PeripheralsOps`，测试注入
//! fake）。公开实现 [`DesktopPeripherals`] 按 DE 优先级链取数：KDE 走 KWin
//! `org.kde.KWin.InputDeviceManager` / `kxkbrc`，GNOME 走 gsettings，X11 走
//! `setxkbmap`；链路全失败返回 BackendUnavailable 并列出各段原因。

mod gnome;
mod kde;
#[cfg(test)]
mod testsupport;
mod x11;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_shell_core::de_detection::detect_desktop_environment;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{KeyboardLayouts, TouchpadStatus};
use agent_shell_core::DesktopEnvironment;
use async_trait::async_trait;

/// 数据来源标识：KWin InputDevice D-Bus 通道。
pub(crate) const SOURCE_KDE_TOUCHPAD: &str = "kde-kwin";
/// 数据来源标识：KDE `kxkbrc` 配置文件。
pub(crate) const SOURCE_KDE_KXKBRC: &str = "kde-kxkbrc";
/// 数据来源标识：GNOME gsettings。
pub(crate) const SOURCE_GNOME_GSETTINGS: &str = "gnome-gsettings";
/// 数据来源标识：X11 `setxkbmap -query`。
pub(crate) const SOURCE_X11_SETXKB: &str = "x11-setxkbmap";

/// CLI 单次调用超时：挂起的 gsettings/setxkbmap 不能拖死整条降级链。
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// KWin InputDevice 对象路径前缀（设备路径为 `<前缀>/<sysname>`）。
pub(crate) const KWIN_INPUT_DEVICE_PREFIX: &str = "/org/kde/KWin/InputDevice";

/// 键位配置文件名（位于 `$XDG_CONFIG_HOME`，即 `~/.config`）。
const KXKBRC_FILE: &str = "kxkbrc";

/// 输入外设契约（§21.31）。
#[async_trait]
pub trait PeripheralsOps: Send + Sync {
    /// 查询触控板状态（开关 / 自然滚动 / 点击手势）。
    async fn touchpad_status(&self) -> Result<TouchpadStatus>;
    /// 列出键盘布局。
    async fn keyboard_layouts(&self) -> Result<KeyboardLayouts>;
}

/// 触控板后端（按 DE 排序后逐段尝试）。
#[derive(Clone, Copy)]
enum TouchpadBackend {
    /// KDE：KWin InputDevice D-Bus 通道。
    KdeKwin,
    /// GNOME：gsettings 全局设置。
    GnomeGsettings,
}

impl TouchpadBackend {
    /// 分段名（与 `source` 取值一致，供链路诊断复用）。
    fn name(self) -> &'static str {
        match self {
            Self::KdeKwin => SOURCE_KDE_TOUCHPAD,
            Self::GnomeGsettings => SOURCE_GNOME_GSETTINGS,
        }
    }
}

/// 键盘布局后端。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutBackend {
    /// KDE：`kxkbrc` `[Layout]` 段。
    KdeKxkbrc,
    /// GNOME：gsettings input-sources。
    GnomeGsettings,
    /// X11：`setxkbmap -query`。
    X11Setxkbmap,
}

impl LayoutBackend {
    /// 分段名（与 `source` 取值一致，供链路诊断复用）。
    fn name(self) -> &'static str {
        match self {
            Self::KdeKxkbrc => SOURCE_KDE_KXKBRC,
            Self::GnomeGsettings => SOURCE_GNOME_GSETTINGS,
            Self::X11Setxkbmap => SOURCE_X11_SETXKB,
        }
    }
}

/// 会话显示服务器类型（决定 `setxkbmap` 是否可作为布局来源）。
///
/// 公开用于测试装配（[`DesktopPeripherals::for_test`]）：生产路径由
/// [`detect_session_kind`] 从 `XDG_SESSION_TYPE` / `WAYLAND_DISPLAY` / `DISPLAY` 判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    /// Wayland 会话：`setxkbmap` 只反映 XWayland 键映射，不作为会话布局来源。
    Wayland,
    /// X11 会话。
    X11,
    /// 无法判定（无 DISPLAY/WAYLAND_DISPLAY）：按最保守处理，不落 `setxkbmap`。
    Unknown,
}

/// DE 优先级链实现：命中的 DE 段提到链首，其余按 KDE → GNOME → X11。
pub struct DesktopPeripherals {
    /// 检测到的桌面环境（决定链首）。
    de: DesktopEnvironment,
    /// 会话显示服务器类型（决定布局链是否含 `setxkbmap`）。
    session: SessionKind,
    /// session bus；不可达时为 None（KDE 段自判失败后继续）。
    conn: Option<zbus::Connection>,
    /// `$XDG_CONFIG_HOME`（读 `kxkbrc`）。
    config_home: PathBuf,
    /// `gsettings` 可执行文件（测试注入脚本路径）。
    gsettings: PathBuf,
    /// `setxkbmap` 可执行文件（测试注入脚本路径）。
    setxkbmap: PathBuf,
}

impl DesktopPeripherals {
    /// 生产装配：检测 DE、连 session bus、按 PATH 解析两个 CLI。
    ///
    /// 构造阶段不探测后端可用性——bus 不可达时 `conn=None`，各段在查询时
    /// 自判失败并按链降级，因此构造永不失败（headless / TTY 亦可用）。
    pub async fn new() -> Self {
        Self {
            de: detect_desktop_environment(),
            session: detect_session_kind(),
            conn: zbus::Connection::session().await.ok(),
            config_home: config_home_from_env(),
            gsettings: PathBuf::from("gsettings"),
            setxkbmap: PathBuf::from("setxkbmap"),
        }
    }

    /// 测试装配：注入 DE、会话类型、连接与三个路径，避免测试读写进程环境变量。
    #[doc(hidden)]
    pub fn for_test(
        de: DesktopEnvironment,
        session: SessionKind,
        conn: Option<zbus::Connection>,
        config_home: PathBuf,
        gsettings: PathBuf,
        setxkbmap: PathBuf,
    ) -> Self {
        Self {
            de,
            session,
            conn,
            config_home,
            gsettings,
            setxkbmap,
        }
    }
}

#[async_trait]
impl PeripheralsOps for DesktopPeripherals {
    async fn touchpad_status(&self) -> Result<TouchpadStatus> {
        let mut reasons = Vec::new();
        for backend in touchpad_backends(self.de) {
            let attempt = match backend {
                TouchpadBackend::KdeKwin => kde::touchpad(self.conn.as_ref()).await,
                TouchpadBackend::GnomeGsettings => gnome::touchpad(&self.gsettings).await,
            };
            match attempt {
                Ok(status) => return Ok(status),
                Err(e) => reasons.push(format!("{}: {e}", backend.name())),
            }
        }
        Err(AgentShellError::BackendUnavailable(format!(
            "no touchpad backend: {}",
            reasons.join("; ")
        )))
    }

    async fn keyboard_layouts(&self) -> Result<KeyboardLayouts> {
        let mut reasons = Vec::new();
        for backend in layout_backends(self.de, self.session) {
            let attempt = match backend {
                LayoutBackend::KdeKxkbrc => kde::layouts(&self.config_home.join(KXKBRC_FILE)).await,
                LayoutBackend::GnomeGsettings => gnome::layouts(&self.gsettings).await,
                LayoutBackend::X11Setxkbmap => x11::layouts(&self.setxkbmap).await,
            };
            match attempt {
                Ok(layouts) => return Ok(layouts),
                Err(e) => reasons.push(format!("{}: {e}", backend.name())),
            }
        }
        Err(AgentShellError::BackendUnavailable(format!(
            "no keyboard-layout backend: {}",
            reasons.join("; ")
        )))
    }
}

/// 触控板后端顺序：GNOME 会话先 gsettings，其余先 KWin。
fn touchpad_backends(de: DesktopEnvironment) -> Vec<TouchpadBackend> {
    match de {
        DesktopEnvironment::GNOME => {
            vec![TouchpadBackend::GnomeGsettings, TouchpadBackend::KdeKwin]
        }
        _ => vec![TouchpadBackend::KdeKwin, TouchpadBackend::GnomeGsettings],
    }
}

/// 键盘布局后端顺序：GNOME 会话先 gsettings，其余先 `kxkbrc`。
///
/// `setxkbmap` 只在 X11 会话参与降级链：Wayland 下它读的是 XWayland 的键映射，
/// 与合成器实际布局可能不同，拿它当会话布局会给出一个「成功但错」的答案。
fn layout_backends(de: DesktopEnvironment, session: SessionKind) -> Vec<LayoutBackend> {
    let mut chain = match de {
        DesktopEnvironment::GNOME => vec![LayoutBackend::GnomeGsettings, LayoutBackend::KdeKxkbrc],
        _ => vec![LayoutBackend::KdeKxkbrc, LayoutBackend::GnomeGsettings],
    };
    if session == SessionKind::X11 {
        chain.push(LayoutBackend::X11Setxkbmap);
    }
    chain
}

/// 会话显示服务器类型（判定顺序与 §16.1 一致：`XDG_SESSION_TYPE` 优先）。
fn detect_session_kind() -> SessionKind {
    match std::env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => return SessionKind::Wayland,
        Ok("x11") => return SessionKind::X11,
        _ => {}
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        SessionKind::Wayland
    } else if std::env::var_os("DISPLAY").is_some() {
        SessionKind::X11
    } else {
        SessionKind::Unknown
    }
}

/// 执行 CLI 并返回 stdout（仅成功时）。
///
/// 分类：二进制缺失（NotFound）→ BackendUnavailable；超时 → Timeout（`kill_on_drop`
/// 回收子进程，不留孤儿）；非零退出、其它 spawn 失败 → Other（附底层 stderr）。
pub(crate) async fn run_command(bin: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(QUERY_TIMEOUT, cmd.output()).await {
        Err(_) => Err(AgentShellError::Timeout(format!(
            "{} {} timed out after {QUERY_TIMEOUT:?}",
            bin.display(),
            args.join(" ")
        ))),
        Ok(Err(e)) if e.kind() == ErrorKind::NotFound => Err(AgentShellError::BackendUnavailable(
            format!("{} not installed", bin.display()),
        )),
        Ok(Err(e)) => Err(AgentShellError::Other(
            format!("{} spawn: {e}", bin.display()).into(),
        )),
        Ok(Ok(out)) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Ok(Ok(out)) => Err(AgentShellError::Other(
            format!(
                "{} {}: {}",
                bin.display(),
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into(),
        )),
    }
}

/// 生产配置目录：`$XDG_CONFIG_HOME`，否则 `$HOME/.config`。
///
/// 两者都缺失时退化为空路径（相对名 `kxkbrc`），不 panic。
fn config_home_from_env() -> PathBuf {
    if let Some(dir) = non_empty_env("XDG_CONFIG_HOME") {
        return PathBuf::from(dir);
    }
    match non_empty_env("HOME") {
        Some(home) => PathBuf::from(home).join(".config"),
        None => PathBuf::new(),
    }
}

/// 读取环境变量，空串按未设置处理（`XDG_CONFIG_HOME=` 会把配置目录拼成相对名）。
fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::stub_script;

    /// 注入均不存在后端的装配（链路耗尽用例专用，只读不写）。
    fn exhausted(de: DesktopEnvironment, session: SessionKind) -> DesktopPeripherals {
        let base = PathBuf::from("/nonexistent-agent-shell-peripherals");
        DesktopPeripherals::for_test(
            de,
            session,
            None,
            base.join("config"),
            base.join("gsettings"),
            base.join("setxkbmap"),
        )
    }

    #[tokio::test]
    async fn touchpad_chain_exhausted_names_each_segment() {
        let _guard = crate::testsupport::fork_guard().await;
        let err = exhausted(DesktopEnvironment::KDE, SessionKind::X11)
            .touchpad_status()
            .await
            .unwrap_err();
        let AgentShellError::BackendUnavailable(msg) = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        assert!(msg.contains("kde-kwin"), "got {msg}");
        assert!(msg.contains("gnome-gsettings"), "got {msg}");
    }

    #[tokio::test]
    async fn layout_chain_exhausted_names_each_segment() {
        let _guard = crate::testsupport::fork_guard().await;
        let err = exhausted(DesktopEnvironment::KDE, SessionKind::X11)
            .keyboard_layouts()
            .await
            .unwrap_err();
        let AgentShellError::BackendUnavailable(msg) = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        for seg in ["kde-kxkbrc", "gnome-gsettings", "x11-setxkbmap"] {
            assert!(msg.contains(seg), "missing {seg} in {msg}");
        }
    }

    #[tokio::test]
    async fn detected_de_backend_is_tried_first() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KXKBRC_FILE), "[Layout]\nLayoutList=us\n").unwrap();
        let gsettings = stub_script(dir.path(), "gsettings", "echo \"[('xkb', 'de')]\"");
        let absent = dir.path().join("absent");

        // KDE 会话：kxkbrc 段在链首，即便 gsettings 也能回答。
        let kde_session = DesktopPeripherals::for_test(
            DesktopEnvironment::KDE,
            SessionKind::X11,
            None,
            dir.path().to_path_buf(),
            gsettings.clone(),
            absent.clone(),
        );
        let kbd = kde_session.keyboard_layouts().await.unwrap();
        assert_eq!(kbd.source, "kde-kxkbrc");
        assert_eq!(kbd.layouts[0].layout, "us");

        // GNOME 会话：gsettings 段在链首，即便 kxkbrc 也存在。
        let gnome_session = DesktopPeripherals::for_test(
            DesktopEnvironment::GNOME,
            SessionKind::X11,
            None,
            dir.path().to_path_buf(),
            gsettings,
            absent,
        );
        let kbd = gnome_session.keyboard_layouts().await.unwrap();
        assert_eq!(kbd.source, "gnome-gsettings");
        assert_eq!(kbd.layouts[0].layout, "de");
    }

    #[test]
    fn setxkbmap_only_joins_the_chain_in_x11_sessions() {
        // Wayland 下 setxkbmap 读的是 XWayland 键映射，与合成器布局可能不同，
        // 不得作为会话布局来源（否则会给出「成功但错」的答案）。
        for de in [
            DesktopEnvironment::KDE,
            DesktopEnvironment::GNOME,
            DesktopEnvironment::Unknown,
        ] {
            assert!(
                !layout_backends(de, SessionKind::Wayland).contains(&LayoutBackend::X11Setxkbmap),
                "{de:?} Wayland chain must not include setxkbmap"
            );
            assert!(
                !layout_backends(de, SessionKind::Unknown).contains(&LayoutBackend::X11Setxkbmap),
                "{de:?} unknown-session chain must not include setxkbmap"
            );
            assert!(
                layout_backends(de, SessionKind::X11).contains(&LayoutBackend::X11Setxkbmap),
                "{de:?} X11 chain must keep setxkbmap as last resort"
            );
        }
    }

    #[tokio::test]
    async fn wayland_session_does_not_fall_back_to_setxkbmap() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        // kxkbrc 缺失 + gsettings 不可用：Wayland 会话必须直接失败，而不是拿
        // XWayland 的 setxkbmap 结果冒充会话布局。放一个「能被调用」的桩，
        // 一旦被调用就会产出布局，从而把误回退显形。
        let gsettings = stub_script(dir.path(), "gsettings", "echo boom >&2; exit 1");
        // 桩在被调用时留下痕迹：调用即写 calls 文件，用于断言「压根没走这条链」。
        let calls = dir.path().join("setxkbmap.calls");
        let setxkbmap = stub_script(
            dir.path(),
            "setxkbmap",
            &format!("printf 'layout: de\\n'; touch {}", calls.display()),
        );
        let svc = DesktopPeripherals::for_test(
            DesktopEnvironment::KDE,
            SessionKind::Wayland,
            None,
            dir.path().to_path_buf(),
            gsettings,
            setxkbmap.clone(),
        );
        let err = svc.keyboard_layouts().await.unwrap_err();
        let AgentShellError::BackendUnavailable(msg) = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        assert!(!msg.contains("x11-setxkbmap"), "unexpected fallback: {msg}");
        assert!(
            !calls.exists(),
            "setxkbmap must not be invoked in a Wayland session"
        );
    }

    #[tokio::test]
    async fn gnome_no_such_schema_falls_through_backend_chain() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        // gsettings 存在但 schema 不可用：拔掉该段后必须继续走链并汇总原因。
        let gsettings = stub_script(
            dir.path(),
            "gsettings",
            "echo \"No such schema 'org.gnome.desktop.input-sources'\" >&2; exit 1",
        );
        let svc = DesktopPeripherals::for_test(
            DesktopEnvironment::GNOME,
            SessionKind::X11,
            None,
            dir.path().to_path_buf(),
            gsettings,
            dir.path().join("absent"),
        );
        let err = svc.keyboard_layouts().await.unwrap_err();
        let AgentShellError::BackendUnavailable(msg) = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        assert!(msg.contains("No such schema"), "got {msg}");
        assert!(msg.contains("x11-setxkbmap"), "got {msg}");
    }
}
