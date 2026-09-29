//! X11 后端：`setxkbmap -query`（跨 DE 保底，XWayland 会话亦可读到当前布局）。

use std::path::Path;

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{KeyboardLayout, KeyboardLayouts};

use crate::{run_command, SOURCE_X11_SETXKB};

/// 读 `setxkbmap -query` 的布局列表。
pub(crate) async fn layouts(setxkbmap: &Path) -> Result<KeyboardLayouts> {
    let out = run_command(setxkbmap, &["-query"]).await?;
    let Some(layouts) = parse_setxkbmap_query(&out) else {
        return Err(AgentShellError::BackendUnavailable(
            "setxkbmap -query reported no layout".into(),
        ));
    };
    // `-query` 不报当前组号：仅有一个布局时它必然生效，多布局则无从判断。
    let active_index = (layouts.len() == 1).then_some(0);
    Ok(KeyboardLayouts {
        source: SOURCE_X11_SETXKB.into(),
        active_index,
        layouts,
    })
}

/// 解析 `setxkbmap -query` 的 `layout` / `variant` 两行。
///
/// `variant` 行可整体缺失（无任何变体）；存在时与 `layout` 按下标对齐，
/// 空项表示该布局无变体。未出现 `layout` 行 → None。
fn parse_setxkbmap_query(out: &str) -> Option<Vec<KeyboardLayout>> {
    let mut layout_line = None;
    let mut variant_line = None;
    for line in out.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "layout" => layout_line = Some(value.trim()),
            "variant" => variant_line = Some(value.trim()),
            _ => {}
        }
    }
    let codes = split_csv(layout_line?);
    let variants = variant_line.map(split_csv).unwrap_or_default();
    let layouts: Vec<KeyboardLayout> = codes
        .iter()
        .enumerate()
        .filter(|(_, code)| !code.is_empty())
        .map(|(i, code)| KeyboardLayout {
            index: i as u32,
            layout: code.clone(),
            variant: variants.get(i).filter(|v| !v.is_empty()).cloned(),
            display_name: None,
        })
        .collect();
    (!layouts.is_empty()).then_some(layouts)
}

/// 逗号拆分并 trim（保留空项以维持下标对齐）。
fn split_csv(value: &str) -> Vec<String> {
    value.split(',').map(|s| s.trim().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::stub_script;

    /// `setxkbmap -query` 的真实输出形状。
    const QUERY_WITH_VARIANT: &str = "rules:      evdev\n\
                                      model:      pc105\n\
                                      layout:     us,ru\n\
                                      variant:    ,phonetic\n";

    #[test]
    fn query_parses_layouts_and_aligns_variant() {
        let layouts = parse_setxkbmap_query(QUERY_WITH_VARIANT).unwrap();
        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0].index, 0);
        assert_eq!(layouts[0].layout, "us");
        assert_eq!(layouts[0].variant, None);
        assert_eq!(layouts[1].index, 1);
        assert_eq!(layouts[1].layout, "ru");
        assert_eq!(layouts[1].variant.as_deref(), Some("phonetic"));
    }

    #[test]
    fn query_without_variant_line_yields_all_none() {
        let out = "rules:      evdev\nmodel:      pc105\nlayout:     us\n";
        let layouts = parse_setxkbmap_query(out).unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].variant, None);
    }

    #[test]
    fn query_short_variant_line_does_not_shift_indices() {
        let out = "layout:  us,ru,de\nvariant: phonetic\n";
        let layouts = parse_setxkbmap_query(out).unwrap();
        assert_eq!(layouts[0].variant.as_deref(), Some("phonetic"));
        assert_eq!(layouts[1].variant, None);
        assert_eq!(layouts[2].variant, None);
    }

    #[test]
    fn query_without_layout_line_is_none() {
        assert!(parse_setxkbmap_query("rules: evdev\nmodel: pc105\n").is_none());
        assert!(parse_setxkbmap_query("").is_none());
    }

    #[tokio::test]
    async fn multiple_layouts_have_no_active_index() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let bin = stub_setxkbmap(dir.path(), QUERY_WITH_VARIANT);
        let result = layouts(&bin).await.unwrap();
        assert_eq!(result.source, SOURCE_X11_SETXKB);
        // 两个布局时 `-query` 无法确定生效序号。
        assert_eq!(result.active_index, None);
    }

    #[tokio::test]
    async fn single_layout_reports_index_zero() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let bin = stub_setxkbmap(dir.path(), "layout:  us\n");
        let result = layouts(&bin).await.unwrap();
        assert_eq!(result.active_index, Some(0));
        assert_eq!(result.layouts.len(), 1);
        assert_eq!(result.layouts[0].layout, "us");
    }

    #[tokio::test]
    async fn missing_binary_is_unavailable() {
        let _guard = crate::testsupport::fork_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let err = layouts(&dir.path().join("absent")).await.unwrap_err();
        assert!(
            matches!(err, AgentShellError::BackendUnavailable(_)),
            "got {err:?}"
        );
    }

    /// 写一个把固定查询结果打印到 stdout 的 `setxkbmap` stub。
    fn stub_setxkbmap(dir: &Path, out: &str) -> std::path::PathBuf {
        stub_script(dir, "setxkbmap", &format!("printf '%s' '{out}'"))
    }
}
