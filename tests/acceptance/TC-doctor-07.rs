//! TC-doctor-07（AC-07，P0）：版本一致性作为排除假失败的前置闸。
//!
//! - 场景：版本输出与构建期提交标识
//! - 判据：包版本与构建期提交短标识均存在，且提交标识等于所测提交；不一致时
//!   不得直接出具失败结论（流程约束）
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-doctor-07`
//!
//! 判定面是真实 CLI 二进制：`agent-shell --version` 必须打印
//! `<包版本> (<构建期 commit>)`。测试先构建二进制（增量构建），再与
//! `git rev-parse --short=7 HEAD` 及工作区脏标记比对——旧包冒验新修复正是
//! 这条闸要排除的假失败。

use std::path::PathBuf;
use std::process::Command;

use acceptance::workspace_root;

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn target_dir(root: &std::path::Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
}

/// 构建 CLI 二进制并返回路径（增量构建；缺失时完整构建一次）。
fn cli_binary() -> PathBuf {
    let root = workspace_root();
    let mut build = Command::new(cargo());
    build.current_dir(&root).args([
        "build",
        "-q",
        "-p",
        "agent-shell-cli",
        "--bin",
        "agent-shell",
    ]);
    let output = build.output().expect("run cargo build for the CLI binary");
    assert!(
        output.status.success(),
        "building agent-shell must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let binary = target_dir(&root).join(profile).join("agent-shell");
    assert!(
        binary.is_file(),
        "expected CLI binary at {}",
        binary.display()
    );
    binary
}

#[tokio::test]
async fn version_embeds_package_version_and_build_commit() {
    let binary = tokio::task::spawn_blocking(cli_binary)
        .await
        .expect("join version build");
    let output = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("run agent-shell --version");
    assert!(
        output.status.success(),
        "--version must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim_end_matches('\n');
    let prefix = format!("agent-shell {} (", env!("CARGO_PKG_VERSION"));
    assert!(
        line.starts_with(&prefix) && line.ends_with(')'),
        "version line must be `agent-shell <version> (<commit>)`, got: {line:?}"
    );
    let commit = &line[prefix.len()..line.len() - 1];
    assert_ne!(
        commit, "unknown",
        "build-time commit is missing; the version gate cannot certify the tested revision"
    );
    let (sha, suffix) = match commit.split_once('-') {
        Some((sha, suffix)) => (sha, Some(suffix)),
        None => (commit, None),
    };
    assert!(
        sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()),
        "build commit must start with a short hex id, got: {commit:?}"
    );

    // 与所测提交核对：sha 必须等于被测 HEAD；脏标记必须与实际工作区一致。
    // 任一环节无法取得（git 不可用、输出异常）都判失败——版本闸不得静默跳过。
    let root = workspace_root();
    let head = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .expect("git rev-parse");
    assert!(
        head.status.success(),
        "git rev-parse must succeed to verify the version gate"
    );
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    assert_eq!(
        sha, head,
        "build commit {sha} must equal the tested revision {head}"
    );
    let dirty = Command::new("git")
        .current_dir(&root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .expect("git status");
    assert!(
        dirty.status.success(),
        "git status must succeed to verify the dirty marker"
    );
    let is_dirty = !String::from_utf8_lossy(&dirty.stdout).is_empty();
    match suffix {
        None => assert!(
            !is_dirty,
            "version reports a clean tree but the tested worktree is dirty"
        ),
        Some("dirty") | Some("dirty-unknown") => assert!(
            is_dirty,
            "version reports a dirty suffix but the tested worktree is clean"
        ),
        Some(other) => panic!("unexpected build commit suffix: {other:?}"),
    }
}
