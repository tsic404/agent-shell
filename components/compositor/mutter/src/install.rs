//! GNOME Shell Extension 安装/启用/状态管理（§8.1「安装流程落盘后启用」）。
//!
//! `extension.js` 与 `metadata.json` 由本 crate 内嵌交付（`include_str!`
//! 单一事实来源），本模块负责把它们落盘到用户扩展目录、标记 user-enabled、
//! 查询与卸载。安装是用户级操作（无 root）：
//!
//! - 落盘：`<data-home>/gnome-shell/extensions/agent-shell-bridge@tsic.top/`
//!   （`$XDG_DATA_HOME` 优先，回退 `$HOME/.local/share`）；打包形态还会
//!   部署到 `/usr/share/gnome-shell/extensions/`（系统级），两处均视为已安装。
//! - 启用：`gnome-extensions enable`（canonical，含 shell 刷新通知），
//!   缺失时 dconf 兜底写 `org/gnome/shell/enabled-extensions`。
//!
//! 纯路径/文件/解析辅助为可单测的纯函数（不依赖真实 GNOME 会话）；启用/
//! 状态查询经外部命令（`gnome-extensions` / `dconf`），仅在有 GNOME 会话的
//! 主机上产生真值，CI/无 GNOME 环境如实报 `false`。

use crate::error::EXTENSION_ID;
use crate::extension::{EXTENSION_JS, EXTENSION_METADATA};
use std::path::{Path, PathBuf};

/// GNOME Shell 系统级扩展根目录（打包部署形态，§20.2）。
const SYSTEM_EXTENSIONS_DIR: &str = "/usr/share/gnome-shell/extensions";

/// 用户数据根目录解析：`$XDG_DATA_HOME` 优先，回退 `$HOME/.local/share`。
/// 两者均缺失返回 `None`（无主目录的环境无法安装，如实报告）。
fn data_home(data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(dh) = data_home.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dh));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/share"))
}

/// 用户扩展安装目录：`<data-home>/gnome-shell/extensions/<uuid>`。
pub fn extension_dir() -> Option<PathBuf> {
    data_home(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .map(|d| d.join("gnome-shell/extensions").join(EXTENSION_ID))
}

/// 候选扩展目录：用户目录优先，系统目录兜底（打包部署形态）。
///
/// `is_installed` 与 `installed_dir` 据此在两种部署模型下都如实报告；
/// `install` 恒写用户目录（无 root），系统目录由打包器部署。
pub fn install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(user) = extension_dir() {
        dirs.push(user);
    }
    dirs.push(PathBuf::from(SYSTEM_EXTENSIONS_DIR).join(EXTENSION_ID));
    dirs
}

/// 扩展是否已安装（用户/系统目录任一「可被当前 GNOME 加载」即视为已安装）。
///
/// 判据 = 文件齐全 + `metadata.json.shell-version` 覆盖当前 shell 主版本
/// （GNOME 按 shell-version 白名单拒绝加载不匹配的扩展）。
pub fn is_installed() -> bool {
    install_dirs().iter().any(|d| is_installed_at(d))
}

/// 已安装扩展的落盘目录（用户优先，用于状态报告 `dir` 字段）。
///
/// 仅判文件齐全（与 [`is_installed`] 不同：不因 shell-version 不匹配而隐藏
/// 真实落盘位置——`dir` 如实报告，`installed` 另判可加载性）。
pub fn installed_dir() -> Option<PathBuf> {
    install_dirs().into_iter().find(|d| files_present_at(d))
}

/// 目录内扩展是否可被当前 GNOME 加载：文件齐全且 shell-version 覆盖当前版本。
fn is_installed_at(dir: &Path) -> bool {
    if !files_present_at(dir) {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(dir.join("metadata.json")) else {
        return false;
    };
    let versions = metadata_shell_versions(&content).unwrap_or_default();
    // 无 shell-version 声明 → GNOME 拒绝加载。
    !versions.is_empty() && shell_version_covers(&versions, current_shell_major())
}

/// 文件是否齐全（`metadata.json` + `extension.js`），忽略 shell-version。
fn files_present_at(dir: &Path) -> bool {
    dir.join("metadata.json").is_file() && dir.join("extension.js").is_file()
}

/// 文件已落盘但 shell-version 不覆盖当前版本（GNOME 拒绝加载）——供状态提示
/// 区分「未安装」与「已落盘但版本不匹配」。
pub fn shell_version_mismatch() -> bool {
    let Some(major) = current_shell_major() else {
        return false; // 非 GNOME / 版本不可探测，无「不匹配」可言。
    };
    install_dirs().iter().any(|d| {
        files_present_at(d)
            && std::fs::read_to_string(d.join("metadata.json"))
                .ok()
                .and_then(|c| metadata_shell_versions(&c))
                .map(|v| !v.is_empty() && !shell_version_covers(&v, Some(major)))
                .unwrap_or(false)
    })
}

/// 解析 metadata.json 的 `shell-version` 数组（纯函数）。
fn metadata_shell_versions(content: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    Some(
        v.get("shell-version")?
            .as_array()?
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
    )
}

/// 判断 shell-version 列表是否覆盖给定主版本（纯函数）。
///
/// `current_major` 为 `None`（非 GNOME / 版本不可探测）时返回 `true`——
/// 无法校验即不误报「未安装」。
fn shell_version_covers(versions: &[String], current_major: Option<u32>) -> bool {
    let Some(major) = current_major else {
        return true;
    };
    versions
        .iter()
        .any(|v| v.split('.').next().and_then(|s| s.parse::<u32>().ok()) == Some(major))
}

/// 当前 GNOME Shell 主版本（major）：`GNOME_SHELL_VERSION` 环境变量优先
/// （GNOME 会话内由 shell 注入），回退 `gnome-shell --version`；两者均不可得
/// 返回 `None`（非 GNOME 会话）。
fn current_shell_major() -> Option<u32> {
    if let Ok(v) = std::env::var("GNOME_SHELL_VERSION") {
        if let Some(major) = parse_major(&v) {
            return Some(major);
        }
    }
    if let Ok(out) = run("gnome-shell", &["--version"]) {
        if let Some(ver) = out.split_whitespace().last() {
            if let Some(major) = parse_major(ver) {
                return Some(major);
            }
        }
    }
    None
}

/// 解析版本串主版本号：`"50.4"` → `50`；非数字返回 `None`。
fn parse_major(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

/// 落盘 extension.js + metadata.json 到用户扩展目录（幂等：覆盖旧版本）。
pub fn install() -> Result<PathBuf, String> {
    let dir = extension_dir().ok_or_else(|| {
        "cannot resolve extension dir (XDG_DATA_HOME and HOME both unset)".to_string()
    })?;
    install_into(&dir)?;
    Ok(dir)
}

/// 写两个文件到指定目录（纯函数，测试注入 tempdir）。
fn install_into(dir: &Path) -> Result<(), String> {
    write_file(&dir.join("metadata.json"), EXTENSION_METADATA)?;
    write_file(&dir.join("extension.js"), EXTENSION_JS)
}

/// 原子写文件：先写 `.tmp` 再 rename（与 portal_sessions 同口径，避免半截文件）。
fn write_file(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))
}

/// 卸载：清启用标记（best-effort）+ 移除用户扩展目录（缺失视为成功——幂等）。
///
/// 系统级目录不在此移除（需 root，由打包器卸载时清理）。
pub fn uninstall() -> Result<(), String> {
    let dir = extension_dir().ok_or_else(|| {
        "cannot resolve extension dir (XDG_DATA_HOME and HOME both unset)".to_string()
    })?;
    // 清理 enabled-extensions：直接走 dconf 兜底（不依赖 gnome-extensions 识别
    // 扩展——GNOME 尚未加载/未识别时 `gnome-extensions disable` 会失败，残留
    // dconf 脏数据）。无 dconf 环境（非 GNOME）无此残留，失败可忽略。
    let _ = dconf_set_enabled(false);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("remove {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// 启用（user-enabled 标记）：`gnome-extensions enable`，缺失时 dconf 兜底。
pub fn enable() -> Result<(), String> {
    if !is_installed() {
        return Err(format!(
            "extension {EXTENSION_ID} not installed; run `agent-shell extension install` first"
        ));
    }
    if which("gnome-extensions").is_some() {
        run("gnome-extensions", &["enable", EXTENSION_ID])
            .map(|_| ())
            .map_err(|e| format!("gnome-extensions enable failed: {e}"))
    } else {
        dconf_set_enabled(true)
    }
}

/// 查询是否 user-enabled：dconf read 优先（headless 可用），gnome-extensions info 兜底。
///
/// 未安装即不可能「已启用」——先短路，避免 `installed=false, enabled=true`
/// 的矛盾状态（dconf 残留 enabled 条目而文件已卸载）。
pub fn is_enabled() -> bool {
    if !is_installed() {
        return false;
    }
    if which("dconf").is_some() {
        if let Ok(out) = run("dconf", &["read", "/org/gnome/shell/enabled-extensions"]) {
            return out.contains(&format!("'{EXTENSION_ID}'"));
        }
    }
    if which("gnome-extensions").is_some() {
        if let Ok(out) = run("gnome-extensions", &["info", EXTENSION_ID]) {
            return out.lines().any(|l| l.trim() == "State: ACTIVE");
        }
    }
    false
}

/// dconf 兜底：读现有 `enabled-extensions` 列表、增删本扩展、写回。
fn dconf_set_enabled(enabled: bool) -> Result<(), String> {
    let key = "/org/gnome/shell/enabled-extensions";
    let mut list: Vec<String> = run("dconf", &["read", key])
        .ok()
        .and_then(|out| parse_dconf_list(&out))
        .unwrap_or_default();
    list.retain(|u| u != EXTENSION_ID);
    if enabled {
        list.push(EXTENSION_ID.to_string());
    }
    let serialized = list
        .iter()
        .map(|u| format!("'{u}'"))
        .collect::<Vec<_>>()
        .join(", ");
    run("dconf", &["write", key, &format!("[{serialized}]")]).map(|_| ())
}

/// 解析 dconf read 输出的字符串数组：`['a@x', 'b@y']` → 元素列表。
/// 空数组/未设置（空串）返回空列表；非数组形态返回 `None`。
fn parse_dconf_list(raw: &str) -> Option<Vec<String>> {
    let raw = raw.trim();
    let inner = raw.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|s| {
            s.trim()
                .strip_prefix('\'')?
                .strip_suffix('\'')
                .map(str::to_string)
        })
        .collect()
}

/// 运行外部命令，返回去空白 stdout；缺失/非零退出/非 UTF-8 返回 Err。
fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("spawn {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{program} exited {}", out.status));
    }
    String::from_utf8(out.stdout)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("{program} non-utf8 output: {e}"))
}

/// `$PATH` 中是否存在可执行文件（`which` 的零依赖极简替代）。
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn data_home_prefers_xdg_over_home() {
        assert_eq!(
            data_home(Some("/xdg/data"), Some("/home/u")),
            Some(PathBuf::from("/xdg/data"))
        );
    }

    #[test]
    fn data_home_falls_back_to_home_local_share() {
        assert_eq!(
            data_home(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/share"))
        );
    }

    #[test]
    fn data_home_none_when_both_unset() {
        assert_eq!(data_home(None, None), None);
        assert_eq!(data_home(Some(""), Some("")), None);
    }

    #[test]
    fn install_into_writes_both_files_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("agent-shell-ext-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        install_into(&dir).expect("install into temp dir");
        assert!(files_present_at(&dir));
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("metadata.json")).unwrap())
                .expect("metadata.json is valid JSON");
        assert_eq!(meta["uuid"], EXTENSION_ID);
        assert!(std::fs::read_to_string(dir.join("extension.js"))
            .unwrap()
            .contains("org.gnome.Shell.AgentShell"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn metadata_has_required_shell_extension_fields() {
        let meta: Value = serde_json::from_str(EXTENSION_METADATA).expect("metadata parses");
        assert_eq!(meta["uuid"], EXTENSION_ID);
        assert_eq!(meta["name"], "agent-shell-bridge");
        let versions = meta["shell-version"]
            .as_array()
            .expect("shell-version array");
        assert!(!versions.is_empty(), "shell-version must be non-empty");
        // P0：验收主场景要求 GNOME 50 能加载——shell-version 必须覆盖 "50"。
        assert!(
            versions.iter().any(|v| v == "50"),
            "shell-version must include GNOME 50"
        );
    }

    #[test]
    fn shell_version_covers_current_major() {
        let versions = vec!["45".to_string(), "49".to_string(), "50".to_string()];
        assert!(shell_version_covers(&versions, Some(50)));
        assert!(shell_version_covers(&versions, Some(45)));
        assert!(shell_version_covers(&versions, None)); // 版本不可探测 → 不误报。
        assert!(!shell_version_covers(&versions, Some(51)));
        assert!(!shell_version_covers(&versions, Some(46))); // 缺 46。
    }

    #[test]
    fn metadata_shell_versions_parses_array() {
        assert_eq!(
            metadata_shell_versions(r#"{"shell-version":["45","50"]}"#),
            Some(vec!["45".to_string(), "50".to_string()])
        );
        assert_eq!(
            metadata_shell_versions(r#"{"shell-version":[]}"#),
            Some(Vec::new())
        );
        assert_eq!(metadata_shell_versions(r#"{}"#), None);
        assert_eq!(metadata_shell_versions("not json"), None);
    }

    #[test]
    fn parse_major_extracts_leading_number() {
        assert_eq!(parse_major("50.4"), Some(50));
        assert_eq!(parse_major("50"), Some(50));
        assert_eq!(parse_major("abc"), None);
        assert_eq!(parse_major(""), None);
    }

    #[test]
    fn extension_js_is_non_empty_esm() {
        // GNOME 45+ ESM 入口：默认导出 Extension 子类（非 init() 返回对象）。
        assert!(EXTENSION_JS.contains("export default class"));
        assert!(EXTENSION_JS.contains("org.gnome.Shell.AgentShell"));
    }

    #[test]
    fn parse_dconf_list_roundtrips_quoted_elements() {
        assert_eq!(
            parse_dconf_list("['a@x', 'b@y']"),
            Some(vec!["a@x".to_string(), "b@y".to_string()])
        );
        assert_eq!(parse_dconf_list("[]"), Some(Vec::new()));
        assert_eq!(parse_dconf_list(""), None);
        assert_eq!(parse_dconf_list("not-a-list"), None);
    }
}
