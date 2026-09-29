//! GNOME 全局快捷键后端（gsettings 自定义快捷键，§21.33）。
//!
//! GNOME 无自定义「命令快捷键」D-Bus 接口；官方路径是 gsettings：
//! `org.gnome.settings-daemon.plugins.media-keys` 的 `custom-keybindings`
//! 列出启用的子 schema 路径，每条路径对应一个
//! `…media-keys.custom-keybinding:<path>` 实例（`name` / `command` / `binding`）。
//! gsd-media-keys 监听这些键，改动即时生效、跨会话持久。
//!
//! 绑定顺序：先写子 schema 三键，再把路径并入 `custom-keybindings`——反过来
//! 则在子键写入失败时留下指向空条目的悬空路径。列表重建保留全部既有路径
//! （含非 `customN` 形态的条目），只做「追加或复用」。

use crate::combo;
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::ShortcutBinding;
use agent_shell_core::types::KeyCombo;
use std::path::Path;
use std::time::Duration;

/// 自定义快捷键的宿主 schema。
const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
/// 单条自定义快捷键的子 schema 名（`set`/`get` 时需拼 `:<path>`）。
const CUSTOM_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
/// 子 schema 路径前缀。
const CUSTOM_PATH_PREFIX: &str =
    "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom";
/// gsettings 调用超时（与 §19 通用 D-Bus 5s 同量级）。
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// gsettings 是否可用（二进制存在 + media-keys schema 已安装）。
pub(crate) async fn ready(gsettings: &Path) -> std::result::Result<(), String> {
    run_gsettings(gsettings, &["list-keys", MEDIA_KEYS])
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 绑定：复用同命令的既有条目，否则占用最小空闲槽位。
pub(crate) async fn bind(
    gsettings: &Path,
    combo: &KeyCombo,
    action: &str,
) -> Result<ShortcutBinding> {
    let accelerator =
        combo::gtk_accelerator(combo).map_err(|e| AgentShellError::Other(e.into()))?;
    let list = run_gsettings(gsettings, &["get", MEDIA_KEYS, "custom-keybindings"]).await?;
    let paths = parse_path_list(&list);
    let slots = read_slots(gsettings, &paths).await;

    let slot = reuse_slot(&slots, action).unwrap_or_else(|| free_slot(&slots));
    let path = format!("{CUSTOM_PATH_PREFIX}{slot}/");
    let schema = format!("{CUSTOM_SCHEMA}:{path}");
    let label = label_for(action);

    for (key, value) in [
        ("name", gvariant_string(&label)),
        ("command", gvariant_string(action)),
        ("binding", gvariant_string(&accelerator)),
    ] {
        run_gsettings(gsettings, &["set", &schema, key, &value]).await?;
    }

    if !paths.iter().any(|p| p == &path) {
        let mut updated = paths.clone();
        updated.push(path.clone());
        let value = gvariant_string_list(&updated);
        run_gsettings(
            gsettings,
            &["set", MEDIA_KEYS, "custom-keybindings", &value],
        )
        .await?;
    }

    Ok(ShortcutBinding {
        backend: "gnome-gsettings".into(),
        component: Some(path),
        combo: combo::canonical(combo),
        action: action.to_string(),
    })
}

/// 既有自定义快捷键槽位（序号 + 命令，用于复用判断）。
struct ExistingSlot {
    index: u32,
    command: String,
}

/// `custom-keybindings` 值 → 路径列表（保持原顺序，非 `customN` 条目一并保留）。
fn parse_path_list(list_value: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = list_value;
    while let Some(open) = rest.find('\'') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('\'') else { break };
        paths.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    paths
}

/// 槽位路径 → 序号（非 `customN/` 形态返回 None）。
fn custom_index(path: &str) -> Option<u32> {
    let tail = path.strip_prefix(CUSTOM_PATH_PREFIX)?;
    let digits = tail.strip_suffix('/')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// 回读各 `customN` 槽位已绑定的命令；单个读取失败按未设置处理（不阻断绑定）。
async fn read_slots(gsettings: &Path, paths: &[String]) -> Vec<ExistingSlot> {
    let mut slots = Vec::new();
    for path in paths {
        let Some(index) = custom_index(path) else {
            continue;
        };
        let schema = format!("{CUSTOM_SCHEMA}:{path}");
        let command = run_gsettings(gsettings, &["get", &schema, "command"])
            .await
            .map(|out| gvariant_unquote(out.trim()))
            .unwrap_or_default();
        slots.push(ExistingSlot { index, command });
    }
    slots.sort_by_key(|s| s.index);
    slots
}

/// 复用已绑定同一命令的槽位（重复 `shortcut bind` 同一动作不堆叠条目）。
fn reuse_slot(slots: &[ExistingSlot], action: &str) -> Option<u32> {
    slots.iter().find(|s| s.command == action).map(|s| s.index)
}

/// 未被占用的最小槽位号。
fn free_slot(slots: &[ExistingSlot]) -> u32 {
    (0..)
        .find(|n| !slots.iter().any(|s| s.index == *n))
        .unwrap_or(0)
}

/// GNOME 快捷键 KCM 里的条目名。
fn label_for(action: &str) -> String {
    let single_line: String = action
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = single_line.trim();
    let mut label: String = trimmed.chars().take(64).collect();
    if trimmed.chars().count() > 64 {
        label.push('…');
    }
    format!("agent-shell: {label}")
}

/// GVariant 字符串字面量（gsettings 取值必须是序列化 GVariant，不能传裸文本）。
fn gvariant_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// GVariant 字符串数组字面量（`as`）。
fn gvariant_string_list(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| gvariant_string(v)).collect();
    format!("[{}]", items.join(", "))
}

/// 去掉 gsettings 回读的字符串字面量引号。
fn gvariant_unquote(value: &str) -> String {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .unwrap_or(trimmed);
    inner.replace("\\'", "'").replace("\\\\", "\\")
}

/// 执行 gsettings 并返回 stdout。
///
/// 错误分类：二进制缺失 → BackendUnavailable；schema 缺失/写入被拒（非零退出）
/// → Other 并带 stderr（`ready` 把它当探测原因，链上继续尝试下一个后端）。
async fn run_gsettings(bin: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        CALL_TIMEOUT,
        tokio::process::Command::new(bin)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| AgentShellError::Timeout(format!("gsettings {} timed out", args.join(" "))))?
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            AgentShellError::BackendUnavailable("gsettings not installed".into())
        }
        _ => AgentShellError::Other(format!("gsettings spawn: {e}").into()),
    })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(AgentShellError::Other(
            format!(
                "gsettings {}: {}",
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

    fn ctrl_alt_t() -> KeyCombo {
        KeyCombo {
            keys: vec![Key::Char('t')],
            modifiers: ModifierMask {
                ctrl: true,
                alt: true,
                ..ModifierMask::NONE
            },
        }
    }

    fn custom_path(index: u32) -> String {
        format!("{CUSTOM_PATH_PREFIX}{index}/")
    }

    #[test]
    fn parses_path_list_keeping_foreign_entries_and_order() {
        let value = format!(
            "['/other/path/', '{0}', '{1}']",
            custom_path(2),
            custom_path(0)
        );
        let paths = parse_path_list(&value);
        assert_eq!(
            paths,
            vec!["/other/path/".to_string(), custom_path(2), custom_path(0)]
        );
        // 空数组与 @as 形态没有引号 → 空列表。
        assert!(parse_path_list("@as []").is_empty());
        assert!(parse_path_list("[]").is_empty());
    }

    #[test]
    fn custom_index_only_accepts_canonical_slots() {
        assert_eq!(custom_index(&custom_path(7)), Some(7));
        assert_eq!(custom_index("/other/path/"), None);
        assert_eq!(custom_index(&format!("{CUSTOM_PATH_PREFIX}x/")), None);
        assert_eq!(custom_index(CUSTOM_PATH_PREFIX), None);
    }

    #[test]
    fn picks_free_slot_and_reuses_matching_command() {
        let slots = vec![
            ExistingSlot {
                index: 0,
                command: "notify-send bye".into(),
            },
            ExistingSlot {
                index: 2,
                command: "notify-send hi".into(),
            },
        ];
        assert_eq!(free_slot(&slots), 1);
        assert_eq!(reuse_slot(&slots, "notify-send hi"), Some(2));
        assert_eq!(reuse_slot(&slots, "notify-send new"), None);
    }

    #[test]
    fn gvariant_literals_escape_quotes_and_backslashes() {
        assert_eq!(gvariant_string("notify-send hi"), "'notify-send hi'");
        assert_eq!(gvariant_string("sh -c 'echo hi'"), r"'sh -c \'echo hi\''");
        assert_eq!(gvariant_string(r"C:\path"), r"'C:\\path'");
        assert_eq!(gvariant_unquote(r"'C:\\path'"), r"C:\path");
        assert_eq!(
            gvariant_string_list(&["a".into(), "b".into()]),
            "['a', 'b']"
        );
    }

    #[test]
    fn label_is_single_line_and_bounded() {
        assert_eq!(label_for("notify-send hi"), "agent-shell: notify-send hi");
        assert_eq!(label_for("echo a\nb"), "agent-shell: echo a b");
        assert!(label_for(&"x".repeat(100)).ends_with('…'));
    }

    /// 测试用 gsettings 桩：记录调用参数、按参数回放固定输出。
    struct Stub {
        _dir: tempfile::TempDir,
        bin: std::path::PathBuf,
        log: std::path::PathBuf,
    }

    impl Stub {
        fn new(build: impl Fn(&Path) -> String) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let log = dir.path().join("calls.log");
            let script_body = build(&log);
            let bin = dir.path().join("gsettings");
            // 先写暂存文件再 rename：避免 fork 期间路径上存在写打开 fd（ETXTBSY）。
            let staged = dir.path().join("gsettings.staged");
            std::fs::write(&staged, &script_body).expect("write stub");
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

    /// 生成桩脚本：`list` 是 `get custom-keybindings` 的回放值，`commands` 按
    /// 路径片段给出各子 schema `get … command` 的回放值（未命中的槽位回放空串）。
    ///
    /// 模式里的空格必须转义（`get\ foo`）——`case` 模式一旦整体加引号，通配符
    /// `*` 就退化为字面量，桩会静默走到 `*)` 分支。
    fn stub_body(log: &Path, list: &str, commands: &[(&str, &str)]) -> String {
        let mut cases = String::new();
        for (fragment, command) in commands {
            cases.push_str(&format!(
                "get\\ {CUSTOM_SCHEMA}:*{fragment}*\\ command) printf '%s\\n' \"{command}\" ;;\n"
            ));
        }
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\ncase \"$*\" in\n\
             get\\ {MEDIA_KEYS}\\ custom-keybindings) printf '%s\\n' \"{list}\" ;;\n\
             {cases}\
             get\\ {CUSTOM_SCHEMA}:*\\ command) printf '%s\\n' '' ;;\n\
             list-keys\\ {MEDIA_KEYS}) printf 'custom-keybindings\\n' ;;\n\
             *) : ;;\nesac\n",
            log = log.display()
        )
    }

    #[tokio::test]
    async fn bind_appends_new_slot_and_writes_child_keys_first() {
        let _guard = crate::testsupport::fork_guard().await;
        let stub = Stub::new(|log| stub_body(log, "@as []", &[]));
        let binding = bind(&stub.bin, &ctrl_alt_t(), "notify-send hi")
            .await
            .expect("bind");
        assert_eq!(binding.backend, "gnome-gsettings");
        assert_eq!(binding.combo, "ctrl+alt+t");
        let path = binding.component.expect("path");
        assert_eq!(path, custom_path(0));

        let calls = stub.calls();
        let child_name = calls
            .find(&format!(
                "set {CUSTOM_SCHEMA}:{path} name 'agent-shell: notify-send hi'"
            ))
            .unwrap_or_else(|| panic!("child name write missing:\n{calls}"));
        let child_binding = calls
            .find(&format!(
                "set {CUSTOM_SCHEMA}:{path} binding '<Control><Alt>t'"
            ))
            .unwrap_or_else(|| panic!("child binding write missing:\n{calls}"));
        let list_write = calls
            .find(&format!("set {MEDIA_KEYS} custom-keybindings ['{path}']"))
            .unwrap_or_else(|| panic!("list append missing:\n{calls}"));
        // 子键必须先于列表写入，否则失败会留下悬空条目。
        assert!(child_name < list_write && child_binding < list_write);
        assert!(calls.contains(&format!(
            "set {CUSTOM_SCHEMA}:{path} command 'notify-send hi'"
        )));
    }

    #[tokio::test]
    async fn bind_reuses_slot_of_same_command_and_preserves_foreign_paths() {
        let _guard = crate::testsupport::fork_guard().await;
        let list = format!(
            "['/other/path/', '{0}', '{1}']",
            custom_path(0),
            custom_path(2)
        );
        let stub = Stub::new(|log| stub_body(log, &list, &[("custom2/", "notify-send hi")]));
        let binding = bind(&stub.bin, &ctrl_alt_t(), "notify-send hi")
            .await
            .expect("bind");
        assert_eq!(binding.component.as_deref(), Some(&*custom_path(2)));
        let calls = stub.calls();
        assert!(calls.contains(&format!("set {CUSTOM_SCHEMA}:{0} name", custom_path(2))));
        // 复用既有槽位 → 不重写列表（避免触碰用户既有条目）。
        assert!(
            !calls.contains(&format!("set {MEDIA_KEYS} custom-keybindings")),
            "list must stay untouched on reuse:\n{calls}"
        );
    }

    #[tokio::test]
    async fn bind_appends_new_slot_keeping_foreign_paths() {
        let _guard = crate::testsupport::fork_guard().await;
        let list = format!("['/other/path/', '{0}']", custom_path(2));
        let stub = Stub::new(|log| stub_body(log, &list, &[("custom2/", "other command")]));
        let binding = bind(&stub.bin, &ctrl_alt_t(), "notify-send hi")
            .await
            .expect("bind");
        assert_eq!(binding.component.as_deref(), Some(&*custom_path(0)));
        let calls = stub.calls();
        assert!(
            calls.contains(&format!(
                "set {MEDIA_KEYS} custom-keybindings ['/other/path/', '{0}', '{1}']",
                custom_path(2),
                custom_path(0)
            )),
            "foreign paths must be preserved:\n{calls}"
        );
    }

    #[tokio::test]
    async fn ready_reports_missing_schema() {
        let _guard = crate::testsupport::fork_guard().await;
        let stub = Stub::new(|_| {
            "#!/bin/sh\nprintf '%s\\n' \"$*\"\necho 'No such schema' >&2\nexit 1\n".to_string()
        });
        let reason = ready(&stub.bin).await.expect_err("schema missing");
        assert!(reason.contains("No such schema"), "reason: {reason}");
    }

    #[tokio::test]
    async fn bind_surfaces_write_failure_with_stderr() {
        let _guard = crate::testsupport::fork_guard().await;
        let stub = Stub::new(|log| {
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\ncase \"$*\" in\n\
                 get\\ {MEDIA_KEYS}\\ custom-keybindings) printf '%s\\n' '@as []' ;;\n\
                 set\\ {CUSTOM_SCHEMA}:*/\\ binding*) echo 'Cannot parse value' >&2; exit 1 ;;\n\
                 *) : ;;\nesac\n",
                log = log.display()
            )
        });
        let err = bind(&stub.bin, &ctrl_alt_t(), "notify-send hi")
            .await
            .expect_err("binding write must fail");
        assert!(
            err.to_string().contains("Cannot parse value"),
            "unexpected error: {err}"
        );
    }
}
