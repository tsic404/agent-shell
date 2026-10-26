//! 随包 systemd 单元的拉起形态 e2e（真实 daemon 二进制、无图形会话）。
//!
//! 单元曾只传 `--idle-timeout-secs`：systemd 给的 stdin 是 `/dev/null`，daemon
//! 落入 stdio 形态读到 EOF 即以 0 退出，`Restart=on-failure` 又不重启正常退出，
//! 常驻服务从未起来。用例按单元原文的 ExecStart 启动真实二进制，钉住只有进程
//! 层面能观察的后果：stdin 关闭后仍在服务、端点落在运行时目录的
//! `agent-shell.sock`、会话级单实例锁未被常驻实例占用。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// daemon 装配（compositor/portal/a11y 探测）完成才应答，留足余量。
const READY_TIMEOUT: Duration = Duration::from_secs(60);

fn daemon_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-shell-daemon"))
}

/// 单元文件随 daemon crate 分发，与安装路径无关（打包脚本只替换 argv[0]）。
fn unit_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("agent-shell-daemon.service")
}

/// 单元唯一 ExecStart 的参数（跳过 argv[0]），按用户单元的 `%t` 展开运行时目录。
///
/// 不处理行续接：单元改动若引入续接，这里显式失败而不是猜出半条命令。
fn unit_exec_start_args(runtime_dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(unit_path()).expect("read unit file");
    let mut lines = text
        .lines()
        .filter(|l| l.trim_start().starts_with("ExecStart="));
    let line = lines.next().expect("unit must define ExecStart");
    assert!(
        lines.next().is_none(),
        "unit must define exactly one ExecStart"
    );
    let command = line.trim_start().trim_start_matches("ExecStart=");
    assert!(
        !command.trim_end().ends_with('\\'),
        "test does not expand ExecStart line continuations: {command}"
    );
    command
        .split_whitespace()
        .skip(1)
        .map(|token| token.replace("%t", &runtime_dir.to_string_lossy()))
        .collect()
}

/// 子进程守卫：用例中途 panic 也不留孤儿 daemon。
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 无图形会话的沙箱环境（临时用户目录，抹掉图形会话痕迹与外部锁路径覆盖）。
fn sandboxed_daemon(dir: &Path) -> Command {
    let mut cmd = Command::new(daemon_bin());
    cmd.env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("XDG_CURRENT_DESKTOP")
        .env_remove("AGENT_SHELL_LOCK")
        .env("XDG_SESSION_TYPE", "tty")
        .env("HOME", dir)
        .env("XDG_STATE_HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_RUNTIME_DIR", dir)
        .current_dir(dir);
    cmd
}

/// stderr 落临时文件：等端点超时时把 daemon 自述的失败原因带回断言消息。
fn stderr_file(path: &Path) -> Stdio {
    Stdio::from(std::fs::File::create(path).expect("create stderr file"))
}

fn spawn_guarded(cmd: &mut Command) -> Guard {
    Guard(cmd.spawn().expect("spawn daemon"))
}

/// 一次 `daemon.status` 往返；端点未起或未就绪即描述性错误。
fn daemon_status(socket: &Path) -> Result<serde_json::Value, String> {
    let stream = UnixStream::connect(socket).map_err(|e| format!("connect: {e}"))?;
    let timeout = Some(Duration::from_secs(10));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| format!("set read timeout: {e}"))?;
    let mut reader = BufReader::new(stream);
    let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"daemon.status\"}\n";
    reader
        .get_mut()
        .write_all(request)
        .map_err(|e| format!("write request: {e}"))?;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;
    let msg: serde_json::Value = serde_json::from_str(&line)
        .map_err(|e| format!("malformed JSON-RPC line {line:?}: {e}"))?;
    msg.get("result")
        .cloned()
        .ok_or_else(|| format!("daemon.status failed: {line}"))
}

/// 等端点就绪；子进程先退出时立刻失败并带上退出码与 stderr（而非空等超时）。
fn wait_until_serving(guard: &mut Guard, socket: &Path, stderr: &Path) {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            panic!(
                "daemon exited before serving {} ({status}); stderr:\n{}",
                socket.display(),
                std::fs::read_to_string(stderr).unwrap_or_default()
            );
        }
        match daemon_status(socket) {
            Ok(status) if status["running"] == serde_json::Value::Bool(true) => return,
            Ok(status) => last = format!("daemon.status={status}"),
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "daemon did not serve daemon.status on {} within {READY_TIMEOUT:?}: {last}\nstderr:\n{}",
        socket.display(),
        std::fs::read_to_string(stderr).unwrap_or_default()
    );
}

#[test]
fn unit_exec_start_keeps_the_daemon_resident_under_systemd_stdin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let unit_socket = dir.path().join("agent-shell.sock");
    let args = unit_exec_start_args(dir.path());

    // 端点路径是操作方传给 CLI --socket/AGENT_SHELL_SOCKET 的地址，用例与文档
    // 必须指向同一文件。
    let socket_arg = args
        .iter()
        .position(|a| a == "--socket")
        .unwrap_or_else(|| panic!("unit must launch the explicit socket form: {args:?}"));
    assert_eq!(
        args.get(socket_arg + 1).map(String::as_str),
        unit_socket.to_str(),
        "unit endpoint must be the documented runtime-dir socket: {args:?}"
    );

    let unit_stderr = dir.path().join("unit.stderr");
    let mut unit_cmd = sandboxed_daemon(dir.path());
    unit_cmd
        .args(&args)
        // systemd 单元的 StandardInput=null 等价：stdio 形态在这里读完 EOF
        // 就退，常驻形态必须继续服务。
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_file(&unit_stderr));
    let mut unit = spawn_guarded(&mut unit_cmd);
    wait_until_serving(&mut unit, &unit_socket, &unit_stderr);
    assert!(
        unit.0.try_wait().expect("try_wait").is_none(),
        "unit daemon must stay resident after serving a request"
    );

    // 常驻实例不得占住会话级单实例锁：不带端点的 CLI 派生瞬态 daemon，其锁解析
    // 在沙箱抹掉 AGENT_SHELL_LOCK 后落到默认会话锁，被占用时每条命令都要等满锁窗口。
    let transient_socket = dir.path().join("transient.sock");
    let transient_stderr = dir.path().join("transient.stderr");
    let mut transient_cmd = sandboxed_daemon(dir.path());
    transient_cmd
        .arg("--socket")
        .arg(&transient_socket)
        .arg("--idle-timeout-secs")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_file(&transient_stderr));
    let mut transient = spawn_guarded(&mut transient_cmd);
    wait_until_serving(&mut transient, &transient_socket, &transient_stderr);
}
