//! TC-tech-09（AC-121、AC-111，P1）：随包用户服务单元的拉起形态。
//!
//! - 场景：按单元 ExecStart 拉起常驻服务；stdin 为空源；与未指定端点的客户端共存
//! - 判据：运行时目录出现可应答端点；应答后进程存活；端点再次可应答；未指定端点
//!   的客户端完成一次调用且不阻塞至 30s 锁窗口；该实例随后仍可应答
//! - 执行：`cargo test --no-default-features -p agent-shell-acceptance --test TC-tech-09`
//!
//! 用例不调用 `systemctl`，无 systemd 不构成跳过理由；判定面是真实二进制。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use acceptance::workspace_root;
use serde_json::Value;

/// daemon 装配（合成器/portal/a11y 探测）完成后才应答，留足冷启动余量。
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// 单次 RPC 往返上限。
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// 未指定端点的客户端上限：明显短于 C-102 的 30s 锁窗口，超时即说明它在等锁。
const CLIENT_DEADLINE: Duration = Duration::from_secs(25);

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

/// 构建产物目录：`CARGO_TARGET_DIR` 优先，相对值按工作区根解析（与 build 的 cwd 一致）。
fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                root.join(dir)
            }
        }
        None => root.join("target"),
    }
}

/// 构建真实 daemon 与 CLI 二进制（增量构建；跨包 bin 不为本测试目标产出）。
fn binaries() -> (PathBuf, PathBuf) {
    let root = workspace_root();
    let mut build = Command::new(cargo());
    build.current_dir(&root).args([
        "build",
        "--quiet",
        "--offline",
        "-p",
        "agent-shell-daemon",
        "--bin",
        "agent-shell-daemon",
        "-p",
        "agent-shell-cli",
        "--bin",
        "agent-shell",
    ]);
    // 无 PipeWire dev 头的环境（CI no-pipewire profile、arm64）：与验收包的
    // daemon 依赖同口径关闭 portal-screencast，只影响编译期特性。
    let has_pipewire_headers = Command::new("pkg-config")
        .args(["--exists", "libpipewire-0.3"])
        .status()
        .is_ok_and(|status| status.success());
    if !has_pipewire_headers {
        build.arg("--no-default-features");
    }
    let output = build
        .output()
        .expect("run cargo build for daemon and CLI binaries");
    assert!(
        output.status.success(),
        "building daemon/CLI must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let dir = target_dir(&root).join(profile);
    let daemon = dir.join("agent-shell-daemon");
    let cli = dir.join("agent-shell");
    assert!(
        daemon.is_file(),
        "expected daemon binary at {}",
        daemon.display()
    );
    assert!(cli.is_file(), "expected CLI binary at {}", cli.display());
    (daemon, cli)
}

/// 单元唯一 ExecStart 的参数（跳过 argv[0]），按用户单元的 `%t` 展开运行时目录。
///
/// 不展开行续接：单元若引入续接，这里显式失败而不是执行半条命令。
fn unit_launch_args(runtime_dir: &Path) -> Vec<String> {
    let unit = workspace_root().join("daemon/agent-shell-daemon.service");
    let text =
        std::fs::read_to_string(&unit).unwrap_or_else(|e| panic!("read {}: {e}", unit.display()));
    let mut lines = text
        .lines()
        .filter(|line| line.trim_start().starts_with("ExecStart="));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("{} 未声明 ExecStart", unit.display()));
    assert!(
        lines.next().is_none(),
        "{} 声明了多条 ExecStart",
        unit.display()
    );
    let command = line.trim_start().trim_start_matches("ExecStart=");
    assert!(
        !command.trim_end().ends_with('\\'),
        "用例不展开 ExecStart 行续接：{command}"
    );
    let runtime = runtime_dir.to_string_lossy();
    command
        .split_whitespace()
        .skip(1)
        .map(|token| token.replace("%t", runtime.as_ref()))
        .collect()
}

/// 单元启动命令声明的显式端点，必须是运行时目录内的路径。
fn declared_endpoint(args: &[String], runtime_dir: &Path) -> PathBuf {
    let value = args
        .iter()
        .enumerate()
        .find_map(|(index, arg)| match arg.as_str() {
            "--socket" => args.get(index + 1).cloned(),
            other => other.strip_prefix("--socket=").map(str::to_string),
        })
        .unwrap_or_else(|| panic!("单元启动命令未声明显式套接字端点：{args:?}"));
    let endpoint = PathBuf::from(value);
    assert_eq!(
        endpoint.parent(),
        Some(runtime_dir),
        "端点 {} 不在用户运行时目录 {} 内",
        endpoint.display(),
        runtime_dir.display()
    );
    endpoint
}

/// 无图形会话的临时用户环境：隔离 XDG 目录并抹掉宿主图形/端点/锁覆盖。
fn sandboxed(command: &mut Command, dir: &Path) {
    command
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("XDG_CURRENT_DESKTOP")
        .env_remove("AGENT_SHELL_SOCKET")
        .env_remove("AGENT_SHELL_LOCK")
        .env_remove("AGENT_SHELL_AGENT_ID")
        .env("XDG_SESSION_TYPE", "tty")
        .env("HOME", dir)
        .env("XDG_STATE_HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_RUNTIME_DIR", dir)
        .current_dir(dir);
}

/// 子进程守卫：用例中途 panic 也不留孤儿 daemon。
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stderr_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// 一次 `daemon.status` 往返；连接或应答失败返回描述性错误。
fn daemon_status(endpoint: &Path) -> Result<Value, String> {
    let stream = UnixStream::connect(endpoint)
        .map_err(|e| format!("connect {}: {e}", endpoint.display()))?;
    stream
        .set_read_timeout(Some(RPC_TIMEOUT))
        .map_err(|e| format!("set read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(RPC_TIMEOUT))
        .map_err(|e| format!("set write timeout: {e}"))?;
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"daemon.status\"}\n")
        .map_err(|e| format!("write request: {e}"))?;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;
    assert!(!line.is_empty(), "端点未应答即关闭：{}", endpoint.display());
    let message: Value = serde_json::from_str(&line)
        .map_err(|e| format!("malformed JSON-RPC line {line:?}: {e}"))?;
    message
        .get("result")
        .cloned()
        .ok_or_else(|| format!("daemon.status failed: {line}"))
}

/// 等端点就绪；子进程先退出时立刻失败并带上退出码与 stderr（而非空等超时）。
fn wait_until_serving(daemon: &mut Guard, endpoint: &Path, stderr: &Path) {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Some(status) = daemon.0.try_wait().expect("try_wait") {
            panic!(
                "daemon exited before serving {} ({status}); stderr:\n{}",
                endpoint.display(),
                stderr_text(stderr)
            );
        }
        match daemon_status(endpoint) {
            Ok(_) => return,
            Err(error) => last = error,
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "daemon did not serve daemon.status on {} within {READY_TIMEOUT:?}: {last}\nstderr:\n{}",
        endpoint.display(),
        stderr_text(stderr)
    );
}

#[test]
fn unit_exec_start_serves_runtime_endpoint_and_keeps_default_client_unblocked() {
    let (daemon_bin, cli_bin) = binaries();
    let sandbox = tempfile::tempdir().expect("tempdir");
    let runtime_dir = sandbox.path();

    let args = unit_launch_args(runtime_dir);
    let endpoint = declared_endpoint(&args, runtime_dir);
    let unit_stderr = runtime_dir.join("unit.stderr");

    let mut unit = Command::new(&daemon_bin);
    unit.args(&args)
        // 单元 StandardInput=null 等价：stdio 形态在这里读完 EOF 就退，常驻形态
        // 必须继续服务。
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&unit_stderr).expect("create unit stderr"),
        ));
    sandboxed(&mut unit, runtime_dir);
    let mut daemon = Guard(unit.spawn().expect("spawn daemon from unit ExecStart"));

    // (1) 端点出现在用户运行时目录，并可经该端点取得应答。
    wait_until_serving(&mut daemon, &endpoint, &unit_stderr);
    let file_type = std::fs::metadata(&endpoint)
        .unwrap_or_else(|e| panic!("endpoint {} metadata: {e}", endpoint.display()))
        .file_type();
    assert!(
        file_type.is_socket(),
        "运行时目录内的端点不是套接字：{}",
        endpoint.display()
    );

    // (2) 应答后进程仍在运行；同一端点可再次应答（stdin 空源不结束常驻形态）。
    assert!(
        daemon.0.try_wait().expect("try_wait").is_none(),
        "daemon 在取得应答后退出"
    );
    daemon_status(&endpoint).expect("同一端点必须可再次应答");

    // (3) 未指定端点的客户端完成一次调用，且不阻塞至 30s 锁等待窗口上限。
    let mut client = Command::new(&cli_bin);
    client.args(["daemon", "status"]);
    sandboxed(&mut client, runtime_dir);
    let started = Instant::now();
    let output = client.output().expect("run agent-shell daemon status");
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "未指定端点的客户端必须成功；stderr:\n{stderr}"
    );
    assert!(
        elapsed < CLIENT_DEADLINE,
        "客户端耗时 {elapsed:?}，已接近 C-102 的 30s 锁等待窗口"
    );
    let status: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("daemon status 输出不是 JSON（{e}）；stdout:\n{stdout}"));
    assert_eq!(
        status["running"],
        Value::Bool(true),
        "客户端未取得 daemon 应答；stdout:\n{stdout}"
    );

    // (4) 上一步之后，常驻实例仍存活且仍可应答。
    assert!(
        daemon.0.try_wait().expect("try_wait").is_none(),
        "常驻实例在未指定端点的客户端调用后退出"
    );
    daemon_status(&endpoint).expect("客户端调用后常驻实例仍须可应答");
}
