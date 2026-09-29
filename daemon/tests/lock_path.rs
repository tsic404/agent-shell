//! `--lock-path` 路径守卫 e2e：真实 daemon 二进制，进程层面确认拒绝启动。
//!
//! 单条类型判定由 `single_instance` 单测覆盖；这里钉住「用户看到的后果」——
//! 路径被他人预埋符号链接时 daemon 退出非零、stderr 说明原因、链接目标未被改写。

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn daemon_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-shell-daemon"))
}

/// 锁路径是指向他处的符号链接：拒绝启动，且链接目标内容原样保留。
#[test]
fn daemon_refuses_symlinked_lock_path_and_leaves_target_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("victim.txt");
    std::fs::write(&target, "precious content").expect("write target");
    let lock = dir.path().join("agent-shell.lock");
    std::os::unix::fs::symlink(&target, &lock).expect("plant symlink");

    let out = Command::new(daemon_bin())
        .arg("--socket")
        .arg(dir.path().join("agent-shell.sock"))
        .arg("--lock-path")
        .arg(&lock)
        .arg("--idle-timeout-secs")
        .arg("5")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run agent-shell-daemon");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "daemon must refuse a symlinked lock path (stderr: {stderr})"
    );
    assert!(
        stderr.contains("symlink") && stderr.contains("agent-shell.lock"),
        "stderr must name the symlink and the path: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("target readable"),
        "precious content",
        "link target must not be truncated or rewritten"
    );
}

/// 普通文件锁路径照常启动（守卫只拦非普通文件），并能正常服务一次 RPC。
#[test]
fn plain_lock_file_still_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent-shell.sock");
    let lock = dir.path().join("agent-shell.lock");
    std::fs::write(&lock, "").expect("pre-create regular lock file");

    let mut child = Command::new(daemon_bin())
        .arg("--socket")
        .arg(&socket)
        .arg("--lock-path")
        .arg(&lock)
        .arg("--idle-timeout-secs")
        .arg("5")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    // 就绪判定走真实 RPC：装配（compositor/a11y 探测）完成后才会应答。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match std::os::unix::net::UnixStream::connect(&socket) {
            Ok(stream) => {
                use std::io::{BufRead, BufReader, Write};
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .expect("set timeout");
                let mut reader = BufReader::new(stream);
                reader
                    .get_mut()
                    .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"info.show\"}\n")
                    .expect("write request");
                let mut line = String::new();
                reader.read_line(&mut line).expect("read response");
                assert!(line.contains("\"result\""), "unexpected response: {line}");
                break;
            }
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "daemon did not serve with a regular lock file: {e}"
                );
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}
