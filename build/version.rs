//! 共享构建脚本：把构建树的 git commit 打进发布二进制。
//!
//! 四个发布二进制 crate（cli / daemon / mcp / rootd）通过各自 Cargo.toml 的
//! `build = "../build/version.rs"` 引用本脚本，从而在编译期获得
//! `AGENT_SHELL_GIT_COMMIT` 常量（见 §20.7 QA 二进制版本约定）。
//!
//! commit 解析顺序：
//!   1. `AGENT_SHELL_GIT_COMMIT` 环境变量覆盖（无 `.git` 的沙箱构建注入用）；
//!   2. `git rev-parse --short=7 HEAD`，并按 `git status` 判定干净/脏：
//!      - 干净（status 成功且无输出）→ 短哈希；
//!      - 脏（status 成功且有输出）→ `<短哈希>-dirty`；
//!      - status 本身失败（bare 仓库/权限错误）→ `<短哈希>-dirty-unknown`，
//!        绝不静默当干净；
//!   3. `unknown`（无 git 且无覆盖——QA 流程不应落入此分支）。
//!
//! `-dirty` 在 build.rs 重跑时采样：源码改动不触发重跑，增量构建可能残留
//! 上一采样结果。QA 发布必须从干净工作树构建（`git status --porcelain` 为空），
//! 或由打包/发布脚本显式注入 `AGENT_SHELL_GIT_COMMIT`。
//!
//! 目的：真机 QA 二进制自报 commit，`agent-shell --version` 即可回溯来源，
//! 防「旧包冒验新修复」（TSI-3094）。

use std::process::Command;

/// 运行 `git <args>`，成功且 stdout 非空时返回 trim 后的内容，否则 `None`。
///
/// 注意：本 helper 把「命令失败」与「成功但无输出」都折叠为 `None`，只适合
/// 输出必非空的查询（如 `rev-parse`）。`git status` 的空输出正是「干净」的
/// 合法结果，必须用 [`worktree_dirty`] 区分，否则会把干净树误当失败。
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 判定工作树是否有已跟踪文件改动：
/// - `Ok(false)`：干净（`git status` 成功且无输出）；
/// - `Ok(true)`：脏（成功且有输出）；
/// - `Err(())`：`git status` 本身失败（bare 仓库/权限错误），无法判定。
fn worktree_dirty() -> Result<bool, ()> {
    let out = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    Ok(!out.stdout.is_empty())
}

fn main() {
    // 显式覆盖优先：`AGENT_SHELL_GIT_COMMIT=$(git rev-parse ...) cargo build`。
    let mut commit = std::env::var("AGENT_SHELL_GIT_COMMIT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    if commit.is_none() {
        commit = git(&["rev-parse", "--short=7", "HEAD"]).map(|hash| match worktree_dirty() {
            Ok(true) => format!("{hash}-dirty"),
            Ok(false) => hash,
            Err(()) => format!("{hash}-dirty-unknown"),
        });
    }

    let commit = commit.unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AGENT_SHELL_GIT_COMMIT={commit}");
    println!("cargo:rerun-if-env-changed=AGENT_SHELL_GIT_COMMIT");

    // commit 切换必须触发 build.rs 重跑，否则产物残留旧 commit 标记——
    // 正是本 issue 要防的「旧包冒验」。
    // - HEAD 位于 per-worktree git dir（`--absolute-git-dir`）；
    // - refs/ 与 packed-refs 位于 common dir（worktree 场景 per-worktree dir
    //   不含它们；`git gc`/`pack-refs` 后分支前进只改 packed-refs）。
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
    }
    if let Some(common_dir) = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
        println!("cargo:rerun-if-changed={common_dir}/refs");
        println!("cargo:rerun-if-changed={common_dir}/packed-refs");
    }
}
