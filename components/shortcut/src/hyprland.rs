//! Hyprland 全局快捷键后端（hyprctl + 片段配置，§21.33）。
//!
//! Hyprland 没有绑定快捷键的 D-Bus/CLI 持久接口：`hyprctl keyword bind …`
//! 只改运行期状态，重启即失。因此一次绑定落两处——运行期 `hyprctl keyword`，
//! 以及本组件独占的片段文件 `$XDG_CONFIG_HOME/hypr/agent-shell.conf`（主配置
//! `hyprland.conf` 只需 `source` 它一次）。重绑同一组合键只改片段文件里的那一行。

use crate::combo;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::ShortcutBinding;
use agent_shell_core::types::KeyCombo;
use std::path::Path;
use std::time::Duration;

/// 片段文件名（本组件独占，用户可在主配置里 source）。
const FRAGMENT_NAME: &str = "agent-shell.conf";
/// hyprctl 调用超时（§19 hyprctl 通道 2s）。
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// hyprctl 是否可用（能连上当前 Hyprland 实例）。
pub(crate) async fn ready(hyprctl: &Path) -> std::result::Result<(), String> {
    run_hyprctl(hyprctl, &["version"])
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 绑定：运行期下发 + 片段文件落地 + 主配置 source 补写。
pub(crate) async fn bind(
    hyprctl: &Path,
    config_home: &Path,
    combo: &KeyCombo,
    action: &str,
) -> Result<ShortcutBinding> {
    let bind_spec = combo::hyprland_bind(combo).map_err(|e| AgentShellError::Other(e.into()))?;
    let line = bind_line(&bind_spec, action).map_err(|e| AgentShellError::Other(e.into()))?;

    run_hyprctl(hyprctl, &["keyword", "bind", &line]).await?;

    let fragment = config_home.join("hypr").join(FRAGMENT_NAME);
    upsert_fragment(&fragment, &bind_spec, &line)?;
    ensure_sourced(&config_home.join("hypr").join("hyprland.conf"), &fragment)?;

    Ok(ShortcutBinding {
        backend: "hyprland".into(),
        component: Some(fragment.display().to_string()),
        combo: combo::canonical(combo),
        action: action.to_string(),
    })
}

/// 组装 `bind = <组合键>, exec, <action>` 行。
///
/// 换行/控制字符会在这行落进 `hyprland.conf` 的 `source` 片段后开启新的配置
/// 指令（`exec-once` / `monitor` / `bind`），故显式拒绝而非转义——Hyprland 配置
/// 没有可以承载任意文本的引号形式。
fn bind_line(bind_spec: &str, action: &str) -> std::result::Result<String, String> {
    if action.chars().any(char::is_control) {
        return Err("action contains control characters (newline injection)".into());
    }
    Ok(format!("bind = {bind_spec}, exec, {action}"))
}

/// 片段文件里替换/追加该组合键的 bind 行（`bind_spec` 相同的旧行被覆盖）。
fn upsert_fragment(fragment: &Path, bind_spec: &str, line: &str) -> Result<()> {
    let Some(parent) = fragment.parent() else {
        return Err(AgentShellError::Other(
            format!("{} has no parent directory", fragment.display()).into(),
        ));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| AgentShellError::Other(format!("create {}: {e}", parent.display()).into()))?;
    let existing = read_optional(fragment)?.unwrap_or_default();
    let prefix = format!("bind = {bind_spec}, ");
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with(&prefix))
        .map(str::to_string)
        .collect();
    lines.push(line.to_string());
    let mut body = lines.join("\n");
    body.push('\n');
    write_atomic(fragment, &body)
}

/// 主配置补写 `source = <片段>`（已存在则不动；文件不存在则创建）。
fn ensure_sourced(main_conf: &Path, fragment: &Path) -> Result<()> {
    let source_line = format!("source = {}", fragment.display());
    let existing = read_optional(main_conf)?.unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == source_line || l.trim().ends_with(FRAGMENT_NAME))
    {
        return Ok(());
    }
    if let Some(parent) = main_conf.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AgentShellError::Other(format!("create {}: {e}", parent.display()).into())
        })?;
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&source_line);
    body.push('\n');
    write_atomic(main_conf, &body)
}

/// 读文本文件：仅 `NotFound` 视为「没有这个文件」，其余读取失败（权限、非 UTF-8、
/// 路径是目录等）一律上抛。
///
/// 不能把读取失败折叠成空串——调用方随后会用 `write_atomic` 覆盖该路径，读失败
/// 当空文件等于把用户既有配置整份截断成一行。
fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(Some(body)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AgentShellError::Other(
            format!("read {}: {e}", path.display()).into(),
        )),
    }
}

/// 原子替换写入：先写同目录临时文件再 `rename`，写中途失败不截断既有配置。
fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let staged = staged_path(path)?;
    std::fs::write(&staged, body)
        .map_err(|e| AgentShellError::Other(format!("write {}: {e}", staged.display()).into()))?;
    if let Err(e) = std::fs::rename(&staged, path) {
        let _ = std::fs::remove_file(&staged);
        return Err(AgentShellError::Other(
            format!("rename {} -> {}: {e}", staged.display(), path.display()).into(),
        ));
    }
    Ok(())
}

/// 同目录临时文件路径（与目标同文件系统，保证 `rename` 原子）。
fn staged_path(path: &Path) -> Result<std::path::PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| {
            AgentShellError::Other(format!("{} has no file name", path.display()).into())
        })?
        .to_string_lossy()
        .into_owned();
    Ok(match path.parent() {
        Some(parent) => parent.join(format!(".{name}.agent-shell.tmp")),
        None => std::path::PathBuf::from(format!(".{name}.agent-shell.tmp")),
    })
}

/// 执行 hyprctl 并返回 stdout。二进制缺失 → BackendUnavailable；非零退出
/// → Other（含 stderr，如实例未运行时的 "Couldn't connect to …"）。
async fn run_hyprctl(bin: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        CALL_TIMEOUT,
        tokio::process::Command::new(bin)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| AgentShellError::Timeout(format!("hyprctl {} timed out", args.join(" "))))?
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            AgentShellError::BackendUnavailable("hyprctl not installed".into())
        }
        _ => AgentShellError::Other(format!("hyprctl spawn: {e}").into()),
    })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(AgentShellError::Other(
            format!(
                "hyprctl {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::{Key, ModifierMask};
    use std::path::PathBuf;

    /// 片段文件路径（`$XDG_CONFIG_HOME/hypr/agent-shell.conf`）。
    fn fragment_path(config_home: &Path) -> PathBuf {
        config_home.join("hypr").join(FRAGMENT_NAME)
    }

    fn meta_shift_t() -> KeyCombo {
        KeyCombo {
            keys: vec![Key::Char('t')],
            modifiers: ModifierMask {
                meta: true,
                shift: true,
                ..ModifierMask::NONE
            },
        }
    }

    /// hyprctl 桩：记录调用参数，退出码可控。
    struct Stub {
        _dir: tempfile::TempDir,
        bin: std::path::PathBuf,
        log: std::path::PathBuf,
    }

    impl Stub {
        fn new(exit_code: i32) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let log = dir.path().join("calls.log");
            let bin = dir.path().join("hyprctl");
            let staged = dir.path().join("hyprctl.staged");
            std::fs::write(
                &staged,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\n\
                     echo \"Couldn't connect to a Hyprland instance\" >&2\n\
                     exit {exit_code}\n",
                    log = log.display()
                ),
            )
            .expect("write stub");
            let mut perms = std::fs::metadata(&staged).expect("stat").permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&staged, perms).expect("chmod");
            std::fs::rename(&staged, &bin).expect("rename stub");
            Self {
                _dir: dir,
                bin,
                log,
            }
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    #[test]
    fn upsert_replaces_same_combo_and_keeps_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fragment = fragment_path(dir.path());
        upsert_fragment(
            &fragment,
            "SUPER SHIFT, T",
            "bind = SUPER SHIFT, T, exec, first",
        )
        .expect("first write");
        upsert_fragment(&fragment, "SUPER, Q", "bind = SUPER, Q, exec, other")
            .expect("second write");
        upsert_fragment(
            &fragment,
            "SUPER SHIFT, T",
            "bind = SUPER SHIFT, T, exec, replaced",
        )
        .expect("rebind");
        let body = std::fs::read_to_string(&fragment).expect("read fragment");
        assert!(body.contains("bind = SUPER, Q, exec, other"));
        assert!(body.contains("bind = SUPER SHIFT, T, exec, replaced"));
        assert!(
            !body.contains("exec, first"),
            "old line must be replaced:\n{body}"
        );
    }

    #[test]
    fn ensure_sourced_appends_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fragment = fragment_path(dir.path());
        let main_conf = dir.path().join("hypr").join("hyprland.conf");
        ensure_sourced(&main_conf, &fragment).expect("create");
        ensure_sourced(&main_conf, &fragment).expect("idempotent");
        let body = std::fs::read_to_string(&main_conf).expect("read main conf");
        assert_eq!(body.matches(FRAGMENT_NAME).count(), 1, "body:\n{body}");
        assert!(body.starts_with(&format!("source = {}", fragment.display())));
    }

    #[test]
    fn ensure_sourced_keeps_existing_main_conf_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fragment = fragment_path(dir.path());
        let main_conf = dir.path().join("hypr").join("hyprland.conf");
        std::fs::create_dir_all(main_conf.parent().expect("parent")).expect("mkdir");
        std::fs::write(&main_conf, "monitor = , preferred, auto, 1").expect("seed");
        ensure_sourced(&main_conf, &fragment).expect("append");
        let body = std::fs::read_to_string(&main_conf).expect("read");
        assert!(body.starts_with("monitor = , preferred, auto, 1\n"));
        assert!(body.contains(FRAGMENT_NAME));
    }

    #[tokio::test]
    async fn bind_applies_keyword_and_persists_fragment() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = Stub::new(0);
        let binding = bind(&stub.bin, dir.path(), &meta_shift_t(), "notify-send hi")
            .await
            .expect("bind");
        assert_eq!(binding.backend, "hyprland");
        assert_eq!(binding.combo, "shift+meta+t");
        assert_eq!(
            stub.calls().trim(),
            "keyword bind bind = SUPER SHIFT, T, exec, notify-send hi"
        );
        let fragment = std::fs::read_to_string(fragment_path(dir.path())).expect("fragment");
        assert_eq!(fragment, "bind = SUPER SHIFT, T, exec, notify-send hi\n");
        let main_conf = std::fs::read_to_string(dir.path().join("hypr").join("hyprland.conf"))
            .expect("main conf");
        assert!(main_conf.contains(FRAGMENT_NAME));
    }

    #[test]
    fn bind_line_rejects_control_characters() {
        // 换行会把后续文本变成新的 Hyprland 指令（exec-once 等），必须拒绝。
        assert!(bind_line("SUPER, T", "touch /tmp/ok").is_ok());
        assert!(bind_line("SUPER, T", "touch a\ntouch b").is_err());
        assert!(bind_line("SUPER, T", "touch a\rmonitor=x").is_err());
        assert!(bind_line("SUPER, T", "touch a\u{7}b").is_err());
    }

    #[test]
    fn read_optional_only_treats_missing_as_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_optional(&dir.path().join("absent.conf"))
            .expect("missing is not an error")
            .is_none());
        // 路径是目录（读失败但不是 NotFound）→ 上抛，绝不当作空文件。
        let as_dir = dir.path().join("hyprland.conf");
        std::fs::create_dir(&as_dir).expect("mkdir");
        assert!(read_optional(&as_dir).is_err());
    }

    #[test]
    fn ensure_sourced_refuses_to_overwrite_unreadable_main_conf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fragment = fragment_path(dir.path());
        let main_conf = dir.path().join("hypr").join("hyprland.conf");
        // 主配置「存在但不可读」：用目录占位，读失败与权限无关、任意用户下确定性复现。
        std::fs::create_dir_all(&main_conf).expect("mkdir as conf placeholder");
        let err = ensure_sourced(&main_conf, &fragment)
            .expect_err("must not treat read failure as empty");
        assert!(
            err.to_string().contains("read"),
            "error must name the failed read: {err}"
        );
        assert!(main_conf.is_dir(), "existing path must stay untouched");
    }

    #[test]
    fn upsert_leaves_existing_fragment_intact_and_writes_atomically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fragment = fragment_path(dir.path());
        upsert_fragment(&fragment, "SUPER, A", "bind = SUPER, A, exec, one").expect("first");
        upsert_fragment(&fragment, "SUPER, B", "bind = SUPER, B, exec, two").expect("second");
        let body = std::fs::read_to_string(&fragment).expect("read");
        assert!(body.contains("exec, one") && body.contains("exec, two"));
        // 写入走临时文件 + rename：目录里不留暂存文件。
        let leftovers: Vec<String> = std::fs::read_dir(fragment.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("agent-shell.tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staged files left behind: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn bind_rejects_newline_action_before_touching_config() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = Stub::new(0);
        let err = bind(
            &stub.bin,
            dir.path(),
            &meta_shift_t(),
            "touch a\nexec-once = evil",
        )
        .await
        .expect_err("control characters must be rejected");
        assert!(
            err.to_string().contains("control characters"),
            "unexpected error: {err}"
        );
        assert!(
            !fragment_path(dir.path()).exists(),
            "fragment must not be written for a rejected action"
        );
    }

    #[tokio::test]
    async fn ready_reports_dead_instance() {
        let _guard = crate::testsupport::fork_guard().await;
        let stub = Stub::new(1);
        let reason = ready(&stub.bin).await.expect_err("hyprctl must fail");
        assert!(reason.contains("Couldn't connect"), "reason: {reason}");
    }

    #[tokio::test]
    async fn bind_surfaces_hyprctl_failure_before_persisting() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = Stub::new(1);
        let err = bind(&stub.bin, dir.path(), &meta_shift_t(), "notify-send hi")
            .await
            .expect_err("bind must fail");
        assert!(
            err.to_string().contains("Couldn't connect"),
            "unexpected error: {err}"
        );
        assert!(
            !fragment_path(dir.path()).exists(),
            "fragment must not be written when the live apply failed"
        );
    }
}
