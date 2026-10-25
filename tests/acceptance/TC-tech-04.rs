//! TC-tech-04（AC-111，P1）：根工作区未剔除被测包。
//!
//! - 场景：根工作区未把目标包从默认成员集合中剔除，也未将其列入排除清单
//! - 判据：目标包属于默认成员集合且不在排除清单；被剔除即判失败；显式点名该包
//!   构建不构成覆盖证据
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-04`
//!
//! 取证对象是根 `Cargo.toml`（`members` / `default-members` / `exclude`）与
//! `cargo metadata --no-deps` 的 `workspace_default_members` 事实：`exclude`
//! 列入被测包即被剔除；`default-members` 未含被测包即默认构建不覆盖它。判定与
//! 反证入口在 `acceptance::apparatus`，与折叠判别力用例（TC-tech-07/08）同源。

use acceptance::apparatus::{self, Evidence, DEFAULT_MEMBERS_BRANCH, NO_EXCLUSION_BRANCH};
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
async fn acceptance_package_is_default_member_and_not_excluded() {
    apparatus::assert_baseline(&[NO_EXCLUSION_BRANCH, DEFAULT_MEMBERS_BRANCH])
        .unwrap_or_else(|e| panic!("{e}"));

    // (1) 根清单排除清单不得含被测包。
    let root_manifest = workspace_root().join("Cargo.toml");
    let text = std::fs::read_to_string(&root_manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", root_manifest.display()));
    let exclusion_verdict = apparatus::judge_no_exclusion(&Evidence::RootManifest(text));
    exclusion_verdict.require_pass("TC-tech-04");

    // (2) 构建元数据：被测包属于默认成员集合（workspace_default_members）。
    let metadata = default_metadata();
    let members_verdict =
        apparatus::judge_default_members(&Evidence::BuildMetadata(metadata.to_string()));
    members_verdict.require_pass("TC-tech-04");
}
