//! `--socket` / `AGENT_SHELL_SOCKET` 显式端点 e2e（§22.2 激活策略）。
//!
//! CLI 直连已运行的 daemon（unix socket），不 spawn 瞬态子进程、不受会话单实例
//! 锁约束——这正是无图形会话（QA/调试）里验证 CLI 的路径。用例启动真实
//! `agent-shell-daemon` 与真实 `agent-shell` 二进制，端到端钉住两侧契约。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// daemon 装配（compositor/portal/a11y 探测）后才服务请求，等待窗口覆盖冷启动。
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// CLI 单命令上限：超过即说明它在等某个 30s 锁窗口，而非走端点。
const CLI_DEADLINE: Duration = Duration::from_secs(20);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli/ 上一级是 workspace 根")
        .to_path_buf()
}

/// 构建产物根目录：`CARGO_TARGET_DIR` 优先（共享/独立 target 目录是常见开发配置），
/// 相对值按 workspace 根解析——与下面 `cargo build` 的 cwd 一致。
fn target_dir() -> PathBuf {
    resolve_target_dir(std::env::var_os("CARGO_TARGET_DIR"))
}

/// [`target_dir`] 的纯函数形态（env 取值显式传入，便于单测）。
fn resolve_target_dir(configured: Option<std::ffi::OsString>) -> PathBuf {
    match configured {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                workspace_root().join(dir)
            }
        }
        None => workspace_root().join("target"),
    }
}

/// daemon 二进制路径（`<target>/debug`）。
///
/// `cargo test` 只为**被测包**的 bin 产出 `<target>/debug/<name>`；daemon 属于另一个
/// 包，`--all-targets` 下它的 bin 只编成 test harness。集成测试因此自己补一次
/// `cargo build`（Cargo 的指纹判定保证二进制与当前工作树一致），再启动真实二进制。
/// 构建在进程内只做一次，并行用例不重复调用 cargo。
///
/// `--offline`：能跑起本测试说明外层 cargo 已解析并构建过全部依赖，无需再访问
/// registry；省掉索引往返（无 crates.io 出口的机器上这一步要卡数十秒）。
/// CI 的 Test job 会先构建同一二进制，这里的调用退化成一次快速指纹检查。
static DAEMON_BINARY: LazyLock<PathBuf> = LazyLock::new(|| {
    let root = workspace_root();
    // `CARGO` 由驱动本次测试的 cargo 在运行时注入：用它而不是 PATH，避免命中
    // 另一个工具链的 cargo；直接执行测试二进制时退回 PATH。
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(&cargo)
        .current_dir(&root)
        .args([
            "build",
            "--quiet",
            "--offline",
            "-p",
            "agent-shell-daemon",
            "--bin",
            "agent-shell-daemon",
        ])
        .status()
        .unwrap_or_else(|e| panic!("cannot run `{cargo} build` in {}: {e}", root.display()));
    assert!(
        status.success(),
        "`{cargo} build -p agent-shell-daemon --bin agent-shell-daemon` failed: {status} \
         (re-run it in {} for the full error)",
        root.display()
    );
    let path = target_dir().join("debug/agent-shell-daemon");
    assert!(
        path.is_file(),
        "{} missing right after a successful build (CARGO_TARGET_DIR={:?})",
        path.display(),
        std::env::var("CARGO_TARGET_DIR")
    );
    path
});

fn daemon_binary() -> &'static Path {
    &DAEMON_BINARY
}

fn cli_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-shell"))
}

/// 显式端点 daemon：`--socket` 监听 + `--lock-path` 私有锁（不碰会话级锁）。
struct SocketDaemon {
    child: Child,
    socket: PathBuf,
    lock: PathBuf,
}

impl SocketDaemon {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("agent-shell.sock");
        let lock = dir.join("agent-shell.lock");
        let child = Command::new(daemon_binary())
            .arg("--socket")
            .arg(&socket)
            .arg("--lock-path")
            .arg(&lock)
            .arg("--idle-timeout-secs")
            .arg("120")
            // 抹掉宿主图形会话痕迹：用例复现的正是「非图形会话 CLI 验证」场景，
            // 同时避免测试 daemon 接入真实合成器/portal。
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("XDG_CURRENT_DESKTOP")
            .env("XDG_SESSION_TYPE", "tty")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn agent-shell-daemon");
        let daemon = Self {
            child,
            socket,
            lock,
        };
        daemon.wait_until_serving();
        daemon
    }

    /// socket 文件先于 daemon 装配出现，就绪判定必须走真实 RPC 往返。
    fn wait_until_serving(&self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last = String::new();
        while Instant::now() < deadline {
            match info_over_socket(&self.socket) {
                Ok(_) => return,
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "daemon did not serve info on {} within {READY_TIMEOUT:?}: {last}",
            self.socket.display()
        );
    }
}

impl Drop for SocketDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 原始 socket 往返：端点确实以「行分隔 JSON-RPC 2.0」对外服务。
fn info_over_socket(path: &Path) -> Result<String, String> {
    let mut stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"info.show\"}\n")
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    if !line.contains("\"result\"") {
        return Err(format!("unexpected response: {line}"));
    }
    Ok(line)
}

/// `info --output-format json` 的输出必须是 daemon 结构化回执（含 backend 字段）。
fn assert_info_json(stdout: &str) {
    let v: serde_json::Value =
        serde_json::from_str(stdout).expect("info --output-format json must be JSON");
    let backend = v["backend"].as_str().unwrap_or_default();
    assert!(
        backend.contains("via daemon"),
        "info must come from daemon assembly, got backend={backend:?} (stdout: {stdout})"
    );
}

fn run_cli(daemon: &SocketDaemon, args: &[&str]) -> (std::process::Output, Duration) {
    let started = Instant::now();
    let mut cmd = Command::new(cli_binary());
    cmd.arg("--socket").arg(&daemon.socket);
    cmd.args(args);
    let out = cmd.output().expect("run agent-shell");
    (out, started.elapsed())
}

/// 端点 daemon 持锁期间 CLI 仍可用：若 CLI 退回 spawn，其子进程会继承
/// `AGENT_SHELL_LOCK` 并卡在同一个锁上 30s 后失败。
#[test]
fn cli_uses_socket_endpoint_while_session_lock_is_held() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = SocketDaemon::start(dir.path());

    let started = Instant::now();
    let out = Command::new(cli_binary())
        .env("AGENT_SHELL_LOCK", &daemon.lock)
        .arg("--socket")
        .arg(&daemon.socket)
        .arg("info")
        .arg("--output-format")
        .arg("json")
        .output()
        .expect("run agent-shell info");
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "info over --socket must exit 0 (stderr: {stderr})"
    );
    assert_info_json(&stdout);
    assert!(
        elapsed < CLI_DEADLINE,
        "CLI must not wait on the held lock; took {elapsed:?}"
    );
}

/// `AGENT_SHELL_SOCKET` 与 `--socket` 等价（无 flag 时的端点来源）。
#[test]
fn cli_honours_agent_shell_socket_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = SocketDaemon::start(dir.path());

    let out = Command::new(cli_binary())
        .env("AGENT_SHELL_SOCKET", &daemon.socket)
        .arg("info")
        .arg("--output-format")
        .arg("json")
        .output()
        .expect("run agent-shell info");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "AGENT_SHELL_SOCKET must select the endpoint (stderr: {stderr})"
    );
    assert_info_json(&stdout);
}

/// 端点不可达时明确报错，不得静默退回 spawn（用户点了具体端点）。
#[test]
fn cli_reports_unreachable_endpoint_without_falling_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing.sock");

    let out = Command::new(cli_binary())
        .arg("--socket")
        .arg(&missing)
        .arg("info")
        .output()
        .expect("run agent-shell info");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must fail, not fall back to spawn");
    assert!(
        stderr.contains("cannot connect to daemon socket")
            && stderr.contains(&missing.display().to_string()),
        "error must name the unreachable endpoint: {stderr}"
    );
}

/// 一条长连接（events subscribe）占用期间，第二条连接照常被服务。
#[test]
fn socket_endpoint_serves_concurrent_connections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = SocketDaemon::start(dir.path());

    let mut stream = Command::new(cli_binary())
        .arg("--socket")
        .arg(&daemon.socket)
        .arg("events")
        .arg("subscribe")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn events subscribe");
    std::thread::sleep(Duration::from_millis(700));

    let (out, elapsed) = run_cli(&daemon, &["info", "--output-format", "json"]);
    let _ = stream.kill();
    let _ = stream.wait();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "second connection must be served while a stream connection is open (stderr: {stderr})"
    );
    assert_info_json(&stdout);
    assert!(
        elapsed < CLI_DEADLINE,
        "second connection blocked: {elapsed:?}"
    );
}

/// 产物目录解析：未设置走 `<workspace>/target`；绝对 `CARGO_TARGET_DIR` 原样使用；
/// 相对值按 workspace 根解析（与 `cargo build` 的 cwd 一致）。
#[test]
fn target_dir_resolution_honours_cargo_target_dir() {
    let root = workspace_root();
    assert_eq!(resolve_target_dir(None), root.join("target"));
    assert_eq!(
        resolve_target_dir(Some("/shared/cargo-target".into())),
        PathBuf::from("/shared/cargo-target")
    );
    assert_eq!(
        resolve_target_dir(Some("build-target".into())),
        root.join("build-target")
    );
}
