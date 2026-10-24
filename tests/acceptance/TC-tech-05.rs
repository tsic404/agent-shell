//! TC-tech-05（AC-111，P1）：默认 CI 的 Rust 测试步骤实际执行该目标且全部通过。
//!
//! - 场景：默认 CI 的 Rust 测试步骤实际执行该测试目标且全部通过
//! - 判据：运行日志含该目标的执行行、通过计数与零忽略计数；日志缺失、目标未
//!   出现、出现失败或忽略即判失败
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-tech-05`
//!
//! 默认 CI（`.github/workflows/pr-check.yml` 的 Test (Rust) 步骤）以
//! `cargo test --all-targets` 执行整个工作区——该命令含本包全部测试目标。
//! 本用例以同一命令形态限定本包、显式点名 TC-tech 兄弟目标（排除自身避免
//! 递归执行），在子进程运行后逐目标核对日志。切段核对与反证入口在
//! `acceptance::apparatus`，与折叠判别力用例（TC-tech-07/08）同源。

use acceptance::apparatus::{
    self, Evidence, LOG_EVIDENCE_ABSENT_BRANCH, LOG_EVIDENCE_BRANCH, TC_TECH_TARGETS,
};
use acceptance::workspace_root;
use std::path::PathBuf;
use std::process::Command;

/// 默认 CI 的 Rust 测试步骤命令（pr-check.yml Test (Rust) 步骤的入口）。
const CI_TEST_STEP_COMMAND: &str = "cargo test --all-targets";

/// 本目标自身（子进程过滤器里排除，避免递归执行）。
const SELF_TARGET: &str = "TC-tech-05";

/// 子进程执行的 TC-tech 兄弟目标（取证名单去掉自身）。
fn sibling_targets() -> Vec<&'static str> {
    TC_TECH_TARGETS
        .iter()
        .copied()
        .filter(|target| *target != SELF_TARGET)
        .collect()
}

/// 读取工作流文件：断言 CI 的 Test 步骤以 `--all-targets` 执行（该步骤是
/// 判据所指的「默认 CI 的 Rust 测试步骤」）。
fn ci_test_step_covers_all_targets() {
    let workflow = workspace_root().join(".github/workflows/pr-check.yml");
    let text = std::fs::read_to_string(&workflow)
        .unwrap_or_else(|e| panic!("read {}: {e}", workflow.display()));
    assert!(
        text.contains(CI_TEST_STEP_COMMAND),
        "pr-check.yml 的 Test 步骤不含 `{CI_TEST_STEP_COMMAND}`——默认 CI 未以\
         --all-targets 执行测试目标，判据 TC-tech-05 不满足"
    );
}

/// 以 CI 同形态（`cargo test --all-targets` + 本包限定）运行兄弟目标，返回
/// 捕获的运行日志（stdout + stderr）。
fn run_sibling_targets_log(targets: &[&str]) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // --offline：能跑起本测试说明外层 cargo 已解析并构建过全部依赖（同
    // cli/tests/socket_endpoint_e2e 的既有惯例），省掉 registry 索引往返——
    // 无 crates.io 出口的机器上这一步要卡数十秒。
    // 不带 --all-targets：它与 --test 同时给出时 --test 过滤器失效、会跑全部
    // 目标（含本目标 → 递归）；CI 的 --all-targets 形态由 ci_test_step_covers_
    // all_targets 对工作流文件的断言承载。
    // 不带 --quiet：判据要「运行日志含执行行」，`--quiet` 会吞掉
    // `Running <TC-id>.rs` 行。
    let mut args = vec!["test", "--offline", "-p", "agent-shell-acceptance"];
    for target in targets {
        args.push("--test");
        args.push(target);
    }
    // `sh -c 'exec "$@" 2>&1'`：把 cargo 的 `Running …` 行（stderr）与 harness 的
    // `test result:` 行（stdout）合到同一有序流——`Command::output()` 分管道捕获
    // 会让两类行各自成块、无法按目标切段。CI 日志本身就是合流形态。
    let output = Command::new("sh")
        .current_dir(workspace_root())
        .arg("-c")
        .arg(r#"exec "$@" 2>&1"#)
        .arg("sh")
        .arg(&cargo)
        .args(&args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{cargo} {}`: {e}", args.join(" ")));
    let log = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "子进程 `{cargo} {}` 退出非零：判据 TC-tech-05 不满足。日志：\n{log}",
        args.join(" ")
    );
    log
}

/// 断言目标源文件路径存在（取证对象完整）。
fn assert_sources_present() {
    let dir: PathBuf = workspace_root().join("tests/acceptance");
    for target in TC_TECH_TARGETS {
        let path = dir.join(format!("{target}.rs"));
        assert!(
            path.is_file(),
            "{} 不存在——判据 TC-tech-05 的取证对象缺失",
            path.display()
        );
    }
}

#[test]
fn ci_log_evidences_execution_pass_count_and_zero_ignored() {
    // 基线反证：证据齐全即通过、依据缺失必须判失败（不得折叠为跳过）。
    apparatus::assert_baseline(&[LOG_EVIDENCE_BRANCH, LOG_EVIDENCE_ABSENT_BRANCH])
        .unwrap_or_else(|e| panic!("{e}"));

    ci_test_step_covers_all_targets();
    assert_sources_present();
    let targets = sibling_targets();
    let log = run_sibling_targets_log(&targets);
    for target in &targets {
        let evidence = Evidence::RunLog {
            target: (*target).to_string(),
            text: log.clone(),
        };
        apparatus::judge_log_evidence(&evidence).require_pass("TC-tech-05");
    }
}
