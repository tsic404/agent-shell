//! TC-tech-04（AC-111，P1）：根工作区未剔除被测包。
//!
//! - 场景：根工作区未把目标包从默认成员集合中剔除，也未将其列入排除清单
//! - 判据：目标包属于默认成员集合且不在排除清单；被剔除即判失败；显式点名该包
//!   构建不构成覆盖证据
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-04`
//!
//! 取证对象是根 `Cargo.toml`（`members` / `default-members` / `exclude`）与
//! `cargo metadata --no-deps` 的 `workspace_default_members` 事实：`exclude`
//! 列入被测包即被剔除；`default-members` 未含被测包即默认构建不覆盖它；两者都
//! 不成立时，被测包必须落在 `workspace_default_members`（即默认成员集合）。
//! 显式点名构建（`-p`）不构成覆盖证据——判据要求的是**默认**成员集合覆盖。

use acceptance::workspace_root;
use serde_json::Value;
use std::process::Command;

fn default_metadata() -> Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(&cargo)
        .current_dir(workspace_root())
        .args(["--quiet", "metadata", "--format-version", "1", "--no-deps"])
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{cargo} metadata` in workspace: {e}"));
    assert!(
        output.status.success(),
        "`{cargo} metadata --no-deps` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata output is JSON")
}

/// 根清单的 `exclude` 列表（缺失 = 空）。
///
/// 同时识别单行 `exclude = ["tests/acceptance"]` 与多行数组写法；行内注释与
/// 数组后的残留文本不参与取值。
fn excluded_packages(root_manifest: &str) -> Vec<String> {
    let mut excluded = Vec::new();
    let mut lines = root_manifest.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        // `exclude` 键（排除 `excluded = …` 这类同前缀键）。
        let Some(rest) = trimmed.strip_prefix("exclude") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '=') {
            continue;
        }
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        // 收集数组体直到匹配的 `]`（单行或多行）。
        let mut body = rest.trim().to_string();
        while !body.contains(']') {
            match lines.next() {
                Some(next) => {
                    body.push(' ');
                    body.push_str(next.trim());
                }
                None => break,
            }
        }
        let body = body.split(']').next().unwrap_or_default();
        // 去掉数组开括号：单行 `["x"]` 与多行 `[\n "x",\n]` 的首项都因此干净。
        let body = body.trim_start().trim_start_matches('[');
        for entry in body.split(',') {
            let entry = entry
                .split('#')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if !entry.is_empty() && entry != "[" {
                excluded.push(entry.to_string());
            }
        }
    }
    excluded
}

/// 排除清单解析自检：单行、多行、缺失与同前缀键四种形态。
fn parser_self_check() {
    assert_eq!(
        excluded_packages("exclude = [\"tests/acceptance\"]\n"),
        vec!["tests/acceptance".to_string()],
        "单行 exclude 数组必须被识别"
    );
    assert_eq!(
        excluded_packages("[workspace]\nexclude = [\n    \"a\",\n    \"tests/acceptance\",\n]\n"),
        vec!["a".to_string(), "tests/acceptance".to_string()],
        "多行 exclude 数组必须被识别"
    );
    assert!(
        excluded_packages("[workspace]\nmembers = [\"tests/acceptance\"]\n").is_empty(),
        "无 exclude 键 = 空清单"
    );
    assert!(
        excluded_packages("excluded = [\"tests/acceptance\"]\n").is_empty(),
        "同前缀键 excluded 不得被误认作 exclude"
    );
}

#[tokio::test]
async fn acceptance_package_is_default_member_and_not_excluded() {
    parser_self_check();
    // (1) 根清单排除清单不得含被测包（`exclude` 条目逐一比对）。
    let root_manifest = workspace_root().join("Cargo.toml");
    let text = std::fs::read_to_string(&root_manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", root_manifest.display()));
    for entry in excluded_packages(&text) {
        assert_ne!(
            entry, "tests/acceptance",
            "根 Cargo.toml 的 exclude 剔除了被测包 tests/acceptance：\
             判据 TC-tech-04 不满足"
        );
    }

    // (2) 构建元数据：被测包属于默认成员集合（workspace_default_members）。
    let meta = default_metadata();
    let default_members = meta["workspace_default_members"]
        .as_array()
        .expect("workspace_default_members is an array");
    let listed = default_members.iter().any(|m| {
        m.as_str()
            .is_some_and(|s| s.contains("agent-shell-acceptance"))
    });
    assert!(
        listed,
        "被测包 agent-shell-acceptance 不在默认成员集合中：{default_members:?}；\
         显式点名构建不构成覆盖证据，判据 TC-tech-04 不满足"
    );
}
