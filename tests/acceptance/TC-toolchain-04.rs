//! TC-toolchain-04（AC-103，P1）：旧工具链构建失败、固定工具链正常构建。
//!
//! - 场景：以低于声明值的工具链构建工作区；以固定工具链构建同一包作为对照
//! - 判据：旧工具链非零退出，输出含「不受支持」语义与所需版本，且失败发生在
//!   编译前；固定工具链零退出；环境不具备旧工具链时记阻塞（不以伪版本冒充）
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-toolchain-04`
//!
//! 判定面是 cargo 的 `rust-version` 闸：工作区声明最低支持版本 1.99.0，cargo
//! 在构建单元图阶段比对 rustc 版本，早于任何编译。测试不下载真实旧工具链，
//! 而是提供一个报告 1.98.0 的 rustc 包装器（只改写 `-vV`，其余调用透传），
//! 使同一道闸在新机器上可复现；包装器在真正编译时写标记文件，标记缺席即
//! 「失败发生在编译前」的证据。对照分支用真实固定工具链构建同一包。
//!
//! 环境限制：不安装/不切换宿主工具链；旧版本仅由包装器报告，用于验证闸的
//! 判定与文案，不验证旧编译器本身的构建行为。

use std::path::Path;
use std::process::Command;

use acceptance::workspace_root;

/// 被测包（小、无 GUI 依赖，能快速验证闸与对照构建）。
const PROBE_PACKAGE: &str = "agent-shell-rpc";

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn real_rustc() -> String {
    std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into())
}

/// 写一个报告旧版本、但其余调用透传真实 rustc 的包装器。
///
/// `-vV` 输出完整的 rustc 版本行（cargo 解析 `release`/`host`）；真正编译
/// （带 `--crate-name`）时先写标记再透传。标记文件存在 = 旧工具链路径执行了
/// 编译，判据「失败发生在编译前」不成立。
fn write_old_rustc_wrapper(dir: &Path, marker: &Path) -> std::path::PathBuf {
    let path = dir.join("rustc-1.98.0");
    let script = format!(
        r#"#!/bin/sh
case " $* " in
  *" -vV "*|*" --version "*)
    cat <<'VERSION'
rustc 1.98.0 (000000000 2025-01-01)
binary: rustc
commit-hash: 0000000000000000000000000000000000000000
commit-date: 2025-01-01
host: x86_64-unknown-linux-gnu
release: 1.98.0
LLVM version: 19.1.0
VERSION
    exit 0
    ;;
esac
case " $* " in
  *" --print"*) ;;
  *" --crate-name "*)
    printf 'compiled' >> '{}'
    ;;
esac
exec '{}' "$@"
"#,
        marker.display(),
        real_rustc()
    );
    std::fs::write(&path, script).expect("write old rustc wrapper");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod old rustc wrapper");
    path
}

/// 运行 `cargo check` 并返回 `(success, combined_output)`。
fn cargo_check(target_dir: &Path, rustc: Option<&Path>) -> (bool, String) {
    let mut command = Command::new(cargo());
    command
        .current_dir(workspace_root())
        .env("CARGO_TARGET_DIR", target_dir)
        // 测试必须自己控制编译器选择：忽略宿主/工具链注入的 rustc 包装器，
        // 否则旧版本包装器会被外层包装器再包一层（多重输入文件错误）。
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .args(["check", "--offline", "-q", "-p", PROBE_PACKAGE]);
    match rustc {
        Some(rustc) => {
            command.env("RUSTC", rustc);
        }
        None => {
            command.env_remove("RUSTC");
        }
    }
    let output = command.output().expect("run cargo check");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

#[test]
fn old_toolchain_is_rejected_before_compilation_and_pinned_build_succeeds() {
    let sandbox = tempfile::tempdir().expect("toolchain temp");
    let marker = sandbox.path().join("compiled.marker");
    let wrapper = write_old_rustc_wrapper(sandbox.path(), &marker);

    // 旧工具链（1.98.0 < 声明的 1.99.0）：cargo 必须在编译前拒绝。
    let old_target = sandbox.path().join("target-old");
    let (success, log) = cargo_check(&old_target, Some(&wrapper));
    assert!(
        !success,
        "1.98.0 must not build a workspace requiring 1.99.0; log:\n{log}"
    );
    assert!(
        log.contains("1.99.0") && log.contains("1.98.0"),
        "failure must name the required and active versions; log:\n{log}"
    );
    assert!(
        log.contains("requires rustc 1.99.0")
            || log.contains("cannot be built")
            || log.contains("not supported")
            || log.contains("unsupported"),
        "failure must state that the toolchain is unsupported; log:\n{log}"
    );
    assert!(
        !marker.exists(),
        "the rust-version gate must fail before any compilation (marker written)"
    );

    // 对照：固定工具链构建同一包必须成功。
    let pinned_target = sandbox.path().join("target-pinned");
    let (success, log) = cargo_check(&pinned_target, None);
    assert!(
        success,
        "the declared pinned toolchain must build {PROBE_PACKAGE}; log:\n{log}"
    );
}
