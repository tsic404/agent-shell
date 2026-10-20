//! TC-tech-03（AC-111，P1）：包清单未关闭自动测试发现，目标在测试腿中启用。
//!
//! - 场景：目标包未关闭自动测试发现，且被测目标未被显式声明为不参与测试腿
//! - 判据：包清单未关闭自动测试发现；构建元数据中该目标在测试腿中处于启用状态
//!   （未被显式声明为不参与测试腿）；任一不满足即判失败
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-03`
//!
//! 取证分两面：(1) `tests/acceptance/Cargo.toml` 不得有 `autotests = false`
//! （关闭自动测试发现的唯一清单开关），TC-tech 目标也不得被清单显式声明为
//! `test = false` / `harness = false`（显式排除出默认测试腿）；(2) `cargo
//! metadata --no-deps` 的构建元数据中 TC-tech-01~06 目标 `test: true`（在测试
//! 腿中启用）。本目标自身同样必须满足这两条（自证），否则取证循环失察。

use acceptance::workspace_root;
use serde_json::Value;
use std::process::Command;

/// 设计仓用例标识（TC-tech 家族，与本判据相关的全部目标）。
const TC_TECH_TARGETS: &[&str] = &[
    "TC-tech-01",
    "TC-tech-02",
    "TC-tech-03",
    "TC-tech-04",
    "TC-tech-05",
    "TC-tech-06",
];

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
async fn autotests_not_disabled_and_targets_active_in_test_leg() {
    // (1) 包清单未关闭自动测试发现，且 TC-tech 目标未被显式声明为不参与测试腿
    //     （`test = false` 把目标排除出测试腿；`harness = false` 换自定义运行器，
    //     均不由默认测试腿执行——清单层的显式声明）。
    let manifest = workspace_root().join("tests/acceptance/Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    assert!(
        !text.lines().any(|l| l.trim() == "autotests = false"),
        "tests/acceptance/Cargo.toml 关闭了自动测试发现（autotests = false），\
         判据 TC-tech-03 不满足"
    );
    for tc in TC_TECH_TARGETS {
        for decl in ["test = false", "harness = false"] {
            assert!(
                !text.lines().any(|l| l.trim() == decl),
                "{tc} 相关声明 {decl} 出现在包清单中——该目标被显式声明为\
                 不参与测试腿，判据 TC-tech-03 不满足"
            );
        }
    }

    // (2) 构建元数据中目标在测试腿中启用。
    let meta = default_metadata();
    let package = meta["packages"]
        .as_array()
        .expect("metadata carries packages")
        .iter()
        .find(|p| p["name"] == "agent-shell-acceptance")
        .unwrap_or_else(|| panic!("agent-shell-acceptance 不在构建元数据中"));
    let targets = package["targets"].as_array().expect("package targets");

    for tc in TC_TECH_TARGETS {
        let target = targets
            .iter()
            .find(|t| t["name"] == *tc)
            .unwrap_or_else(|| panic!("目标 {tc} 不在构建元数据中"));
        assert_eq!(
            target["test"].as_bool(),
            Some(true),
            "{tc} 未在测试腿中启用（test != true）：{}",
            target
        );
    }
}
