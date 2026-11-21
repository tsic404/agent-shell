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

use serde_json::Value;

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
    let mut build = Command::new(&cargo);
    build.current_dir(&root).args([
        "build",
        "--quiet",
        "--offline",
        "-p",
        "agent-shell-daemon",
        "--bin",
        "agent-shell-daemon",
    ]);
    // 无 PipeWire dev 头的机器（CI 的 no-pipewire profile、arm64 发行构建）：
    // `portal-screencast` 编译期排除才能产出 daemon（与 tests/acceptance 依赖
    // daemon 时的 default-features=false 同口径）。有 dev 头的环境保持默认特性。
    let pipewire_dev = Command::new("pkg-config")
        .args(["--exists", "libpipewire-0.3"])
        .status()
        .is_ok_and(|status| status.success());
    if !pipewire_dev {
        build.arg("--no-default-features");
    }
    let status = build
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
        Self::start_with_state_dir(dir, None)
    }

    /// 带独立状态目录启动：审计日志落 `state_dir/agent-shell/audit.jsonl`，
    /// 用例可以按住那把文件锁观察真实争用。
    fn start_with_state_dir(dir: &Path, state_dir: Option<&Path>) -> Self {
        let socket = dir.join("agent-shell.sock");
        let lock = dir.join("agent-shell.lock");
        let mut cmd = Command::new(daemon_binary());
        cmd.arg("--socket")
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
            .env("XDG_SESSION_TYPE", "tty");
        if let Some(state_dir) = state_dir {
            cmd.env("XDG_STATE_HOME", state_dir);
        }
        let child = cmd
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

    /// 本 daemon 的审计锚点路径（`XDG_STATE_HOME` 已指向独立目录时有效）。
    /// 记录实际写在锚点旁的进程分段里，锚点本身可能不存在。
    fn audit_path(state_dir: &Path) -> PathBuf {
        state_dir.join("agent-shell").join("audit.jsonl")
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

/// `info --output-format json` 的输出必须是 daemon 结构化回执：backend 与同一
/// daemon 的 doctor 合成器行逐字一致，CLI 不得自带后端名。
fn assert_info_json(daemon: &SocketDaemon, stdout: &str) {
    let v: serde_json::Value =
        serde_json::from_str(stdout).expect("info --output-format json must be JSON");
    let backend = v["backend"].as_str().unwrap_or_default();
    let doctor =
        rpc_result_over_socket(&daemon.socket, "doctor.run", "null").expect("raw doctor.run");
    let row = doctor["lines"]
        .as_array()
        .expect("doctor lines")
        .iter()
        .filter_map(|line| line.as_str())
        .find(|line| !line.starts_with(' ') && line.contains("合成器"))
        .expect("doctor compositor row");
    let expected = row
        .split_once(':')
        .unwrap_or_else(|| panic!("compositor row keeps `<label>: <value>`: {row}"))
        .1
        .trim();
    assert_eq!(
        backend, expected,
        "info backend must equal the daemon-reported compositor (stdout: {stdout})"
    );
    assert!(
        !backend.is_empty(),
        "info must report the daemon compositor, got empty backend"
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
    assert_info_json(&daemon, &stdout);
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
    assert_info_json(&daemon, &stdout);
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

/// 未指定端点：运行时目录里的随包常驻端点可应答时，入口直接接入它，不派生
/// 第二个服务实例（AC-121 / ADR-017）。判定面用常驻实例的进程内状态：回执带
/// 着预先写入的 IME 引擎名，新派生的瞬态实例不可能答出它。
#[test]
fn cli_default_entry_uses_resident_endpoint_without_spawning_second_instance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut daemon = SocketDaemon::start(dir.path());
    rpc_result_over_socket(
        &daemon.socket,
        "ime.engine.set",
        r#"{"engine":"resident-marker"}"#,
    )
    .expect("seed resident-only state");
    let before = std::fs::metadata(&daemon.socket).expect("socket metadata");

    let started = Instant::now();
    let out = Command::new(cli_binary())
        .env("XDG_RUNTIME_DIR", dir.path())
        .env_remove("AGENT_SHELL_SOCKET")
        .env_remove("AGENT_SHELL_LOCK")
        .args(["daemon", "status"])
        .output()
        .expect("run agent-shell daemon status");
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "default entry must reach the resident endpoint (stderr: {stderr})"
    );
    let status: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("daemon status is not JSON ({e}): {stdout}"));
    assert_eq!(
        status["ime_engine"],
        Value::String("resident-marker".into()),
        "reply must come from the resident instance, not a freshly spawned one: {stdout}"
    );
    assert!(
        elapsed < CLI_DEADLINE,
        "default entry must not wait on a transient instance: {elapsed:?}"
    );
    let after = std::fs::metadata(&daemon.socket).expect("socket metadata after call");
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        (before.dev(), before.ino()),
        (after.dev(), after.ino()),
        "resident endpoint must not be rebound by a second instance"
    );
    assert!(
        daemon.child.try_wait().expect("try_wait").is_none(),
        "resident instance must still be running"
    );
}

/// 审计文件被另一个写者持锁时，常驻端点仍须被判定为可应答、默认入口不得
/// 派生瞬态实例。
///
/// 审计落在 `daemon.ping` 的应答路径上。锁等待若无上限，另一实例（哪怕只是
/// 正常读 `security.audit` 的查询）短暂持锁就会让常驻实例错过 CLI 的 500 ms
/// 探测窗口，被误判为不可应答——默认入口因此漂移成「每次派生新实例」。
#[test]
fn default_entry_reuses_resident_endpoint_while_audit_lock_is_held() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let daemon = SocketDaemon::start_with_state_dir(dir.path(), Some(&state));
    rpc_result_over_socket(
        &daemon.socket,
        "ime.engine.set",
        r#"{"engine":"resident-marker"}"#,
    )
    .expect("seed resident-only state");
    let audit = SocketDaemon::audit_path(&state);
    // 记录写在 daemon 自己的进程分段里，锚点文件不再由 daemon 创建：这里显式
    // 建一把空的主日志当锁对象，模拟旧版本遗留的同名文件被外部写者独占。
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit)
        .expect("create audit anchor");

    // 另一个进程持排他锁：`flock(1)` 阻塞到用例放行为止。
    let mut holder = Command::new("flock")
        .arg("-x")
        .arg(&audit)
        .arg("sh")
        .arg("-c")
        .arg("read _line")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn flock holder");
    // 给 flock 一点时间真正拿到锁。
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        holder.try_wait().expect("try_wait holder").is_none(),
        "flock holder must still be holding"
    );

    let started = Instant::now();
    let out = Command::new(cli_binary())
        .env("XDG_RUNTIME_DIR", dir.path())
        .env_remove("AGENT_SHELL_SOCKET")
        .env_remove("AGENT_SHELL_LOCK")
        .args(["daemon", "status"])
        .output()
        .expect("run agent-shell daemon status");
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "default entry must answer while the audit lock is held (stderr: {stderr})"
    );
    let status: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("daemon status is not JSON ({e}): {stdout}"));
    assert_eq!(
        status["ime_engine"],
        Value::String("resident-marker".into()),
        "reply must come from the resident instance, not a freshly spawned one: {stdout}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "audit lock wait must stay inside the probe budget: {elapsed:?}"
    );

    // 放锁后该条审计仍要可查：不丢记录（C-107）。
    drop(holder.stdin.take());
    let _ = holder.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut raw = String::new();
    while Instant::now() < deadline {
        raw = audit_text(&audit);
        if raw.contains("daemon.ping") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        raw.contains("daemon.ping"),
        "queued audit record must land after the lock is released: {raw}"
    );
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("unparsable audit line {line:?}: {e}"));
    }
}

/// 某个审计锚点对应的全部可读记录（主日志 + 各进程分段）拼接文本。
fn audit_text(anchor: &Path) -> String {
    let mut text = String::new();
    for file in agent_shell_core::audit::audit_files(anchor) {
        text.push_str(&std::fs::read_to_string(file).unwrap_or_default());
    }
    text
}

/// 同 uid 监听但不应答的常驻端点：默认入口必须判为不可应答并派生瞬态。
/// 只「连得上」就复用，会把业务请求发给一个从不读写的进程，命令被拖到
/// 整条预算耗尽（ADR-017 的「可应答时接入」）。
#[test]
fn default_entry_falls_back_when_resident_listener_never_answers() {
    let _ = daemon_binary(); // 瞬态回退需要真实的 daemon 二进制
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent-shell.sock");
    let listener =
        std::os::unix::net::UnixListener::bind(&socket).expect("bind silent resident endpoint");
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let silent = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            accepted_tx.send(()).ok();
            // 接住连接但既不读也不写，直到用例放行。
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
            drop(stream);
        }
    });

    let started = Instant::now();
    let out = Command::new(cli_binary())
        .env("XDG_RUNTIME_DIR", dir.path())
        .env_remove("AGENT_SHELL_SOCKET")
        .env_remove("AGENT_SHELL_LOCK")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("XDG_CURRENT_DESKTOP")
        .env("XDG_SESSION_TYPE", "tty")
        .args(["daemon", "status"])
        .output()
        .expect("run agent-shell daemon status");
    let elapsed = started.elapsed();
    accepted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("default entry must probe the resident endpoint before falling back");
    release_tx.send(()).ok();
    silent.join().expect("silent listener thread");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "silent resident listener must fall back to a transient daemon (stderr: {stderr})"
    );
    assert!(
        elapsed < CLI_DEADLINE,
        "readiness probe must be bounded: {elapsed:?}"
    );
    let status: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("daemon status is not JSON ({e}): {stdout}"));
    assert_eq!(
        status["running"],
        Value::Bool(true),
        "reply must come from the transient daemon: {stdout}"
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
    assert_info_json(&daemon, &stdout);
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

/// 原始 socket 往返：发任意方法与参数，返回响应的 `result`（透传对照用）。
///
/// 端点是行分隔 JSON-RPC：请求必须带换行，否则 daemon 的逐行读取永远凑不齐
/// 一行，连接上不会有任何回执。
fn rpc_result_over_socket(path: &Path, method: &str, params: &str) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    let req = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#) + "\n";
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write {method}: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read {method}: {e}"))?;
    let v: Value = serde_json::from_str(&line).map_err(|e| format!("bad response: {e}: {line}"))?;
    v.get("result")
        .cloned()
        .ok_or_else(|| format!("response without result: {line}"))
}

/// 未接线写侧占位回执可见性回归。
///
/// 9 个未接线写侧子命令（file trash / file open-directory / mime set /
/// bluetooth connect / bluetooth disconnect / touchpad on / touchpad off /
/// touchpad natural-scroll / kbd layout set）
/// 得到 daemon 的 not_implemented 回执时必须把回执打印到 stdout——旧行为
/// 只 `call().await?` 吞掉返回值，rc=0 且 stdout 无载荷，消费方无法以
/// 状态字段判定「未实现」（AC-105 判据）。
///
/// 正向钉住：每个命令 rc=0 且 stdout 载荷 `status=not_implemented`、
/// `service` 与方法名一一对应。
/// 反证探针：同一请求经原始 socket 直调 daemon 的响应 `result` 与 CLI
/// 打印的载荷逐字段相等——可见性来自 daemon 真实回执的透传，而非 CLI
/// 硬编码或丢弃后伪造；若未来接线，status 不再是 not_implemented，
/// 本用例随 `PLACEHOLDER_SERVICES` 摘除同步更新。
#[test]
fn placeholder_receipts_visible_for_unwired_write_commands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = SocketDaemon::start(dir.path());

    // (CLI 参数, 方法名, 直调载荷) —— 方法与载荷与 `cli/src/main.rs` 实发同形
    // （file/mime/kbd 均为位置参数子命令）。
    let cases: &[(&[&str], &str, &str)] = &[
        (
            &["file", "trash", "/tmp/probe"],
            "file.trash",
            r#"{"path":"/tmp/probe"}"#,
        ),
        (
            &["file", "open-directory", "/tmp/probe"],
            "file.open_directory",
            r#"{"path":"/tmp/probe"}"#,
        ),
        (
            &["mime", "set", "text/plain", "probe.desktop"],
            "mime.set",
            r#"{"mime":"text/plain","app":"probe.desktop"}"#,
        ),
        (
            &["bluetooth", "connect", "AA:BB:CC:DD:EE:FF"],
            "bluetooth.connect",
            r#"{"address":"AA:BB:CC:DD:EE:FF"}"#,
        ),
        (
            &["bluetooth", "disconnect", "AA:BB:CC:DD:EE:FF"],
            "bluetooth.disconnect",
            r#"{"address":"AA:BB:CC:DD:EE:FF"}"#,
        ),
        (&["touchpad", "on"], "touchpad.set", r#"{"enabled":true}"#),
        (&["touchpad", "off"], "touchpad.set", r#"{"enabled":false}"#),
        (
            &["touchpad", "natural-scroll", "on"],
            "touchpad.set",
            r#"{"natural_scroll":true}"#,
        ),
        (
            &["kbd", "layout", "set", "us"],
            "kbd.layout.set",
            r#"{"layout":"us"}"#,
        ),
    ];

    for (args, service, params) in cases {
        let (out, _) = run_cli(&daemon, args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{args:?} must exit 0 against the unwired daemon; stderr: {stderr}"
        );
        let v: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!("{args:?} must print a receipt JSON on stdout: {e}\nstdout: {stdout}")
        });
        assert_eq!(
            v["status"], "not_implemented",
            "{args:?} receipt must carry status=not_implemented; stdout: {stdout}"
        );
        assert_eq!(
            v["service"], *service,
            "{args:?} receipt must name the method; stdout: {stdout}"
        );

        // 反证探针：CLI 打印的载荷必须与 daemon 对同请求的原生回执逐字段相等。
        let direct = rpc_result_over_socket(&daemon.socket, service, params)
            .unwrap_or_else(|e| panic!("direct rpc for {service} failed: {e}"));
        assert_eq!(
            direct, v,
            "CLI printed receipt must equal the daemon result verbatim for {service}"
        );
    }
}
