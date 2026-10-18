//! KWin 私有 Wayland 接口授权探测（`.desktop` 的 `X-KDE-Wayland-Interfaces`）。
//!
//! KWin ≤6.7.x 把 `org_kde_kwin_fake_input` / `org_kde_plasma_window_management`
//! 列入 `interfacesBlackList`：只向在 .desktop 条目里声明了对应接口的客户端通告
//! registry global，条目按 `Exec` 首词的规范路径与客户端 `/proc/<pid>/exe` 比对
//! （KWin `src/utils/serviceutils.h` 的 `fetchProcessServiceField`）。未授权时
//! doctor 看到的是「protocol 不存在」的假象，实际是缺授权条目。
//!
//! 本模块只**探测**（C-027：健康体检只探测与呈现、不修复），供 doctor 的输入注入
//! 行回答「授权条目在不在、指向哪个二进制」，让 `--at` 全链失败的排障有落点。

use std::path::{Path, PathBuf};

/// 输入注入所需的授权接口（注入行判定只看它；窗口管理接口另有其槽）。
pub const FAKE_INPUT_INTERFACE: &str = "org_kde_kwin_fake_input";

/// 授权探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntitlementState {
    /// 有条目声明了该接口且 `Exec` 指向该二进制。
    Granted { entry: PathBuf },
    /// 有条目的 `Exec` 指向该二进制，但未声明该接口。
    InterfaceMissing { entry: PathBuf },
    /// 没有任何条目的 `Exec` 指向该二进制。
    NoEntry { scanned: Vec<PathBuf> },
}

/// 按 XDG 规范的标准 applications 目录探测。
pub fn probe(executable: &Path) -> EntitlementState {
    probe_in(executable, &applications_dirs())
}

/// 同 [`probe`]，目录列表由调用方给出（测试与自定义数据目录用）。
pub fn probe_in(executable: &Path, applications_dirs: &[PathBuf]) -> EntitlementState {
    let executable = canonical(executable);
    let mut scanned = Vec::new();
    for dir in applications_dirs {
        scanned.push(dir.clone());
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(desktop) = parse_desktop_entry(&content) else {
                continue;
            };
            if desktop.exec.as_deref().map(canonical) != Some(executable.clone()) {
                continue;
            }
            if desktop.declares_interface(FAKE_INPUT_INTERFACE) {
                return EntitlementState::Granted { entry: path };
            }
            return EntitlementState::InterfaceMissing { entry: path };
        }
    }
    EntitlementState::NoEntry { scanned }
}

/// 单行授权状态摘要（doctor 输入注入行；未授权时给出可执行的修复落点）。
pub fn hint() -> String {
    let executable = std::env::current_exe().unwrap_or_default();
    match probe(&executable) {
        EntitlementState::Granted { entry } => format!(
            "授权条目 {} 已声明 {FAKE_INPUT_INTERFACE}（KWin 尚未通告，重启会话后重试）",
            entry.display()
        ),
        EntitlementState::InterfaceMissing { entry } => format!(
            "授权条目 {} 未声明 {FAKE_INPUT_INTERFACE}（Exec 已指向本二进制）",
            entry.display()
        ),
        EntitlementState::NoEntry { scanned } => format!(
            "无 Exec={} 的授权条目（已查 {}）",
            executable.display(),
            scanned
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// 标准 applications 目录：`$XDG_DATA_HOME`，然后是 `$XDG_DATA_DIRS`（缺省值
/// 按 XDG 规范），各加 `applications/` 子目录。
pub fn applications_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(home) = data_home {
        dirs.push(home.join("applications"));
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.trim().is_empty())
            .map(|d| PathBuf::from(d).join("applications")),
    );
    dirs
}

/// 条目里与授权相关的两个字段（`[Desktop Entry]` 组内取值）。
#[derive(Debug, Default, PartialEq, Eq)]
struct DesktopEntry {
    exec: Option<PathBuf>,
    interfaces: Vec<String>,
}

impl DesktopEntry {
    fn declares_interface(&self, interface: &str) -> bool {
        self.interfaces.iter().any(|i| i == interface)
    }
}

/// 解析 `[Desktop Entry]` 组：`Exec` 取首词（与 KWin `QProcess::splitCommand`
/// 同口径），接口按逗号分隔（KWin 该键是逗号分隔的字符串列表）。
fn parse_desktop_entry(content: &str) -> Option<DesktopEntry> {
    let mut entry = DesktopEntry::default();
    let mut in_group = false;
    for line in content.lines() {
        let line = line.trim();
        if let Some(group) = line.strip_prefix('[') {
            in_group = group.trim_end_matches(']').trim() == "Desktop Entry";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Exec" => entry.exec = value.split_whitespace().next().map(PathBuf::from),
            "X-KDE-Wayland-Interfaces" => {
                entry.interfaces = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
    }
    entry.exec.as_ref()?;
    Some(entry)
}

/// 规范路径；路径不存在（测试夹具、已删条目）时退回原值。
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_entry(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write desktop entry");
        path
    }

    #[test]
    fn granted_when_exec_matches_and_interface_declared() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe = tmp.path().join("agent-shell-daemon");
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write exe");
        let apps = tmp.path().join("applications");
        std::fs::create_dir(&apps).expect("mkdir");
        write_entry(
            &apps,
            "org.agentshell.Daemon.desktop",
            &format!(
                "[Desktop Entry]\nType=Application\nExec={} --idle-timeout-secs 1800\n\
                 X-KDE-Wayland-Interfaces=org_kde_plasma_window_management,{FAKE_INPUT_INTERFACE}\n",
                exe.display()
            ),
        );

        assert_eq!(
            probe_in(&exe, std::slice::from_ref(&apps)),
            EntitlementState::Granted {
                entry: apps.join("org.agentshell.Daemon.desktop")
            }
        );
    }

    #[test]
    fn missing_interface_and_missing_entry_are_distinguished() {
        // 「有匹配条目但未声明接口」与「没有匹配条目」是两种修复动作：
        // 前者改条目，后者要新增条目（KWin 匹配 Exec 与 /proc/<pid>/exe）。
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe = tmp.path().join("agent-shell-daemon");
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write exe");
        let apps = tmp.path().join("applications");
        std::fs::create_dir(&apps).expect("mkdir");
        write_entry(
            &apps,
            "agent-shell-daemon.desktop",
            &format!("[Desktop Entry]\nExec={}\n", exe.display()),
        );
        assert_eq!(
            probe_in(&exe, std::slice::from_ref(&apps)),
            EntitlementState::InterfaceMissing {
                entry: apps.join("agent-shell-daemon.desktop")
            }
        );

        // 条目 Exec 指向别的二进制（如打包路径 /usr/bin/agent-shell-daemon）：
        // 源构建的 daemon 不被授权，这是本单实测到的线上形态。
        let other = tmp.path().join("agent-shell-daemon-packaged");
        write_entry(
            &apps,
            "other.desktop",
            &format!(
                "[Desktop Entry]\nExec={}\nX-KDE-Wayland-Interfaces={FAKE_INPUT_INTERFACE}\n",
                other.display()
            ),
        );
        assert_eq!(
            probe_in(&exe, std::slice::from_ref(&apps)),
            EntitlementState::InterfaceMissing {
                entry: apps.join("agent-shell-daemon.desktop")
            },
            "Exec 不匹配的条目不得当作已授权"
        );

        let absent = tmp.path().join("no-such-dir");
        assert_eq!(
            probe_in(&exe, std::slice::from_ref(&absent)),
            EntitlementState::NoEntry {
                scanned: vec![absent]
            }
        );
    }

    #[test]
    fn desktop_entry_lookup_ignores_other_groups_and_actions() {
        // 动作组（`[Desktop Action new]`）里的同名键不得覆盖主组值。
        let parsed = parse_desktop_entry(
            "[Desktop Entry]\nExec=/usr/bin/agent-shell-daemon\n\
             X-KDE-Wayland-Interfaces=org_kde_kwin_fake_input\n\
             \n[Desktop Action new]\nExec=/usr/bin/other\n",
        )
        .expect("entry parsed");
        assert_eq!(
            parsed.exec,
            Some(PathBuf::from("/usr/bin/agent-shell-daemon"))
        );
        assert!(parsed.declares_interface(FAKE_INPUT_INTERFACE));

        // 无 Exec 的条目（如纯 URL handler 片段）不参与匹配。
        assert_eq!(parse_desktop_entry("[Desktop Entry]\nName=x\n"), None);
    }
}
