//! 输出格式化：table / json 两种渲染（设计文档 §17.2 CLI）。

use agent_shell_rpc::WindowEntry;
use serde_json::json;

/// 窗口列表 table 行（daemon 返回条目）。
pub fn windows_entries_table(wins: &[WindowEntry]) -> String {
    let mut out =
        String::from("ID                    TITLE                 APP                PID\n");
    for w in wins {
        out.push_str(&format!(
            "{:<20}  {:<20}  {:<17}  {}\n",
            truncate(&w.native_id, 20),
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
                "from_cache": w.from_cache,
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
            from_cache: false,
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
