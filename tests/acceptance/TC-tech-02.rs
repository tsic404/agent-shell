//! TC-tech-02（AC-111，P1）：测试源不含跳过标记与整文件条件编译豁免。
//!
//! - 场景：测试源不含跳过标记与整文件条件编译豁免
//! - 判据：测试源无忽略标记、无整文件条件编译豁免；出现任一即判失败；同文件内
//!   的普通条件断言不在此列
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-02`
//!
//! 取证对象由包清单的 `[[test]]` 声明与目录遍历**双向核对**得到：清单声明的
//! 每个目标路径都必须是目录内的源文件，目录内每个 `*.rs`（排除 `src/`）也都
//! 必须在清单中声明——新增用例目标漏声明或漏扫描都判失败。逐文件按属性头
//! 匹配跳过/豁免标记：
//! - 忽略标记：`#[ignore]`、`#[ignore = "…"]`（属性头 `#[ignore` 后紧跟 `]` /
//!   `=`）与 `#[cfg_attr(…, ignore)]`（属性头 `#[cfg_attr` 且参数含 `ignore`
//!   词元）；
//! - 整文件条件编译豁免：`#![cfg(…)]` / `#![cfg_attr(…)]` 内层属性（作用于整个
//!   crate，会按条件裁掉整个测试目标）。
//!
//! 普通条件断言在函数体内、不以属性形态出现，天然不在扫描范围。
//!
//! 夹具的 `headless_session` 是进程级环境修改，本目标不装配 daemon，无副作用。

use acceptance::workspace_root;
use std::path::{Path, PathBuf};

/// 包清单里 `[[test]]` 声明的目标源文件（相对包根，保持声明顺序）。
fn manifest_test_paths(manifest: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut in_test = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_test = trimmed == "[[test]]";
            continue;
        }
        if !in_test {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("path") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        if !value.is_empty() {
            paths.push(value.to_string());
        }
    }
    paths
}

/// 目录内直接子级的 `*.rs` 文件（`src/` 是子目录，不在其列），排序后返回。
fn directory_sources(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    names.sort();
    names
}

/// `#[<name>` 属性头且后继 `]` / `=`（可夹空白）。
///
/// 用于把 `#[ignore]` / `#[ignore = "…"]` 与 `#[ignore_something]` 区分开；
/// `name = "!cfg("` 时命中整文件豁免 `#![cfg(…)]` 的头。
fn attribute_head(line: &str, name: &str) -> bool {
    let needle = format!("#[{name}");
    line.find(&needle).is_some_and(|pos| {
        let after = line[pos + needle.len()..].trim_start();
        after.starts_with(']') || after.starts_with('=')
    })
}

/// 行内是否含独立 `ignore` 词元（`#[cfg_attr(…, ignore)]` 的参数判定）。
fn contains_ignore_token(line: &str) -> bool {
    line.match_indices("ignore").any(|(idx, _)| {
        let before_ok = line[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let after_ok = line[idx + "ignore".len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        before_ok && after_ok
    })
}

/// 一行是否构成跳过/豁免标记（判据逐字核对）。
///
/// 只认以 `#[` 开头的属性行：注释（`//` / `//!` / `///`）与函数体内的普通断言
/// 不以属性形态出现，不在扫描范围——用例源文件文档里对标记的字面举例不构成
/// 自身漏检。
fn is_skip_or_exemption(line: &str) -> bool {
    let trimmed = line.trim();
    if !(trimmed.starts_with("#[") || trimmed.starts_with("#![")) {
        return false;
    }
    // 忽略标记：`#[ignore]` / `#[ignore = "…"]`（属性头后紧跟 `]` / `=`）。
    let ignore = attribute_head(trimmed, "ignore");
    // `#[cfg_attr(…, ignore)]`：带参属性头 `#[cfg_attr(` + 参数含 `ignore` 词元。
    let cfg_attr_ignore = trimmed.contains("#[cfg_attr(") && contains_ignore_token(trimmed);
    // 整文件条件编译豁免：内层属性 `#![cfg(…)]` / `#![cfg_attr(…)]` 作用于整个
    // crate（文件）；外层 `#[cfg(…)]` 只作用于单个条目，不在本判据内。
    let file_level = trimmed.starts_with("#![cfg(") || trimmed.starts_with("#![cfg_attr(");
    ignore || cfg_attr_ignore || file_level
}

/// 匹配器自检：判据的判别力必须在取证前自证——`#[ignore = "…"]` 等合法写法
/// 必须命中，近似串（`#[ignore_something]`）与注释中的字面举例必须放行。
fn matcher_self_check() {
    for marker in [
        "#[ignore]",
        "    #[ignore]",
        r#"#[ignore = "flaky"]"#,
        "#[cfg_attr(unix, ignore)]",
        r#"#[cfg_attr(feature = "x", ignore)]"#,
        "#![cfg(unix)]",
        "#![cfg_attr(unix, allow(dead_code))]",
    ] {
        assert!(
            is_skip_or_exemption(marker),
            "匹配器漏检合法跳过/豁免写法：{marker}"
        );
    }
    for non_marker in [
        "#[ignore_something]",
        "#[cfg(unix)]",
        "#[cfg_attr(unix, ignored)]",
        "// #[ignore]",
        "/// #[ignore] 的字面举例",
        "assert!(!value);",
        "let ignore = 1;",
    ] {
        assert!(
            !is_skip_or_exemption(non_marker),
            "匹配器误伤非标记行：{non_marker}"
        );
    }
}

#[test]
fn no_ignore_markers_or_file_level_cfg_in_acceptance_sources() {
    matcher_self_check();
    let dir = workspace_root().join("tests/acceptance");
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.join("Cargo.toml").display()));
    let declared = manifest_test_paths(&manifest);
    let mut discovered = directory_sources(&dir);

    // 双向核对：清单与目录必须一一对应（新增目标漏声明 / 漏扫描都判失败）。
    let mut declared_sorted = declared.clone();
    declared_sorted.sort();
    assert_eq!(
        declared_sorted, discovered,
        "tests/acceptance/Cargo.toml 的 [[test]] 声明与目录内 *.rs 源文件不一一对应：\
         声明 {declared:?}，目录 {discovered:?}"
    );
    assert!(
        !declared.is_empty(),
        "tests/acceptance/Cargo.toml 未声明任何 [[test]] 目标"
    );

    for relative in &declared {
        let path: PathBuf = dir.join(relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for (lineno, line) in text.lines().enumerate() {
            if is_skip_or_exemption(line) {
                panic!(
                    "{}:{lineno} 含跳过/豁免标记（判据 TC-tech-02）：{}",
                    path.display(),
                    line.trim()
                );
            }
        }
    }
    discovered.clear();
}
