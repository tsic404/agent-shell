//! TC-toolchain-03（AC-102，P1）：清单声明的 rust-version 与工具链声明一致。
//!
//! - 场景：工作区 `rust-version` 与根工具链声明文件同源同值，全部成员继承而非
//!   各自写死。
//! - 判据：根 `[workspace.package] rust-version` 等于声明文件 channel；每个成员
//!   清单都写 `rust-version.workspace = true` 且无一处硬编码；`cargo metadata
//!   --no-deps` 中每个包的 `rust_version` 均等于该值且无 null；成员数与包数一致
//!   （43）；根清单无 `default-members` / `exclude` 收窄或剔除成员。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-toolchain-03`
//!
//! 取证两面：文本侧解析根清单与 `cargo metadata --no-deps` 给出的每个成员
//! `manifest_path`；构建元数据侧读取 `packages[].rust_version`。`rust-version`
//! 的继承值只在解析后的元数据中可见，硬编码检测则必须在清单文本上做（元数据
//! 无法区分继承与硬编码）——两面合起来才判「继承且同值」。
//!
//! 环境限制：需要 `cargo` 可执行（取 `$CARGO`，退回 `cargo`）；`cargo metadata`
//! 只读清单与路径，不编译、不需要 fixture。

use acceptance::workspace_root;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (idx, ch) in line.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '#' => return &line[..idx],
            None => {}
        }
    }
    line
}

/// 解析 `[section]` 内的 `key = value`（去掉行内注释与首尾空白）。
fn section_field(text: &str, section: &str, key: &str) -> Option<String> {
    let mut current = String::new();
    for line in text.lines() {
        let line = strip_comment(line).trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name.trim().to_string();
            continue;
        }
        if current != section {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            continue;
        };
        if lhs.trim() == key {
            return Some(rhs.trim().to_string());
        }
    }
    None
}

/// 解析 `[workspace] members = [...]`（单行与多行两种写法）。
fn workspace_members(root_manifest: &str) -> Vec<String> {
    let mut members = Vec::new();
    let mut in_workspace = false;
    let mut in_members = false;
    for raw in root_manifest.lines() {
        let line = strip_comment(raw).trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_workspace = name.trim() == "workspace";
            in_members = false;
            continue;
        }
        if !in_workspace {
            continue;
        }
        if !in_members {
            if let Some(rhs) = line
                .strip_prefix("members")
                .and_then(|l| l.trim().strip_prefix('='))
            {
                in_members = true;
                push_members(rhs, &mut members, &mut in_members);
            }
            continue;
        }
        push_members(line, &mut members, &mut in_members);
    }
    members
}

fn push_members(text: &str, members: &mut Vec<String>, in_members: &mut bool) {
    for part in text.split('"').skip(1).step_by(2) {
        members.push(part.to_string());
    }
    if text.contains(']') {
        *in_members = false;
    }
}

/// 清单文本的非注释行（用于判定硬编码与继承写法）。
fn code_lines(text: &str) -> Vec<&str> {
    text.lines().map(strip_comment).map(str::trim).collect()
}

fn metadata() -> Value {
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

#[test]
fn rust_version_matches_toolchain_and_is_inherited_everywhere() {
    let root = workspace_root();
    let root_manifest = fs::read_to_string(root.join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("read root Cargo.toml: {e}"));
    let toolchain = fs::read_to_string(root.join("rust-toolchain.toml"))
        .unwrap_or_else(|e| panic!("read rust-toolchain.toml: {e}"));

    // ---- (1) channel 与根 rust-version 同值 ----
    let channel = section_field(&toolchain, "toolchain", "channel")
        .expect("rust-toolchain.toml 缺少 channel");
    let channel = channel
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or_else(|| panic!("channel 必须用双引号包裹：{channel}"))
        .to_string();
    let rust_version = section_field(&root_manifest, "workspace.package", "rust-version")
        .expect("根清单缺少 [workspace.package] rust-version");
    let rust_version = rust_version
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(rust_version.as_str())
        .to_string();
    assert_eq!(
        rust_version, channel,
        "根清单 rust-version 与工具链声明 channel 不一致"
    );

    // ---- (2) 根清单未收窄/剔除成员 ----
    for key in ["default-members", "exclude"] {
        let hit = code_lines(&root_manifest).into_iter().find(|l| {
            l.strip_prefix(key)
                .is_some_and(|r| r.trim_start().starts_with('='))
        });
        assert!(
            hit.is_none(),
            "根清单出现 {key}，成员集合被收窄或剔除：{:?}",
            hit
        );
    }
    let members = workspace_members(&root_manifest);
    assert_eq!(
        members.len(),
        43,
        "根清单 members 数量应为 43，实得 {}：{members:?}",
        members.len()
    );

    // ---- (3) 每个成员清单继承而非硬编码 ----
    let meta = metadata();
    let packages = meta["packages"]
        .as_array()
        .expect("metadata carries packages");
    assert_eq!(
        packages.len(),
        members.len(),
        "metadata 包数与根清单 members 数不一致：{} vs {}",
        packages.len(),
        members.len()
    );
    for package in packages {
        let name = package["name"].as_str().unwrap_or("<unnamed>");
        let manifest_path = package["manifest_path"]
            .as_str()
            .unwrap_or_else(|| panic!("包 {name} 缺少 manifest_path"));
        let text = fs::read_to_string(Path::new(manifest_path))
            .unwrap_or_else(|e| panic!("read {manifest_path}: {e}"));
        let lines = code_lines(&text);
        assert!(
            lines.contains(&"rust-version.workspace = true"),
            "包 {name} 的清单未继承根 rust-version（缺少 `rust-version.workspace = true`）：{manifest_path}"
        );
        let hardcoded: Vec<&&str> = lines
            .iter()
            .filter(|l| {
                l.strip_prefix("rust-version")
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            })
            .collect();
        assert!(
            hardcoded.is_empty(),
            "包 {name} 的清单硬编码 rust-version，未继承根声明：{hardcoded:?} ({manifest_path})"
        );

        // ---- (4) 元数据中继承后的值等于声明 ----
        let value = package
            .get("rust_version")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("包 {name} 的 rust_version 为 null 或缺失：{package}"));
        assert_eq!(
            value, channel,
            "包 {name} 解析后的 rust_version 与工具链声明不一致"
        );
    }
}
