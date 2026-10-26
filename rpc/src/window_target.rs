//! 窗口目标解析的共用语义（命令面与 MCP 工具面同源，契约 C-013）。
//!
//! 命令面把目标写成单个字符串（标题子串 / `id:` 前缀 / 裸标识），MCP 工具面用
//! 结构化入参（window_id / app_id / title / pid）。两套语法各自保留，但判定只有
//! 一份：标识等价规则、多命中必须报错、候选渲染都在本模块，避免两个入口各写一套
//! 匹配语义而漂移。

use crate::WindowEntry;

/// native_id 等价判定：精确相等，或互为 `{...}` 包裹形式。
///
/// KWin `internalId.toString()` 产出 `{uuid}`，用户可能裸传 `uuid` 或原文
/// `{uuid}`；剥离单层花括号后比较可同时覆盖两种写法。
pub fn native_id_matches(candidate: &str, native_id: &str) -> bool {
    candidate == native_id || strip_braces(candidate) == strip_braces(native_id)
}

/// 剥离单层 `{...}` 花括号；非包裹形式原样返回。
fn strip_braces(s: &str) -> &str {
    s.strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(s)
}

/// 候选渲染：`[native_id] title`，逗号分隔。
pub fn candidates<'a>(windows: impl IntoIterator<Item = &'a WindowEntry>) -> String {
    windows
        .into_iter()
        .map(|w| format!("[{}] {}", w.native_id, w.title))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 把候选折叠为唯一结果。
///
/// - 零命中：`window not found: <label>; visible titles: <全部可见窗口>`
/// - 多命中：`<n> windows match <label>, disambiguate with id:; candidates: <候选>`
///
/// `label` 由调用方给出选择条件描述（命令面传原始目标串的 `Debug` 形式，MCP 传
/// `app_id="..."` 这类入参描述），错误语义因此跨入口一致：多命中一律报歧义而不是
/// 静默取首个，零命中一律列出可见窗口。
pub fn expect_single<'a>(
    label: &str,
    matches: Vec<&'a WindowEntry>,
    visible: &[WindowEntry],
) -> Result<&'a WindowEntry, String> {
    match matches.len() {
        0 => Err(format!(
            "window not found: {label}; visible titles: {}",
            candidates(visible)
        )),
        1 => Ok(matches[0]),
        n => Err(format!(
            "{n} windows match {label}, disambiguate with id:; candidates: {}",
            candidates(matches)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(native_id: &str, title: &str, app_id: &str) -> WindowEntry {
        WindowEntry {
            native_id: native_id.into(),
            title: title.into(),
            app_id: app_id.into(),
            pid: 1000,
            x: None,
            y: None,
            width: None,
            height: None,
            workspace: None,
            stacking_order: 0,
            states: vec![],
            window_type: "Normal".into(),
            keep_above: false,
        }
    }

    #[test]
    fn braces_are_optional_on_both_sides() {
        assert!(native_id_matches("{e6f8}", "{e6f8}"));
        assert!(native_id_matches("e6f8", "{e6f8}"));
        assert!(native_id_matches("{e6f8}", "e6f8"));
        assert!(!native_id_matches("e6f8", "{e6f9}"));
        // 只剥离单层：嵌套花括号不等价。
        assert!(!native_id_matches("{{x}}", "{x}"));
    }

    #[test]
    fn single_match_is_returned() {
        let wins = vec![
            entry("w1", "Editor", "kate"),
            entry("w2", "Terminal", "konsole"),
        ];
        let picked = expect_single("window_id=\"w2\"", vec![&wins[1]], &wins).expect("unique");
        assert_eq!(picked.native_id, "w2");
    }

    #[test]
    fn multiple_matches_report_count_and_candidates() {
        let wins = vec![
            entry("w1", "Terminal", "kitty"),
            entry("w2", "Terminal", "kitty"),
            entry("w3", "Files", "nautilus"),
        ];
        let matches = vec![&wins[0], &wins[1]];
        let err = expect_single("app_id=\"kitty\"", matches, &wins).unwrap_err();
        assert!(err.contains("2 windows match app_id=\"kitty\""), "{err}");
        assert!(err.contains("disambiguate with id:"), "{err}");
        assert!(
            err.contains("[w1] Terminal") && err.contains("[w2] Terminal"),
            "{err}"
        );
        assert!(
            !err.contains("[w3]"),
            "candidates must be the matches only: {err}"
        );
    }

    #[test]
    fn zero_matches_list_every_visible_title() {
        let wins = vec![
            entry("w1", "Editor", "kate"),
            entry("w2", "Terminal", "kitty"),
        ];
        let err = expect_single("app_id=\"nope\"", Vec::new(), &wins).unwrap_err();
        assert!(err.contains("window not found: app_id=\"nope\""), "{err}");
        assert!(
            err.contains("[w1] Editor") && err.contains("[w2] Terminal"),
            "{err}"
        );
    }
}
