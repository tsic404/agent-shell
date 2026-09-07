//! 混合版本跨进程 e2e（TSI-2873）：旧 daemon（pre-21c7267 布尔 capabilities）
//! + 新 CLI。
//!
//! 旧 daemon 常驻用户会话、postinst 升级不重启，新 CLI 反序列化旧 daemon 的
//! 布尔 `capabilities` 时不得硬失败（`CapabilityStatus` 同收布尔与 snake_case
//! 字符串）。既有 rpc 单测只覆盖进程内反序列化，本测试用真实 `agent-shell`
//! 二进制连接旧线格式夹具 daemon，端到端验证跨进程路径。
//!
//! 夹具选择机制：`DaemonClient::connect` 经 `find_daemon_binary` 定位 daemon，
//! 查找顺序为「自身所在目录 → target 目录 → PATH」。把真实 `agent-shell` 与
//! 旧线格式夹具（命名 `agent-shell-daemon`）复制到同一临时目录后运行，
//! exe_dir 优先命中夹具，从而驱动真实 CLI 走旧布尔反序列化路径。

use std::path::PathBuf;
use std::process::Command;

#[test]
fn info_deserializes_legacy_bool_capabilities_without_hard_failure() {
    let cli_bin = PathBuf::from(env!("CARGO_BIN_EXE_agent-shell"));
    let legacy_bin = PathBuf::from(env!("CARGO_BIN_EXE_agent-shell-legacy-daemon"));

    let dir = tempfile::tempdir().expect("tempdir");
    // 必须复制（非符号链接）：`std::env::current_exe` 在 Linux 经
    // /proc/self/exe 解析符号链接到真实 target 目录，会遮蔽夹具发现规则。
    // Windows 下可执行文件带 `.exe` 后缀；复制目标必须用 EXE_SUFFIX 拼接，
    // 否则 `Command::new` 指向无扩展名文件直接失败（Linux 下为空串，行为不变）。
    let cli_copy = dir
        .path()
        .join(format!("agent-shell{}", std::env::consts::EXE_SUFFIX));
    let daemon_copy = dir.path().join(format!(
        "agent-shell-daemon{}",
        std::env::consts::EXE_SUFFIX
    ));
    std::fs::copy(&cli_bin, &cli_copy).expect("copy agent-shell binary");
    std::fs::copy(&legacy_bin, &daemon_copy).expect("copy legacy daemon binary");

    let output = Command::new(&cli_copy)
        .arg("info")
        .output()
        .expect("run agent-shell info");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "info must exit 0 against legacy daemon; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("error"),
        "no deserialization hard failure expected; stderr: {stderr}"
    );
    // 夹具特有的 detection 串：证明命中的是旧线格式夹具而非 target 里的
    // 新 daemon（否则新 daemon 也渲染 ✓/✗，测试会给出虚假信心）。
    assert!(
        stdout.contains("legacy daemon"),
        "must exercise the legacy fixture daemon, not the new daemon; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("window_management") && stdout.contains('✓'),
        "window_management:true must render ✓; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("window_events") && stdout.contains('✗'),
        "window_events:false must render ✗; stdout:\n{stdout}"
    );
}
