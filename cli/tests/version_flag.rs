//! `agent-shell --version` 行为测试（§20.7 QA 校验唯一入口）。
//!
//! 断言二进制实际打印 `<crate-version> (<git-commit>)`，而非仅进程内检查
//! clap 版本串——覆盖「构建脚本注入 commit → clap 版本输出」的完整链路，
//! 防止回归为裸 `CARGO_PKG_VERSION`（真机 QA 无法回溯来源）。

use std::path::PathBuf;
use std::process::Command;

#[test]
fn version_flag_reports_version_and_commit() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_agent-shell"));
    let output = Command::new(&bin)
        .arg("--version")
        .output()
        .expect("run agent-shell --version");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "--version must exit 0; stderr: {stderr}"
    );

    // 与 build/version.rs 注入的 commit 精确一致：同一构建里二者必相等。
    let expected = format!(
        "agent-shell {} ({})\n",
        env!("CARGO_PKG_VERSION"),
        env!("AGENT_SHELL_GIT_COMMIT")
    );
    assert_eq!(stdout, expected, "version line must embed build commit");
}
