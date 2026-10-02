//! `--socket` 显式端点生命周期 e2e：真实 daemon 二进制、无图形会话。
//!
//! 绑定细节（权限、残留回收、活跃端点拒绝、非 socket 路径拒绝）由 `serve.rs` 单测
//! 覆盖；这里钉住只有完整进程才能观察的契约：空闲退出回收绑定路径，空闲退出与连接
//! 存续的关系，以及活跃端点上的二次绑定在进程层面的表现（退出码 + stderr）。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 每次探测的上限；daemon 装配（compositor/portal/a11y 探测）后才服务请求。
const READY_TIMEOUT: Duration = Duration::from_secs(60);

fn daemon_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-shell-daemon"))
}

/// 起一个显式端点 daemon（私有锁，不碰会话级锁；抹掉图形会话痕迹）。
fn spawn_daemon(dir: &Path, socket: &Path, lock: &Path, idle_secs: u64) -> Child {
    spawn_daemon_with_stdin(dir, socket, lock, idle_secs, Stdio::null())
}

/// [`spawn_daemon`] 的 stdin 可配置版本。
///
/// socket 形态的 stdin 不是协议通道，`Stdio::null()` 上 `poll(events=0)` 永远收不到
/// POLLHUP——用例需要以管道启动并主动挂断，才能覆盖「挂断不得终止 socket 形态」。
fn spawn_daemon_with_stdin(
    dir: &Path,
    socket: &Path,
    lock: &Path,
    idle_secs: u64,
    stdin: Stdio,
) -> Child {
    Command::new(daemon_bin())
        .arg("--socket")
        .arg(socket)
        .arg("--lock-path")
        .arg(lock)
        .arg("--idle-timeout-secs")
        .arg(idle_secs.to_string())
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("XDG_CURRENT_DESKTOP")
        .env("XDG_SESSION_TYPE", "tty")
        .current_dir(dir)
        .stdin(stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent-shell-daemon")
}

/// 一次行分隔 JSON-RPC 往返：`Ok(result_json)` 或错误描述。
fn request(socket: &Path, method: &str) -> Result<String, String> {
    let stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    rpc_on(stream, method)
}

fn rpc_on(stream: UnixStream, method: &str) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut stream = BufReader::new(stream);
    stream
        .get_mut()
        .write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{method}\"}}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    stream.read_line(&mut line).map_err(|e| e.to_string())?;
    if !line.contains("\"result\"") {
        return Err(format!("unexpected response: {line}"));
    }
    Ok(line)
}

/// 连接并保持打开，直到返回的 `UnixStream` 被 Drop。
fn open_connection(socket: &Path) -> UnixStream {
    let stream = UnixStream::connect(socket).expect("connect endpoint");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    stream
}

/// 在已连接的 socket 上发一条请求，返回 `result` 对象。
fn call(stream: UnixStream, method: &str) -> serde_json::Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{method}\"}}\n").as_bytes())
        .expect("write request");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let msg: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC line");
    msg.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{method} failed: {line}"))
}

/// `daemon.status` 上报的活跃订阅者数。
fn subscribers(socket: &Path) -> u64 {
    let stream = UnixStream::connect(socket).expect("connect endpoint");
    call(stream, "daemon.status")["subscribers"]
        .as_u64()
        .expect("subscribers in daemon.status")
}

/// 订阅事件并**保持连接**：返回 (subscriber_id, 持有连接的 reader)。
/// 连接随返回值的 Drop 断开——用来模拟「订阅客户端被杀」。
fn subscribe_keeping_open(socket: &Path) -> (String, BufReader<UnixStream>) {
    let mut reader = BufReader::new(open_connection(socket));
    reader
        .get_mut()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"events.subscribe\"}\n")
        .expect("write subscribe");
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read subscribe response");
    let msg: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC line");
    let id = msg["result"]["subscriber_id"]
        .as_str()
        .unwrap_or_else(|| panic!("subscribe failed: {line}"))
        .to_string();
    (id, reader)
}

fn wait_until_serving(socket: &Path) {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        match request(socket, "info.show") {
            Ok(_) => return,
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "daemon did not serve info.show on {} within {READY_TIMEOUT:?}: {last}",
        socket.display()
    );
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 空闲超时退出只在没有客户端连着时成立：长连接（订阅流）存续期间端点必须活着。
#[test]
fn idle_exit_waits_for_open_connections_to_finish() {
    const IDLE_SECS: u64 = 2;
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent-shell.sock");
    let lock = dir.path().join("agent-shell.lock");
    let mut child = spawn_daemon(dir.path(), &socket, &lock, IDLE_SECS);
    wait_until_serving(&socket);

    // 持有连接的客户端：超过空闲窗口后端点仍须可用（第二条连接照常服务）。
    let held = open_connection(&socket);
    std::thread::sleep(Duration::from_secs(IDLE_SECS * 2));
    assert!(
        request(&socket, "info.show").is_ok(),
        "endpoint must stay alive while a client is connected"
    );

    // 关掉长连接后，空闲窗口一到端点自行退出（clean exit，无残留）。
    // 用 `try_wait` 判定：轮询 socket 本身会刷新活动时钟，反而让端点不死。
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(IDLE_SECS * 4 + 5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "endpoint must idle-exit after the last connection closes"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(status.success(), "idle exit must be clean, got {status}");
    assert!(
        !socket.exists(),
        "clean idle exit must unlink the bound socket path"
    );
    kill(&mut child);
}

/// 订阅客户端被杀（Ctrl-C / SIGKILL）后必须回收：hub 订阅表项与转发任务都不再持有
/// （`daemon status.subscribers` 回落 0），且死订阅不得把端点续命——随后仍按空闲超时退出。
#[test]
fn dead_subscriber_is_reclaimed_and_does_not_keep_the_endpoint_alive() {
    const IDLE_SECS: u64 = 2;
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent-shell.sock");
    let lock = dir.path().join("agent-shell.lock");
    let mut child = spawn_daemon(dir.path(), &socket, &lock, IDLE_SECS);
    wait_until_serving(&socket);

    let (subscriber_id, subscriber) = subscribe_keeping_open(&socket);
    assert!(
        !subscriber_id.is_empty(),
        "subscribe must hand out a subscriber_id"
    );
    assert_eq!(subscribers(&socket), 1, "live subscriber must be visible");

    // 杀掉订阅客户端：丢弃连接（等价于进程被 Ctrl-C / SIGKILL）。
    drop(subscriber);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = subscribers(&socket);
        if count == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "dead subscriber leaked: subscribers still reports {count}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // 死订阅不得续命端点：轮询本身会刷新活动时钟，退出判定用 try_wait。
    let deadline = Instant::now() + Duration::from_secs(IDLE_SECS * 4 + 5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "endpoint must idle-exit once no client is connected"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(status.success(), "idle exit must be clean, got {status}");
    kill(&mut child);
}

/// 活跃端点上的二次绑定：进程退出非零、stderr 说明端点已被占用，原 daemon 继续服务。
#[test]
fn second_daemon_on_live_endpoint_fails_without_disturbing_the_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent-shell.sock");
    let mut first = spawn_daemon(dir.path(), &socket, &dir.path().join("first.lock"), 60);
    wait_until_serving(&socket);

    let out = Command::new(daemon_bin())
        .arg("--socket")
        .arg(&socket)
        .arg("--lock-path")
        .arg(dir.path().join("second.lock"))
        .arg("--idle-timeout-secs")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run second daemon");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "second daemon must fail on a live endpoint"
    );
    assert!(
        stderr.contains("already listening") && stderr.contains(&socket.display().to_string()),
        "stderr must name the live endpoint: {stderr}"
    );
    assert!(
        request(&socket, "info.show").is_ok(),
        "first daemon must keep serving after the refused second bind"
    );
    kill(&mut first);
}

/// socket 形态的 stdin 不是协议通道：写端关闭（内核置 POLLHUP）不得终止 daemon——
/// 端点必须继续服务请求，并按空闲超时自行退出。
///
/// 看门狗只在 stdio 形态安装（`main.rs` 的 `args.socket.is_none()` gating）。既有
/// socket 用例全部以 `Stdio::null()` 启动，误装看门狗也拦不住；本用例以 piped stdin
/// 启动并主动挂断，正是该 gating 的回归保护。
#[test]
fn stdin_hangup_does_not_stop_socket_mode_daemon() {
    const IDLE_SECS: u64 = 2;
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent-shell.sock");
    let lock = dir.path().join("agent-shell.lock");
    let mut child = spawn_daemon_with_stdin(dir.path(), &socket, &lock, IDLE_SECS, Stdio::piped());
    wait_until_serving(&socket);

    // 客户端挂断：关闭 stdin 写端。
    drop(child.stdin.take());
    // 端点必须继续服务——把挂断当作瞬态客户端消亡就会 exit(0)，后续请求失败。
    assert!(
        request(&socket, "info.show").is_ok(),
        "socket daemon must keep serving after stdin hangup"
    );
    assert!(
        request(&socket, "info.show").is_ok(),
        "socket daemon must keep serving on further requests after stdin hangup"
    );

    // 收尾：挂断不改变空闲退出语义——超时后进程自行正常退出，不留残留。
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "clean idle exit expected, got {status:?}");
                break;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                kill(&mut child);
                panic!("daemon must idle-exit after stdin hangup, not stay resident");
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}
