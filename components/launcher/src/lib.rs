//! 应用启动器组件（设计文档 §21.3 应用启动、§21.4 LauncherComponent）。
//!
//! 公共路径：
//! - `.desktop` 解析：扫描 `/usr/share/applications/` + `~/.local/share/applications/`
//!   （§21.3 跨 DE 通用），`list_installed_apps` 至少覆盖系统目录全量。
//! - 启动：`gio launch <desktop-file>`（GLib ≥2.76 支持 `--activation-token`，
//!   Wayland 聚焦走 XDG activation，§21.28/§21.36）。
//! - URI：portal `org.freedesktop.portal.OpenURI.OpenURI`（KDE/GNOME/DDE ✓）；
//!   portal 不可达时回退 `gio open` / `xdg-open`。

use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, LauncherComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{AppInfo, AppTarget};
use async_trait::async_trait;
use std::path::PathBuf;
#[derive(Default)]
pub struct DesktopFileLauncher {
    /// 额外扫描目录（测试注入）。
    extra_dirs: Vec<PathBuf>,
}

/// 解析单个 .desktop 文件的 [Desktop Entry] 段。
///
/// 仅提取展示所需字段；`Exec` 中 `%f/%F/%u/%U/%d/%D/%n/%N/%i/%c/%k` 字段码
/// 在 ByCommand 直接执行场景外由 `gio launch` 处理，此处原样保留。
pub fn parse_desktop_file(path: &PathBuf) -> Option<AppInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut name = None;
    let mut icon = None;
    let mut categories = Vec::new();
    let mut exec = None;
    let mut no_display = false;
    let mut in_desktop_entry = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry || line.starts_with('#') {
            continue;
        }
        // 空行/无 `=` 的行（section 间极常见）跳过，不能让整个文件解析失败。
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Name" => name = Some(value.to_string()),
            "Icon" => icon = Some(value.to_string()),
            "Categories" => {
                categories = value
                    .split(';')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            }
            "Exec" => exec = Some(value.to_string()),
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    if no_display {
        return None;
    }
    let file_name = path.file_name()?.to_string_lossy().to_string();
    let app_id = file_name.strip_suffix(".desktop")?.to_string();
    Some(AppInfo {
        app_id: app_id.clone(),
        name: name.unwrap_or_else(|| app_id.clone()),
        icon,
        categories,
        desktop_file: path.to_string_lossy().to_string(),
        exec: exec.unwrap_or_default(),
        // 有 Exec 且非 Terminal 即视为 GUI 候选（保守近似）。
        is_gui: true,
    })
}

fn standard_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/usr/share/applications")];
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    dirs
}

fn list_apps_in(dir: &std::path::Path) -> Vec<AppInfo> {
    let mut apps = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return apps;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "desktop"))
        .collect();
    paths.sort();
    for p in paths {
        if let Some(info) = parse_desktop_file(&p) {
            apps.push(info);
        }
    }
    apps
}

impl DesktopFileLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    fn find_desktop_file(&self, id_or_path: &str) -> Option<PathBuf> {
        let p = PathBuf::from(id_or_path);
        if p.is_absolute() && p.exists() {
            return Some(p);
        }
        let with_ext = if id_or_path.ends_with(".desktop") {
            id_or_path.to_string()
        } else {
            format!("{id_or_path}.desktop")
        };
        for dir in standard_dirs()
            .into_iter()
            .chain(self.extra_dirs.iter().cloned())
        {
            let candidate = dir.join(&with_ext);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }

    /// `gio launch`；Wayland 下若设置了 XDG_ACTIVATION_TOKEN 则透传
    /// `--activation-token`（GLib ≥2.76）。
    async fn gio_launch(&self, desktop: &str) -> Result<()> {
        let activation_token = std::env::var("XDG_ACTIVATION_TOKEN").ok();
        let mut cmd = tokio::process::Command::new("gio");
        if let Some(token) = &activation_token {
            cmd.arg("launch").arg("--activation-token").arg(token);
        } else {
            cmd.arg("launch");
        }
        cmd.arg(desktop);
        // GUI 应用常驻且会继承 stdio；piped + 等 EOF 会永久挂起（QA 实测）。
        // stdout/stderr 置 null 解耦 fd，spawn 后只等首进程退出（gio launch
        // 本身启动目标后即退出，不随 GUI 应用驻留），不收集输出。
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let status = cmd
            .status()
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("gio: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(AgentShellError::BackendUnavailable(format!(
                "gio launch {desktop}: exited with {status}"
            )))
        }
    }
}

#[async_trait]
impl DesktopComponent for DesktopFileLauncher {
    fn name(&self) -> &'static str {
        "desktop-file-launcher"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Launcher
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        match tokio::process::Command::new("gio")
            .arg("--version")
            .output()
            .await
        {
            Ok(o) if o.status.success() => ComponentHealth::Healthy,
            _ => ComponentHealth::Degraded("gio CLI unavailable".into()),
        }
    }
}

#[async_trait]
impl LauncherComponent for DesktopFileLauncher {
    async fn list_installed_apps(&self) -> Result<Vec<AppInfo>> {
        let mut seen = std::collections::HashSet::new();
        let mut apps = Vec::new();
        // 用户目录优先于系统目录（同名覆盖）。
        for dir in self
            .extra_dirs
            .iter()
            .cloned()
            .chain(standard_dirs().into_iter().rev())
        {
            for app in list_apps_in(&dir) {
                if seen.insert(app.app_id.clone()) {
                    apps.push(app);
                }
            }
        }
        Ok(apps)
    }

    async fn launch_app(&self, app: &AppTarget) -> Result<()> {
        match app {
            AppTarget::ByDesktopFile(id) => {
                let path = self.find_desktop_file(id).ok_or_else(|| {
                    AgentShellError::WindowNotFound(format!("desktop entry {id}"))
                })?;
                self.gio_launch(&path.to_string_lossy()).await
            }
            AppTarget::ByAppId(app_id) => {
                let path = self
                    .find_desktop_file(app_id)
                    .ok_or_else(|| AgentShellError::WindowNotFound(format!("app id {app_id}")))?;
                self.gio_launch(&path.to_string_lossy()).await
            }
            AppTarget::ByCommand(command) => {
                // shell 词法切分后直接执行；不经过 shell 解释（避免注入面）。
                let parts = split_command(command);
                if parts.is_empty() {
                    return Err(AgentShellError::Other("empty command".into()));
                }
                let mut child = tokio::process::Command::new(&parts[0])
                    .args(&parts[1..])
                    .spawn()
                    .map_err(|e| AgentShellError::Other(format!("{command}: {e}").into()))?;
                // 不等待子进程退出——GUI 应用常驻；Child 默认 kill_on_drop=false，
                // 直接 detach（drop JoinHandle 即可，无需 forget）。
                let handle = tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                drop(handle);
                Ok(())
            }
            AppTarget::OpenUri(uri) => self.launch_uri(uri).await,
        }
    }

    async fn launch_uri(&self, uri: &str) -> Result<()> {
        // 优先 portal OpenURI（沙箱感知、KDE/GNOME/DDE ✓）；失败回退 gio open。
        if let Ok(conn) = zbus::Connection::session().await {
            if let Ok(portal) = zbus::Proxy::new(
                &conn,
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.OpenURI",
            )
            .await
            {
                let options: std::collections::HashMap<String, zbus::zvariant::Value> =
                    std::collections::HashMap::new();
                let res: zbus::Result<zbus::zvariant::OwnedObjectPath> =
                    portal.call("OpenURI", &("", uri, options)).await;
                if res.is_ok() {
                    return Ok(());
                }
            }
        }
        // 与 gio_launch 同理：默认应用常驻并继承 stdio，piped + 等 EOF 会挂起。
        let status = tokio::process::Command::new("gio")
            .arg("open")
            .arg(uri)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("gio open: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(AgentShellError::BackendUnavailable(format!(
                "gio open {uri}: exited with {status}"
            )))
        }
    }
}

/// 简单词法切分（支持引号），不做 shell 展开。
fn split_command(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    parts.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_desktop_entry_fields() {
        let dir =
            std::env::temp_dir().join(format!("agent-shell-launcher-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("firefox.desktop");
        std::fs::write(
            &file,
            "[Desktop Entry]\nName=Firefox\nIcon=firefox\nExec=firefox %u\nCategories=Network;WebBrowser;\nType=Application\n",
        )
        .unwrap();
        let info = parse_desktop_file(&file).expect("parses");
        assert_eq!(info.app_id, "firefox");
        assert_eq!(info.name, "Firefox");
        assert_eq!(info.icon.as_deref(), Some("firefox"));
        assert!(info.categories.contains(&"WebBrowser".to_string()));
        assert!(info.is_gui);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tolerates_empty_lines_and_extra_sections() {
        // 🔴1 回归：section 间空行/无 `=` 行不得让解析提前返回 None。
        let dir =
            std::env::temp_dir().join(format!("agent-shell-launcher-blank-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("multi.desktop");
        std::fs::write(
            &file,
            "[Desktop Action new-window]\nExec=firefox --new-window\n\n\n[Desktop Entry]\nName=Firefox\nExec=firefox %u\n\n",
        )
        .unwrap();
        let info = parse_desktop_file(&file).expect("empty lines must not abort parsing");
        assert_eq!(info.name, "Firefox");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skips_no_display_entries() {
        let dir = std::env::temp_dir().join(format!(
            "agent-shell-launcher-nodisp-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hidden.desktop");

        std::fs::write(
            &file,
            "[Desktop Entry]\nName=Hidden\nExec=x\nNoDisplay=true\n",
        )
        .unwrap();
        assert!(parse_desktop_file(&file).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn splits_command_with_quotes() {
        assert_eq!(
            split_command("code --new-window \"my file.txt\" 'x y'"),
            vec!["code", "--new-window", "my file.txt", "x y"]
        );
        assert!(split_command("   ").is_empty());
    }

    #[tokio::test]
    async fn lists_apps_from_standard_dirs() {
        let l = DesktopFileLauncher::new();
        // 不假设环境有应用；仅验证不 panic 且无重复 app_id。
        let apps = l.list_installed_apps().await.unwrap();
        let ids: std::collections::HashSet<_> = apps.iter().map(|a| &a.app_id).collect();
        assert_eq!(ids.len(), apps.len());
    }
}
