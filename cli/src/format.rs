//! 输出格式化：table / json 两种渲染（设计文档 §17.2 CLI）。

use agent_shell_rpc::WindowEntry;
use serde_json::json;

/// `native_id` 列宽。KWin `{uuid}` 为 38 字符，此前 20 字符列宽会把 id 截断成
/// `{593a298c-…`，从表格复制的截断串无法命中 `windows focus`/`move`
/// （`window not found`）。放宽到 40 并停止截断 id：id 是复制粘贴的定位键，
/// 必须原样完整输出；title/app_id 仍截断（仅作对齐显示，不参与定位）。
const ID_COL_WIDTH: usize = 40;

/// 窗口列表 table 行（daemon 返回条目）。
pub fn windows_entries_table(wins: &[WindowEntry]) -> String {
    let mut out = format!(
        "{:<ID_COL_WIDTH$}  {:<20}  {:<17}  {}\n",
        "ID", "TITLE", "APP", "PID"
    );
    for w in wins {
        out.push_str(&format!(
            "{:<ID_COL_WIDTH$}  {:<20}  {:<17}  {}\n",
            w.native_id,
            truncate(&w.title, 20),
            truncate(&w.app_id, 17),
            w.pid,
        ));
    }
    out
}

/// 窗口列表 JSON。
pub fn windows_entries_json(wins: &[WindowEntry]) -> String {
    let items: Vec<_> = wins
        .iter()
        .map(|w| {
            json!({
                "id": w.native_id,
                "title": w.title,
                "app_id": w.app_id,
                "pid": w.pid,
                "x": w.x,
                "y": w.y,
                "width": w.width,
                "height": w.height,
                "workspace": w.workspace,
            })
        })
        .collect();
    serde_json::to_string_pretty(&items).expect("WindowEntry is JSON-serializable")
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(native: &str, title: &str) -> WindowEntry {
        WindowEntry {
            native_id: native.into(),
            title: title.into(),
            app_id: "org.kde.kate".into(),
            pid: 4242,
            x: None,
            y: None,
            width: None,
            height: None,
            workspace: None,
        }
    }

    #[test]
    fn table_has_header_and_row() {
        let t = windows_entries_table(&[sample("abc-123", "Editor")]);
        assert!(t.starts_with("ID"));
        assert!(t.contains("abc-123"));
        assert!(t.contains("org.kde.kate"));
    }

    #[test]
    fn table_preserves_full_kwin_uuid_id() {
        // KWin `{uuid}` 为 38 字符：表格必须原样输出完整 id，否则复制粘贴无法命中。
        let uuid = "{593a298c-79e4-408d-9b5e-7a3c1d2e3f4a}";
        let t = windows_entries_table(&[sample(uuid, "Editor")]);
        assert!(
            t.contains(uuid),
            "table must emit full native_id, got:\n{t}"
        );
        assert!(
            !t.contains('…'),
            "native_id must not be ellipsized, got:\n{t}"
        );
    }

    #[test]
    fn json_roundtrip_carries_id_and_title() {
        let j = windows_entries_json(&[sample("abc-123", "Editor — main.rs")]);
        let v: serde_json::Value = serde_json::from_str(&j).expect("valid json");
        assert_eq!(v[0]["id"], "abc-123");
        assert_eq!(v[0]["title"], "Editor — main.rs");
    }

    #[test]
    fn truncation_marks_overflow() {
        assert_eq!(truncate("short", 10), "short");
        let long = truncate("a-very-long-window-title", 10);
        assert!(long.ends_with('…'));
        assert!(long.chars().count() <= 10);
    }

    #[test]
    fn empty_list_renders_header_only() {
        assert_eq!(windows_entries_table(&[]).lines().count(), 1);
        assert_eq!(windows_entries_json(&[]), "[]");
    }
}
