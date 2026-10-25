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
//! metadata --no-deps` 的构建元数据中 `apparatus::TC_TECH_TARGETS` 名单内的全部
//! 目标 `test: true`（在测试腿中启用）。判定与反证入口在 `acceptance::apparatus`，
//! 与折叠判别力用例（TC-tech-08）同源。

use acceptance::apparatus::{self, Evidence, PACKAGE_MANIFEST_BRANCH, TEST_LEG_BRANCH};
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

#[tokio::test]
async fn autotests_not_disabled_and_targets_active_in_test_leg() {
    apparatus::assert_baseline(&[PACKAGE_MANIFEST_BRANCH, TEST_LEG_BRANCH])
        .unwrap_or_else(|e| panic!("{e}"));

    // (1) 包清单未关闭自动测试发现，且 TC-tech 目标未被显式排除出测试腿。
    let manifest = workspace_root().join("tests/acceptance/Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    let manifest_verdict = apparatus::judge_package_manifest(&Evidence::PackageManifest(text));
    manifest_verdict.require_pass("TC-tech-03");

    // (2) 构建元数据中目标在测试腿中启用（目标缺失即不可判定，判失败）。
    let metadata = default_metadata();
    let leg_verdict = apparatus::judge_test_leg(&Evidence::BuildMetadata(metadata.to_string()));
    leg_verdict.require_pass("TC-tech-03");
}
