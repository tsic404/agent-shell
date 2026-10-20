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
//! 递归执行），在子进程运行后逐目标断言日志：`Running tests/TC-tech-0X.rs`
//! 执行行、`test result: ok. 1 passed; 0 failed; 0 ignored` 通过计数与零忽略
//! 计数。本目标自身在该子进程之外由外层 `cargo test`（CI 的 --all-targets）
//! 执行——若外层未执行它，本用例体根本不会运行，执行行与通过计数由外层
//! 日志天然承载。CI 日志因此含全部六个 TC-tech 目标的执行行、通过计数与
//! 零忽略计数。
//!
//! 夹具的 `headless_session` 是进程级环境修改，本目标不装配 daemon；子进程
//! 继承当前环境，不修改。

use acceptance::workspace_root;
use std::path::PathBuf;
use std::process::Command;

/// 默认 CI 的 Rust 测试步骤命令（pr-check.yml Test (Rust) 步骤的入口）。
const CI_TEST_STEP_COMMAND: &str = "cargo test --all-targets";

/// 子进程执行的 TC-tech 兄弟目标（排除本目标 TC-tech-05 避免递归）。
const SIBLING_TARGETS: &[&str] = &[
    "TC-tech-01",
    "TC-tech-02",
    "TC-tech-03",
    "TC-tech-04",
    "TC-tech-06",
];

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
fn run_sibling_targets_log() -> String {
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
    for target in SIBLING_TARGETS {
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

/// 逐目标核对运行日志：执行行、通过计数、零忽略计数（判据逐字核对）。
///
/// 按目标切段：以 `Running <name>.rs` 起、到该段首个 `test result:` 行为止。
/// `Running <name>.rs (target/...)`：目标源文件在包根时 cargo 打印目标名
/// （不带 `tests/` 前缀）；计数只在该目标自己的结果行里核对——其他目标打印
/// 的 `1 passed` 不能替当前目标作证，被过滤到零用例（`0 passed … N filtered
/// out`）即判失败。
fn check_target_log_evidence(log: &str, target: &str) -> Result<(), String> {
    let run_line = format!("Running {target}.rs");
    let Some(start) = log.find(&run_line) else {
        return Err(format!("日志缺 {target} 的执行行（{run_line}）：\n{log}"));
    };
    let segment = &log[start..];
    let Some(result_pos) = segment.find("test result:") else {
        return Err(format!("{target} 的执行段缺 test result 行：\n{segment}"));
    };
    let result_line = segment[result_pos..].lines().next().unwrap_or_default();
    let section = &segment[..result_pos + result_line.len()];
    if !result_line.contains("test result: ok.") {
        return Err(format!("{target} 的结果行不是通过形态：{result_line}"));
    }
    // 通过计数与零忽略计数绑定到本目标：恰好 1 个用例通过、0 失败、0 忽略、
    // 0 被过滤（`--test` 过滤到零用例时 cargo 以退出码 0 打印 `0 passed`）。
    for needle in ["1 passed", "0 failed", "0 ignored", "0 filtered out"] {
        if !result_line.contains(needle) {
            return Err(format!(
                "{target} 的结果行缺「{needle}」（通过计数须绑定到该目标）：{result_line}"
            ));
        }
    }
    if section.contains("test result: FAILED") {
        return Err(format!(
            "日志出现失败结果（test result: FAILED）：判据 TC-tech-05 不满足。段：\n{section}"
        ));
    }
    Ok(())
}

/// 切段核对的自检：合成日志上正例必须通过，跨目标串味必须失败。
///
/// 覆盖审查指出的失明形态：某目标被 `--test` 过滤到零用例（`0 passed …
/// N filtered out`）时，不能被另一个目标的 `1 passed` 顶替为满足。
fn checker_self_check() {
    let log = concat!(
        "     Running TC-tech-01.rs (target/debug/deps/TC_tech_01-aaaa)\n",
        "running 1 test\n",
        "test a ... ok\n\n",
        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n",
        "     Running TC-tech-06.rs (target/debug/deps/TC_tech_06-bbbb)\n",
        "running 0 tests\n\n",
        "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s\n",
    );
    assert!(
        check_target_log_evidence(log, "TC-tech-01").is_ok(),
        "正例（1 passed; 0 filtered out）必须通过"
    );
    // 目标 TC-tech-06 自己被过滤到零用例：不得借用 TC-tech-01 的 `1 passed`。
    assert!(
        check_target_log_evidence(log, "TC-tech-06").is_err(),
        "零用例目标不得被其他目标的通过计数顶替"
    );
    // 目标未出现即失败。
    assert!(
        check_target_log_evidence(log, "TC-tech-02").is_err(),
        "日志缺目标执行行必须判失败"
    );
}

/// 断言目标源文件路径存在（取证对象完整）。
fn assert_sources_present() {
    let dir: PathBuf = workspace_root().join("tests/acceptance");
    for target in SIBLING_TARGETS.iter().chain(std::iter::once(&"TC-tech-05")) {
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
    checker_self_check();
    ci_test_step_covers_all_targets();
    assert_sources_present();
    let log = run_sibling_targets_log();
    for target in SIBLING_TARGETS {
        check_target_log_evidence(&log, target).unwrap_or_else(|e| panic!("{e}"));
    }
}
