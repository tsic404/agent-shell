//! 桌面环境检测与后端装配入口。
//!
//! 对应设计文档 `design/08-detection/README.md` §16.1–§16.3：
//! 基于 XDG 环境变量 × D-Bus 服务探测的优先级判定链。
//!
//! # 判定顺序（§16.1，不可调换）
//!
//! 1. `XDG_CURRENT_DESKTOP` 环境变量（首要信号；**DDE 判断必须在 KDE 之前**——
//!    deepin-kwin 注册 `org.kde.KWin` 但 `XDG_CURRENT_DESKTOP=Deepin`）。
//! 2. Wayland 会话：D-Bus 服务名探测（可靠；deepin-kwin 与 org.kde.KWin 同名）。
//! 3. X11 会话：`_NET_SUPPORTING_WM_CHECK` 识别窗口管理器。
//! 4. 纯终端（TTY）：stdin/stdout 是 tty 且无 WAYLAND_DISPLAY/DISPLAY。
//!
//! 全部失败兜底返回 [`DesktopEnvironment::Unknown`]。

use std::env;

use crate::error::{AgentShellError, Result};
use crate::types::DesktopEnvironment;

/// DE 检测结果 + 诊断信息（供 `agent-shell doctor` 的「DE 检测」行渲染，§16.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectionReport {
    /// 判定出的桌面环境。
    pub de: DesktopEnvironment,
    /// 命中的信号来源（如 "XDG_CURRENT_DESKTOP" / "dbus:org.kde.KWin" / "_NET_SUPPORTING_WM_CHECK" / "tty"）。
    pub source: &'static str,
}

impl DetectionReport {
    /// doctor 输出形态：`✓ DE 检测 : DDE (Wayland, deepin-kwin)` 的数据部分。
    ///
    /// 会话口径经注入上下文判定，与 `report.de` 的信号源一致，doctor 可测。
    fn summary_with(&self, ctx: &dyn DetectionContext) -> String {
        let session = if ctx.env("WAYLAND_DISPLAY").is_some() {
            "Wayland"
        } else if ctx.env("DISPLAY").is_some() {
            "X11"
        } else if ctx.is_tty() {
            "TTY"
        } else {
            "unknown-session"
        };
        format!("{} ({}, {})", self.de, session, self.source)
    }
}

/// 检测上下文：环境变量读取 + D-Bus 服务探测，均可注入以便单元测试。
trait DetectionContext {
    fn env(&self, key: &str) -> Option<String>;
    fn dbus_service_exists(&self, name: &str) -> bool;
    fn x11_wm(&self) -> DesktopEnvironment;
    fn is_tty(&self) -> bool;
}

/// 真实进程环境的检测上下文。
struct SystemContext;

impl DetectionContext for SystemContext {
    fn env(&self, key: &str) -> Option<String> {
        env::var(key).ok().filter(|v| !v.is_empty())
    }

    fn dbus_service_exists(&self, name: &str) -> bool {
        dbus_service_exists(name)
    }

    fn x11_wm(&self) -> DesktopEnvironment {
        detect_x11_wm()
    }

    fn is_tty(&self) -> bool {
        is_tty_session()
    }
}

/// 检测桌面环境（设计 §16.1 判定顺序原样落地）。
pub fn detect_desktop_environment() -> DesktopEnvironment {
    detect_report(&SystemContext).de
}

/// 检测并渲染 doctor「DE 检测」行数据部分：`DDE (Wayland, XDG_CURRENT_DESKTOP)`。
pub fn detect_report_for_doctor() -> String {
    detect_report(&SystemContext).summary_with(&SystemContext)
}

/// 按 §16.1 判定顺序执行检测，返回带信号来源的报告。
fn detect_report(ctx: &dyn DetectionContext) -> DetectionReport {
    // 1. XDG_CURRENT_DESKTOP 环境变量（首要信号）
    if let Some(current) = ctx.env("XDG_CURRENT_DESKTOP") {
        let lower = current.to_lowercase();
        let from_xdg = [
            ("deepin", DesktopEnvironment::DDE), // DDE 优先！deepin-kwin 同名 KDE
            ("dde", DesktopEnvironment::DDE),
            ("kde", DesktopEnvironment::KDE),
            ("plasma", DesktopEnvironment::KDE),
            ("gnome", DesktopEnvironment::GNOME),
            ("hyprland", DesktopEnvironment::Hyprland),
            ("sway", DesktopEnvironment::Sway),
            ("cosmic", DesktopEnvironment::Cosmic),
            ("xfce", DesktopEnvironment::XFCE),
            ("cinnamon", DesktopEnvironment::Cinnamon),
            ("budgie", DesktopEnvironment::Budgie),
            ("lxqt", DesktopEnvironment::LXQt),
            ("mate", DesktopEnvironment::MATE),
        ]
        .into_iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, de)| de);

        if let Some(de) = from_xdg {
            return DetectionReport {
                de,
                source: "XDG_CURRENT_DESKTOP",
            };
        }
    }

    // 2. Wayland 会话：检查 D-Bus 服务名（可靠）
    if ctx.env("WAYLAND_DISPLAY").is_some() {
        if ctx.env("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            return DetectionReport {
                de: DesktopEnvironment::Hyprland,
                source: "HYPRLAND_INSTANCE_SIGNATURE",
            };
        }
        if ctx.dbus_service_exists("org.kde.KWin") {
            return DetectionReport {
                de: DesktopEnvironment::KDE,
                source: "dbus:org.kde.KWin",
            }; // deepin-kwin 同名
        }
        if ctx.dbus_service_exists("org.gnome.Shell") {
            return DetectionReport {
                de: DesktopEnvironment::GNOME,
                source: "dbus:org.gnome.Shell",
            };
        }
    }

    // 3. X11 会话：检查 DISPLAY + WM
    if ctx.env("DISPLAY").is_some() {
        let de = ctx.x11_wm();
        return DetectionReport {
            de,
            source: "_NET_SUPPORTING_WM_CHECK",
        };
    }

    // 4. 纯终端（TTY）：无 Wayland/X11，无 DE
    //    判据：stdin/stdout 是 tty 且无 WAYLAND_DISPLAY/DISPLAY
    //    注：此处 WAYLAND_DISPLAY/DISPLAY 检查相对上方分支冗余（照抄 §16.1
    //    原文），保留以保证本分支独立成立、不依赖上游分支顺序。
    if ctx.is_tty() && ctx.env("WAYLAND_DISPLAY").is_none() && ctx.env("DISPLAY").is_none() {
        return DetectionReport {
            de: DesktopEnvironment::Tty,
            source: "tty",
        };
    }

    DetectionReport {
        de: DesktopEnvironment::Unknown,
        source: "none",
    }
}

/// 对 session bus 做 name-has-owner 探测，判断 D-Bus 服务是否存在。
///
/// core 保持协议无关：不直接依赖 zbus，经 `busctl --user status <name>` 探测
/// （服务存在 exit 0 / 不存在或 busctl 缺失 exit 非 0，一律按不存在处理，
/// 检测失败兜底 Unknown）。注意必须带 `status` 子命令——busctl 把首个
/// 非选项参数当命令动词，缺子命令会报 `Unknown command verb`。
pub fn dbus_service_exists(name: &str) -> bool {
    std::process::Command::new("busctl")
        .args(["--user", "status", name])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}
/// 读 `_NET_SUPPORTING_WM_CHECK` 原子属性识别 X11 窗口管理器。
///
/// 经 `xprop -root` 读取 `_NET_SUPPORTING_WM_CHECK` → 子窗口 `_NET_WM_NAME`；
/// 无法识别时兜底 [`DesktopEnvironment::X11Generic`]。
pub fn detect_x11_wm() -> DesktopEnvironment {
    let wm_name = || -> Option<String> {
        // 第一步：root 窗口属性给出 WM 所属的管理窗口 ID。
        // 不能带 `-id ""`（xprop 报 Invalid window id format 恒失败）。
        let out = std::process::Command::new("xprop")
            .args(["-root", "-notype", "_NET_SUPPORTING_WM_CHECK"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let win_id = parse_wm_check_window(&String::from_utf8_lossy(&out.stdout))?;

        let out = std::process::Command::new("xprop")
            .args(["-notype", "-id", &win_id, "_NET_WM_NAME"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_wm_name(&String::from_utf8_lossy(&out.stdout))
    }();

    classify_wm_name(wm_name.as_deref())
}

/// 从 `xprop -root -notype _NET_SUPPORTING_WM_CHECK` 输出取窗口 ID。
///
/// 输出形如 `_NET_SUPPORTING_WM_CHECK: window id # 0x400021`。
/// 异常输出（无 `window id #` 标记）返回 None——防御性解析，
/// 不假设 `#` 必然存在（`rsplit('#').next()` 对无 `#` 文本会返回整行）。
fn parse_wm_check_window(output: &str) -> Option<String> {
    let marker = "window id #";
    let idx = output.find(marker)?;
    let id = output[idx + marker.len()..].trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// 从 `xprop -id <win> _NET_WM_NAME` 输出取 WM 名（小写）。
///
/// 输出形如 `_NET_WM_NAME(UTF8_STRING) = "KWin"`；`= ` 缺失或值为空
/// 返回 None。
fn parse_wm_name(output: &str) -> Option<String> {
    let value = output.split("= ").nth(1)?.trim();
    let name = value.trim_matches('"').to_lowercase();
    (!name.is_empty()).then_some(name)
}
/// WM 名 → DE 分类。deepin/dde 分支必须在 kwin 之前：
/// deepin-kwin 的 `_NET_WM_NAME` 同样含 "kwin"（§16.1 DDE 先于 KDE）。
fn classify_wm_name(name: Option<&str>) -> DesktopEnvironment {
    let lower = name.map(str::to_lowercase);
    match lower.as_deref() {
        Some(n) if n.contains("deepin") || n.contains("dde") => DesktopEnvironment::DDE,
        Some(n) if n.contains("kwin") => DesktopEnvironment::KDE,
        Some(n) if n.contains("gnome shell") || n.contains("mutter") => DesktopEnvironment::GNOME,
        Some(n) if n.contains("hyprland") => DesktopEnvironment::Hyprland,
        Some(n) if n.contains("sway") => DesktopEnvironment::Sway,
        _ => DesktopEnvironment::X11Generic,
    }
}

/// 基于 std 的纯终端判定（stdin 或 stdout 是 tty 即成立）。
///
/// `std::io::IsTerminal`（Rust 1.70+）内部即 isatty，且无需 unsafe。
pub fn is_tty_session() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() || std::io::stdout().is_terminal()
}

/// 后端装配入口（设计 §16.2）：match 分发到各合成器组件实现。
///
/// 各 `*Compositor` 具体实现由 T1 提供；T3c 阶段合成器 crate 尚未提供
/// `CompositorComponent` 实现，本函数保留 §16.2 match 骨架并以 `assemble`
/// 特性门控：
/// - 未启用（默认）：对全部 DE 返回 [`AgentShellError::UnsupportedDE`]。
/// - 启用：按骨架分发到具体后端；T1 就绪前各装配分支暂返回
///   [`AgentShellError::NotImplemented`]，非装配 DE 仍返回 `UnsupportedDE`。
#[cfg(not(feature = "assemble"))]
pub fn assemble_wm(
    de: DesktopEnvironment,
) -> Result<Box<dyn crate::component::CompositorComponent>> {
    Err(AgentShellError::UnsupportedDE(format!("{de:?}")))
}

#[cfg(feature = "assemble")]
pub fn assemble_wm(
    de: DesktopEnvironment,
) -> Result<Box<dyn crate::component::CompositorComponent>> {
    // §16.2 match 骨架：7 个装配 DE 分发到具体后端（`*Compositor::new()`
    // 由 T1 提供，就绪前暂以 NotImplemented 占位）；其余 DE 返回 UnsupportedDE。
    match de {
        DesktopEnvironment::DDE
        | DesktopEnvironment::KDE
        | DesktopEnvironment::GNOME
        | DesktopEnvironment::Hyprland
        | DesktopEnvironment::Sway
        | DesktopEnvironment::WLRWayland
        | DesktopEnvironment::X11Generic => Err(AgentShellError::NotImplemented(format!(
            "compositor backend for {de:?} lands with T1"
        ))),
        _ => Err(AgentShellError::UnsupportedDE(format!("{de:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 注入式检测上下文：环境变量来自表，D-Bus/X11/tty 可编程。
    struct FakeCtx {
        envs: HashMap<&'static str, String>,
        dbus: Vec<&'static str>,
        tty: bool,
        x11_wm: DesktopEnvironment,
    }

    impl FakeCtx {
        fn new(envs: &[(&'static str, &str)]) -> Self {
            Self {
                envs: envs.iter().map(|(k, v)| (*k, v.to_string())).collect(),
                dbus: Vec::new(),
                tty: false,
                x11_wm: DesktopEnvironment::X11Generic,
            }
        }

        fn with_dbus(mut self, services: &[&'static str]) -> Self {
            self.dbus = services.to_vec();
            self
        }
    }

    impl Default for FakeCtx {
        fn default() -> Self {
            Self::new(&[])
        }
    }

    impl DetectionContext for FakeCtx {
        fn env(&self, key: &str) -> Option<String> {
            self.envs.get(key).cloned()
        }

        fn dbus_service_exists(&self, name: &str) -> bool {
            self.dbus.contains(&name)
        }

        fn x11_wm(&self) -> DesktopEnvironment {
            self.x11_wm
        }

        fn is_tty(&self) -> bool {
            self.tty
        }
    }

    fn detect(ctx: &FakeCtx) -> DesktopEnvironment {
        detect_report(ctx).de
    }

    #[test]
    fn xdg_deepin_yields_dde_and_beats_kde() {
        // Deepin 会话：deepin-kwin 注册 org.kde.KWin 但 XDG_CURRENT_DESKTOP=Deepin。
        let ctx = FakeCtx::new(&[("XDG_CURRENT_DESKTOP", "Deepin")]).with_dbus(&["org.kde.KWin"]);
        assert_eq!(detect(&ctx), DesktopEnvironment::DDE);
    }

    #[test]
    fn xdg_dde_alias_yields_dde_before_kde() {
        let ctx = FakeCtx::new(&[("XDG_CURRENT_DESKTOP", "DDE")]);
        assert_eq!(detect(&ctx), DesktopEnvironment::DDE);
    }

    #[test]
    fn xdg_kde_yields_kde() {
        let ctx = FakeCtx::new(&[("XDG_CURRENT_DESKTOP", "KDE")]);
        assert_eq!(detect(&ctx), DesktopEnvironment::KDE);

        let ctx = FakeCtx::new(&[("XDG_CURRENT_DESKTOP", "plasma")]);
        assert_eq!(detect(&ctx), DesktopEnvironment::KDE);
    }

    #[test]
    fn wayland_hyprland_signature_wins_without_xdg() {
        let ctx = FakeCtx::new(&[
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("HYPRLAND_INSTANCE_SIGNATURE", "sig_173251"),
        ]);
        assert_eq!(detect(&ctx), DesktopEnvironment::Hyprland);
    }

    #[test]
    fn wayland_dbus_probe_resolves_kde_and_gnome() {
        let ctx = FakeCtx::new(&[("WAYLAND_DISPLAY", "wayland-0")]).with_dbus(&["org.gnome.Shell"]);
        assert_eq!(detect(&ctx), DesktopEnvironment::GNOME);

        let ctx = FakeCtx::new(&[("WAYLAND_DISPLAY", "wayland-0")]).with_dbus(&["org.kde.KWin"]);
        assert_eq!(detect(&ctx), DesktopEnvironment::KDE);
    }

    #[test]
    fn three_no_env_plus_tty_yields_tty() {
        let ctx = FakeCtx {
            tty: true,
            ..FakeCtx::default()
        };
        assert_eq!(detect(&ctx), DesktopEnvironment::Tty);
    }

    #[test]
    fn fully_empty_context_yields_unknown() {
        // 非 tty 且无任何显示服务器信号 → Unknown。
        assert_eq!(detect(&FakeCtx::default()), DesktopEnvironment::Unknown);
    }

    #[test]
    fn x11_display_uses_wm_check_result() {
        let mut ctx = FakeCtx::new(&[("DISPLAY", ":0")]);
        ctx.x11_wm = DesktopEnvironment::KDE;
        assert_eq!(detect(&ctx), DesktopEnvironment::KDE);
    }

    #[test]
    fn detection_report_summary_formats_for_doctor() {
        let ctx = FakeCtx::new(&[
            ("XDG_CURRENT_DESKTOP", "Deepin"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ]);
        let report = detect_report(&ctx);
        assert_eq!(report.de, DesktopEnvironment::DDE);
        assert_eq!(report.source, "XDG_CURRENT_DESKTOP");
        assert!(
            report.summary_with(&ctx).starts_with("DDE (Wayland,"),
            "{}",
            report.summary_with(&ctx)
        );
    }

    #[test]
    fn assemble_wm_rejects_non_assemblable_des() {
        // 验收：8 个非装配 DE 返回 AgentShellError::UnsupportedDE。
        for de in [
            DesktopEnvironment::Budgie,
            DesktopEnvironment::XFCE,
            DesktopEnvironment::Cinnamon,
            DesktopEnvironment::Cosmic,
            DesktopEnvironment::LXQt,
            DesktopEnvironment::MATE,
            DesktopEnvironment::Tty,
            DesktopEnvironment::Unknown,
        ] {
            let err = match assemble_wm(de) {
                Err(e) => e,
                Ok(_) => panic!("{de:?}: expected UnsupportedDE, got backend"),
            };
            assert!(
                matches!(&err, AgentShellError::UnsupportedDE(_)),
                "{de:?}: unexpected error {err:?}"
            );
        }
    }

    #[cfg(feature = "assemble")]
    #[test]
    fn assemble_wm_placeholder_for_assemblable_des() {
        // 7 个装配 DE：T1 就绪前返回 NotImplemented 占位（两种 feature 配置一致）。
        for de in [
            DesktopEnvironment::DDE,
            DesktopEnvironment::KDE,
            DesktopEnvironment::GNOME,
            DesktopEnvironment::Hyprland,
            DesktopEnvironment::Sway,
            DesktopEnvironment::WLRWayland,
            DesktopEnvironment::X11Generic,
        ] {
            let err = match assemble_wm(de) {
                Err(e) => e,
                Ok(_) => panic!("{de:?}: expected placeholder error, got backend"),
            };
            assert!(
                matches!(&err, AgentShellError::NotImplemented(_)),
                "{de:?}: unexpected error {err:?}"
            );
        }
    }

    #[test]
    fn parse_wm_check_window_extracts_id() {
        let out = "_NET_SUPPORTING_WM_CHECK: window id # 0x400021\n";
        assert_eq!(parse_wm_check_window(out), Some("0x400021".to_string()));
    }

    #[test]
    fn parse_wm_check_window_rejects_malformed_output() {
        // 无 `window id #` 标记 → None（防御 rsplit 对无 # 文本返回整行）。
        assert_eq!(
            parse_wm_check_window("_NET_SUPPORTING_WM_CHECK: not found"),
            None
        );
        // 标记后为空 → None。
        assert_eq!(parse_wm_check_window("window id #   "), None);
        assert_eq!(parse_wm_check_window(""), None);
    }

    #[test]
    fn parse_wm_name_extracts_lowercase_name() {
        let out = "_NET_WM_NAME(UTF8_STRING) = \"KWin\"\n";
        assert_eq!(parse_wm_name(out), Some("kwin".to_string()));
    }

    #[test]
    fn parse_wm_name_rejects_missing_or_empty_value() {
        assert_eq!(parse_wm_name("_NET_WM_NAME(UTF8_STRING) = "), None);
        // 无 `= ` 分隔符（如属性缺失时的 `_NET_WM_NAME: not found.`）→ None。
        assert_eq!(parse_wm_name("_NET_WM_NAME: not found."), None);
        assert_eq!(parse_wm_name(""), None);
    }

    #[test]
    fn classify_wm_name_prefers_deepin_over_kwin() {
        // deepin-kwin 的 _NET_WM_NAME 含 "kwin"——DDE 判断必须先于 KDE（§16.1）。
        assert_eq!(
            classify_wm_name(Some("deepin-kwin")),
            DesktopEnvironment::DDE
        );
        assert_eq!(
            classify_wm_name(Some("dde-desktop")),
            DesktopEnvironment::DDE
        );
        assert_eq!(classify_wm_name(Some("kwin")), DesktopEnvironment::KDE);
        assert_eq!(
            classify_wm_name(Some("GNOME Shell")),
            DesktopEnvironment::GNOME
        );
        assert_eq!(classify_wm_name(Some("mutter")), DesktopEnvironment::GNOME);
        assert_eq!(
            classify_wm_name(Some("Hyprland")),
            DesktopEnvironment::Hyprland
        );
        assert_eq!(classify_wm_name(Some("sway")), DesktopEnvironment::Sway);
        // 未知名与探测失败均兜底 X11Generic。
        assert_eq!(
            classify_wm_name(Some("icewm")),
            DesktopEnvironment::X11Generic
        );
        assert_eq!(classify_wm_name(None), DesktopEnvironment::X11Generic);
    }
}
