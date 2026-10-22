//! TC-windows-07（AC-16，P1）：CLI `windows wait` 的成功与超时路径。
//!
//! - 场景：真实 `agent-shell` 二进制经显式 Unix socket 连到脚本化 daemon；
//!   `windows wait ghost --timeout 300ms` 在窗口出现时立即返回，在窗口始终
//!   缺席时于预算内以非零码退出
//! - 判据：成功退出码 0 并打印匹配窗口（native_id/title/app_id）；超时退出码
//!   非零且在 ~2s 内返回，stderr 面向用户、点名目标 app 与超时预算
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-windows-07`
//!
//! 判定面是真实 CLI 进程（构建自本仓）与真实 JSON-RPC 行协议。脚本化 daemon
//! 只替代桌面侧数据源（无图形会话时无法起真实合成器），CLI 的轮询、匹配、
//! 输出与退出码全部在生产路径上执行。构建阶段调用 `cargo build`，需要可写的
//! target 目录（尊重 `CARGO_TARGET_DIR`）；这是本用例唯一的环境依赖。

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use acceptance::workspace_root;
use agent_shell_rpc::method;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// 成功轮询返回的窗口条目（完整协议形状，CLI 据此反序列化）。
fn matching_entry() -> Value {
    json!([{
        "native_id": "ghost-1",
        "title": "Ghost Window",
        "app_id": "ghost",
        "pid": 4242,
        "x": 0,
        "y": 0,
        "width": 800,
        "height": 600,
        "workspace": null,
        "stacking_order": 1,
        "states": [],
        "window_type": "Normal",
        "keep_above": false
    }])
}

/// 脚本化 daemon：以行分隔 JSON-RPC 应答 `windows.list`。
struct FakeDaemon {
    socket: PathBuf,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn fake_daemon(windows: Vec<Value>) -> FakeDaemon {
    let dir = tempfile::tempdir().expect("socket temp dir");
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake daemon socket");
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let windows = windows.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(request) = serde_json::from_str::<Value>(&line) else {
                        break;
                    };
                    if request.get("method").and_then(Value::as_str) != Some(method::WINDOWS_LIST) {
                        continue;
                    }
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": request.get("id").cloned().unwrap_or(json!(0)),
                        "result": { "windows": windows, "from_cache": false },
                    });
                    let mut line = response.to_string();
                    line.push('\n');
                    if write.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = write.flush().await;
                }
            });
        }
    });
    FakeDaemon {
        socket,
        task,
        _dir: dir,
    }
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn target_dir(root: &std::path::Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
}

/// 构建 CLI 二进制并返回路径（增量构建；尊重 `CARGO_TARGET_DIR`）。
fn cli_binary() -> PathBuf {
    let root = workspace_root();
    let output = Command::new(cargo())
        .current_dir(&root)
        .args([
            "build",
            "-q",
            "-p",
            "agent-shell-cli",
            "--bin",
            "agent-shell",
        ])
        .output()
        .expect("run cargo build for the CLI binary");
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

async fn run_wait(
    binary: PathBuf,
    socket: PathBuf,
    sandbox: PathBuf,
    timeout: &'static str,
) -> (Output, Duration) {
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let output = Command::new(&binary)
            .args(["--output-format", "json", "--socket"])
            .arg(&socket)
            .args(["windows", "wait", "ghost", "--timeout", timeout])
            // 子进程沙箱：显式端点形态不派生 daemon，但仍隔离宿主状态/会话，
            // 避免 CLI 读取真实配置或写宿主状态目录。
            .env("HOME", sandbox.join("home"))
            .env("XDG_CONFIG_HOME", sandbox.join("config"))
            .env("XDG_STATE_HOME", sandbox.join("state"))
            .env("XDG_RUNTIME_DIR", sandbox.join("run"))
            .env("XDG_SESSION_TYPE", "tty")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DESKTOP_SESSION")
            .env_remove("XDG_CURRENT_DESKTOP")
            .env_remove("AGENT_SHELL_AGENT_ID")
            .env_remove("AGENT_SHELL_SOCKET")
            .output()
            .expect("run agent-shell windows wait");
        (output, started.elapsed())
    })
    .await
    .expect("join CLI wait run")
}

#[tokio::test]
async fn windows_wait_returns_the_window_or_times_out_within_budget() {
    let binary = tokio::task::spawn_blocking(cli_binary)
        .await
        .expect("join CLI build");

    // 成功路径：窗口出现即返回并打印该窗口。
    let present = fake_daemon(matching_entry().as_array().expect("entry array").clone());
    let sandbox = tempfile::tempdir().expect("CLI sandbox");
    for dir in ["home", "config", "state", "run"] {
        std::fs::create_dir_all(sandbox.path().join(dir)).expect("sandbox dir");
    }
    let (output, elapsed) = run_wait(
        binary.clone(),
        present.socket.clone(),
        sandbox.path().to_path_buf(),
        "300ms",
    )
    .await;
    assert!(
        output.status.success(),
        "wait must exit 0 when the window appears; stderr:\n{}\nstdout:\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "successful wait must return promptly, took {elapsed:?}"
    );
    let printed: Value = serde_json::from_slice(&output.stdout).expect("wait prints JSON windows");
    let entries = printed.as_array().expect("wait prints a window array");
    assert_eq!(
        entries.len(),
        1,
        "wait prints the first matching window only"
    );
    assert_eq!(entries[0]["id"], json!("ghost-1"));
    assert_eq!(entries[0]["title"], json!("Ghost Window"));
    assert_eq!(entries[0]["app_id"], json!("ghost"));

    // 超时路径：窗口始终缺席时按预算耗尽退出，stderr 点名目标与预算。
    let absent = fake_daemon(Vec::new());
    let (output, elapsed) = run_wait(
        binary,
        absent.socket.clone(),
        sandbox.path().to_path_buf(),
        "300ms",
    )
    .await;
    assert!(
        !output.status.success(),
        "wait must exit non-zero on timeout; stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "timeout is a plain command failure"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "300ms budget must be honoured well under 2s, took {elapsed:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ghost"),
        "timeout message must name the target app: {stderr}"
    );
    assert!(
        stderr.contains("did not appear within"),
        "timeout message must explain that the window never appeared: {stderr}"
    );
    assert!(
        stderr.contains("300ms"),
        "timeout message must report the configured budget: {stderr}"
    );
}
