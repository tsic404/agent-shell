//! TC-tech-01（AC-111，P1）：测试目标在默认构建的测试腿中可见。
//!
//! - 场景：测试目标在默认构建的测试腿中可见——归属被测包且无特性前置
//! - 判据：构建元数据中该目标存在、归属被测包、所需特性为空；目标缺失或被
//!   特性门控即判该测试未被实际覆盖
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-01`
//!
//! 本用例以 `cargo metadata --no-deps` 的构建事实取证（判据本身即「构建元数据
//! 中该目标存在、归属被测包、所需特性为空」）：目标名与设计仓用例标识一致、
//! `targets[].src_path` 落在 `tests/acceptance/` 内、`targets[].required-features`
//! 为空。`cargo metadata` 的默认输出即默认构建的元数据；本目标自身也由该
//! 元数据列出且无特性前置——通过即同时自证「本用例的目标可见」。
//!
//! 环境变量是进程级状态（夹具的 `headless_session`），本目标保持单个
//! `#[tokio::test]`。

use acceptance::apparatus::TC_TECH_TARGETS;
use acceptance::workspace_root;
use serde_json::Value;
use std::process::Command;

/// 默认构建的元数据（`--no-deps`：目标集合与依赖图无关，只读本工作区事实）。
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

#[tokio::test]
async fn tc_tech_targets_are_visible_in_build_metadata() {
    let meta = default_metadata();
    let package = meta["packages"]
        .as_array()
        .expect("metadata carries packages")
        .iter()
        .find(|p| p["name"] == "agent-shell-acceptance")
        .unwrap_or_else(|| {
            panic!(
                "agent-shell-acceptance 不在默认构建元数据中：{}",
                meta["packages"]
            )
        });
    let targets = package["targets"].as_array().expect("package targets");

    for tc in TC_TECH_TARGETS {
        let target = targets
            .iter()
            .find(|t| t["name"] == *tc)
            .unwrap_or_else(|| panic!("目标 {tc} 不在构建元数据中：{}", package["targets"]));
        // 归属被测包：源文件落在 tests/acceptance/ 内（设计仓规定的按用例标识目录）。
        let src = target["src_path"].as_str().unwrap_or_default();
        assert!(
            src.contains("/tests/acceptance/"),
            "{tc} 源路径不在 tests/acceptance/ 内：{src}"
        );
        assert_eq!(
            target["kind"]
                .as_array()
                .and_then(|k| k.first())
                .and_then(Value::as_str),
            Some("test"),
            "{tc} 必须是测试目标：{}",
            target
        );
        // 无特性前置：required-features 缺失或为空数组。
        let required = target.get("required-features").and_then(Value::as_array);
        assert!(
            required.is_none_or(|f| f.is_empty()),
            "{tc} 被特性门控（required-features 非空）：{}",
            target
        );
    }
}
