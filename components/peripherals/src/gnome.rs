//! GNOME 后端：gsettings（触控板全局设置 + input-sources 布局）。
//!
//! GNOME 的触控板设置是**全局**的（无逐设备维度），故 `devices` 恒为空；
//! 布局来源与最近使用序（`sources` / `mru-sources`）都是 GVariant 文本，
//! 用轻量扫描解析，避免为固定形状引入 GVariant 解析依赖。

use std::path::Path;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{KeyboardLayout, KeyboardLayouts, TouchpadStatus};

use crate::{run_command, SOURCE_GNOME_GSETTINGS};

/// 触控板设置 schema。
const TOUCHPAD_SCHEMA: &str = "org.gnome.desktop.peripherals.touchpad";
/// 键盘布局 schema。
const INPUT_SOURCES_SCHEMA: &str = "org.gnome.desktop.input-sources";

/// 读 gsettings 触控板设置（全局口径）。
pub(crate) async fn touchpad(gsettings: &Path) -> Result<TouchpadStatus> {
    let enabled = parse_send_events(&get(gsettings, TOUCHPAD_SCHEMA, "send-events").await?)
        .ok_or_else(|| unexpected("send-events"))?;
    let natural_scroll = parse_bool(&get(gsettings, TOUCHPAD_SCHEMA, "natural-scroll").await?)
        .ok_or_else(|| unexpected("natural-scroll"))?;
    let tap_to_click = parse_bool(&get(gsettings, TOUCHPAD_SCHEMA, "tap-to-click").await?)
        .ok_or_else(|| unexpected("tap-to-click"))?;
    Ok(TouchpadStatus {
        source: SOURCE_GNOME_GSETTINGS.into(),
        enabled: Some(enabled),
        natural_scroll: Some(natural_scroll),
        tap_to_click: Some(tap_to_click),
        // GNOME 的触控板设置作用域是全局，没有逐设备明细可填。
        devices: Vec::new(),
    })
}

/// 读 gsettings 键盘布局列表。
pub(crate) async fn layouts(gsettings: &Path) -> Result<KeyboardLayouts> {
    let sources = parse_gvariant_sources(&get(gsettings, INPUT_SOURCES_SCHEMA, "sources").await?);
    if sources.is_empty() {
        return Err(AgentShellError::BackendUnavailable(
            "gsettings sources is empty".into(),
        ));
    }
    // MRU 只影响生效序号，读不到就退化为 None，不该因此丢掉整份布局列表。
    let mru_raw = get(gsettings, INPUT_SOURCES_SCHEMA, "mru-sources")
        .await
        .unwrap_or_default();
    let layouts = sources
        .iter()
        .enumerate()
        .map(|(i, (_, id))| KeyboardLayout {
            index: i as u32,
            layout: id.clone(),
            variant: None,
            display_name: None,
        })
        .collect();
    Ok(KeyboardLayouts {
        source: SOURCE_GNOME_GSETTINGS.into(),
        active_index: resolve_active(&sources, &parse_gvariant_sources(&mru_raw)),
        layouts,
    })
}

/// `gsettings get <schema> <key>` 的 stdout。
async fn get(gsettings: &Path, schema: &str, key: &str) -> Result<String> {
    run_command(gsettings, &["get", schema, key]).await
}

/// 取值非预期时的段内失败（schema 与预期不符 → 交回链路换后端）。
fn unexpected(key: &str) -> AgentShellError {
    AgentShellError::BackendUnavailable(format!("unexpected {key} value"))
}

/// gsettings `send-events` 取值 → 开关；未知取值 → None。
fn parse_send_events(raw: &str) -> Option<bool> {
    match unquote(raw).as_str() {
        "enabled" => Some(true),
        "disabled" | "disabled-on-external-mouse" => Some(false),
        _ => None,
    }
}

/// gsettings 布尔字面量（`true` / `false`）。
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// 去掉 GVariant 字符串字面量的包裹单引号。
fn unquote(raw: &str) -> String {
    raw.trim().trim_matches('\'').to_string()
}

/// 解析 GVariant `[('xkb', 'us'), ('xkb', 'ru')]` 为 `(类型, id)` 序列。
///
/// 只按括号分组、按单引号取串，不识别的残缺分组直接跳过——gsettings 的
/// 输出形状由 schema 固定，无需完整 GVariant 解析器。
fn parse_gvariant_sources(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(open) = rest.find('(') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find(')') else {
            break;
        };
        let items = quoted_strings(&rest[..close]);
        rest = &rest[close + 1..];
        if let [kind, id, ..] = items.as_slice() {
            out.push((kind.clone(), id.clone()));
        }
    }
    out
}

/// 提取一段文本内的单引号字符串（含反斜杠转义）。
fn quoted_strings(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut buf = String::new();
        loop {
            match chars.next() {
                None => break,
                Some('\\') => {
                    if let Some(escaped) = chars.next() {
                        buf.push(escaped);
                    }
                }
                Some('\'') => break,
                Some(other) => buf.push(other),
            }
        }
        out.push(buf);
    }
    out
}

/// 生效序号：`mru-sources[0]` 在 `sources` 中的下标；解析不出 → None。
fn resolve_active(sources: &[(String, String)], mru: &[(String, String)]) -> Option<u32> {
    let first = mru.first()?;
    sources.iter().position(|s| s == first).map(|i| i as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::stub_script;

    /// 写一个按 key 分支回显的 gsettings stub。
    fn stub_gsettings(dir: &Path, body: &str) -> std::path::PathBuf {
        stub_script(dir, "gsettings", body)
    }

    #[test]
    fn gvariant_sources_parses_single_multi_and_empty() {
        assert_eq!(
            parse_gvariant_sources("[('xkb', 'us')]"),
            vec![("xkb".to_string(), "us".to_string())]
        );
        assert_eq!(
            parse_gvariant_sources("[('xkb', 'us'), ('xkb', 'ru')]"),
            vec![
                ("xkb".to_string(), "us".to_string()),
                ("xkb".to_string(), "ru".to_string()),
            ]
        );
        assert!(parse_gvariant_sources("[]").is_empty());
        assert!(parse_gvariant_sources("@a(ss) []").is_empty());
    }

    #[test]
    fn gvariant_sources_tolerates_whitespace_and_other_types() {
        let raw = " [ ('xkb',  'us') , ('ibus', 'libpinyin') ] ";
        assert_eq!(
            parse_gvariant_sources(raw),
            vec![
                ("xkb".to_string(), "us".to_string()),
                ("ibus".to_string(), "libpinyin".to_string()),
            ]
        );
    }

    #[test]
    fn send_events_maps_all_three_values() {
        assert_eq!(parse_send_events("'enabled'"), Some(true));
        assert_eq!(parse_send_events("'disabled'"), Some(false));
        assert_eq!(
            parse_send_events("'disabled-on-external-mouse'"),
            Some(false)
        );
        assert_eq!(parse_send_events("'bogus'"), None);
        assert_eq!(parse_send_events(""), None);
    }

    #[test]
    fn active_index_follows_mru_head() {
        let sources = vec![
            ("xkb".to_string(), "us".to_string()),
            ("xkb".to_string(), "ru".to_string()),
        ];
        // MRU 头部 ru 在 sources 的下标 1。
        let mru = vec![
            ("xkb".to_string(), "ru".to_string()),
            ("xkb".to_string(), "us".to_string()),
        ];
        assert_eq!(resolve_active(&sources, &mru), Some(1));
        // MRU 头部就是首个来源。
        assert_eq!(resolve_active(&sources, &sources), Some(0));
        // MRU 头部不在 sources 中、或 MRU 为空 → None。
        let stray = vec![("xkb".to_string(), "de".to_string())];
        assert_eq!(resolve_active(&sources, &stray), None);
        assert_eq!(resolve_active(&sources, &[]), None);
    }

    #[tokio::test]
    async fn gsettings_stub_drives_gnome_touchpad() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let bin = stub_gsettings(
            dir.path(),
            "case \"$3\" in\n\
             send-events) echo \"'disabled-on-external-mouse'\" ;;\n\
             natural-scroll) echo true ;;\n\
             tap-to-click) echo false ;;\n\
             *) exit 1 ;;\nesac",
        );
        let status = touchpad(&bin).await.unwrap();
        assert_eq!(status.source, SOURCE_GNOME_GSETTINGS);
        assert_eq!(status.enabled, Some(false));
        assert_eq!(status.natural_scroll, Some(true));
        assert_eq!(status.tap_to_click, Some(false));
        assert!(status.devices.is_empty());
    }

    #[tokio::test]
    async fn gsettings_stub_drives_gnome_layouts_with_mru() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let bin = stub_gsettings(
            dir.path(),
            "case \"$3\" in\n\
             sources) echo \"[('xkb', 'us'), ('xkb', 'ru')]\" ;;\n\
             mru-sources) echo \"[('xkb', 'ru'), ('xkb', 'us')]\" ;;\n\
             *) exit 1 ;;\nesac",
        );
        let result = layouts(&bin).await.unwrap();
        assert_eq!(result.source, SOURCE_GNOME_GSETTINGS);
        assert_eq!(result.active_index, Some(1));
        assert_eq!(
            result
                .layouts
                .iter()
                .map(|l| l.layout.as_str())
                .collect::<Vec<_>>(),
            vec!["us", "ru"]
        );
        assert_eq!(result.layouts[0].variant, None);
        assert_eq!(result.layouts[0].display_name, None);
    }

    #[tokio::test]
    async fn gsettings_unexpected_value_fails_the_segment() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let bin = stub_gsettings(dir.path(), "echo \"'bogus'\"");
        let err = touchpad(&bin).await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn gsettings_missing_binary_is_unavailable() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let err = layouts(&dir.path().join("absent")).await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got {err:?}"
        );
    }
}
