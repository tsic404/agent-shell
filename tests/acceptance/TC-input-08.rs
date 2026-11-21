//! TC-input-08（AC-122，P1）：无可应答常驻端点时回退瞬态服务的授权未完成路径。
//!
//! - 场景：运行时目录内没有常驻端点，默认入口派生瞬态服务并触发 portal 授权
//! - 判据：输入命令在自身预算内返回需授权结果；命令运行期间授权界面在；
//!   命令退出后授权界面撤下；下一次调用重新发起并再次出现授权界面
//! - 执行：`cargo test --no-default-features -p agent-shell-acceptance --test TC-input-08`
//!
//! 判定面是真实 CLI + 真实 daemon 进程 + 私有 session bus 上的 RemoteDesktop
//! mock。瞬态形态不显式 `Close`：会话属主进程退出后由 portal 后端撤销会话，
//! mock 以 `NameOwnerChanged` 复刻同一可观察事实。

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use acceptance::{serve_remote_desktop, workspace_root, PrivateBus, RemoteDesktopMock};

/// 命令预算（C-154 默认 5s）+ 进程启动与 daemon 装配余量。
const COMMAND_DEADLINE: Duration = Duration::from_secs(8);

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

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

/// 构建真实 daemon 与 CLI 二进制（与 TC-tech-09 同口径的增量构建）。
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
    assert!(daemon.is_file(), "expected {}", daemon.display());
    assert!(cli.is_file(), "expected {}", cli.display());
    (daemon, cli)
}

/// 无常驻端点的默认入口环境：私有运行时目录 + mock portal 所在的 session bus。
fn transient_cli(cli: &Path, dir: &Path, bus: &PrivateBus, args: &[&str]) -> Command {
    let mut command = Command::new(cli);
    command
        .args(args)
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_RUNTIME_DIR", dir)
        .env("XDG_CURRENT_DESKTOP", "GNOME")
        .env("XDG_SESSION_TYPE", "wayland")
        .env("HOME", dir)
        .env("XDG_STATE_HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("AGENT_SHELL_SOCKET")
        .env_remove("AGENT_SHELL_LOCK")
        .env_remove("AGENT_SHELL_AGENT_ID")
        .current_dir(dir);
    command
}

/// 运行 CLI，并在其存活期间轮询授权界面；返回输出、耗时与观察到的待确认
/// 请求峰值。瞬态形态在进程退出时撤下界面，退出后采样必然为 0，所以界面
/// 是否存在只能在命令仍在运行时观察。
async fn run_cli_observing_authorization(
    mut command: Command,
    portal: &RemoteDesktopMock,
) -> (Output, Duration, usize) {
    let started = Instant::now();
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn agent-shell");
    let mut observed = 0;
    while child.try_wait().expect("poll agent-shell").is_none() {
        observed = observed.max(portal.held_starts());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let output = child
        .wait_with_output()
        .expect("collect agent-shell output");
    (output, started.elapsed(), observed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_fallback_returns_authorization_pending_and_reissues() {
    let bus = PrivateBus::start();
    let (portal_side, eis_side) = UnixStream::pair().expect("socketpair");
    let std_fd: std::os::fd::OwnedFd = portal_side.into();
    let portal = RemoteDesktopMock::new(zbus::zvariant::OwnedFd::from(std_fd), true);
    let _server = serve_remote_desktop(&bus, portal.clone()).await;

    let (daemon_bin, cli_bin) = tokio::task::spawn_blocking(binaries)
        .await
        .expect("join binary build");
    assert!(daemon_bin.is_file());
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = dir.path();

    // (1) 第一条输入命令：无常驻端点，派生瞬态服务，授权未确认时在自身预算内
    //     返回需授权结果（不是 CLI 的 5s 调用超时）。
    let (first, elapsed, held_while_running) = run_cli_observing_authorization(
        transient_cli(&cli_bin, runtime, &bus, &["input", "key", "Return"]),
        &portal,
    )
    .await;
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        !first.status.success(),
        "unconfirmed authorization must fail explicitly; stdout: {}",
        String::from_utf8_lossy(&first.stdout)
    );
    assert!(
        stderr.contains("1004"),
        "must return authorization-needed (1004), not a generic failure: {stderr}"
    );
    assert!(
        stderr.contains("授权未在本次命令预算内确认"),
        "error must state the authorization is pending: {stderr}"
    );
    assert!(
        !stderr.contains("timed out after 5s"),
        "daemon must answer inside the CLI budget: {stderr}"
    );
    assert!(
        elapsed < COMMAND_DEADLINE,
        "transient command took {elapsed:?}"
    );
    assert_eq!(portal.calls().creates, 1);
    assert_eq!(portal.calls().starts, 1);
    assert_eq!(portal.calls().connects, 0);
    assert_eq!(
        held_while_running, 1,
        "the transient daemon must hold the request while the command runs"
    );
    assert!(
        !runtime.join("agent-shell.sock").exists(),
        "transient fallback must not leave a resident endpoint behind"
    );

    // (2) 命令退出即瞬态进程退出：会话属主离开总线，portal 撤下授权界面。
    let mut closes = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        closes = portal.calls().closes;
        if closes >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        closes >= 1,
        "exiting transient service must withdraw the authorization request: {:?}",
        portal.calls()
    );
    assert_eq!(portal.held_starts(), 0, "authorization UI must be gone");

    // (3) 下一次调用重新发起授权请求并再次出现授权界面。
    let (second, elapsed, held_while_running) = run_cli_observing_authorization(
        transient_cli(&cli_bin, runtime, &bus, &["input", "key", "Return"]),
        &portal,
    )
    .await;
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second.status.success(),
        "second call still needs authorization"
    );
    assert!(
        stderr.contains("1004") && stderr.contains("授权未在本次命令预算内确认"),
        "second call must report pending authorization: {stderr}"
    );
    assert!(elapsed < COMMAND_DEADLINE, "second call took {elapsed:?}");
    assert_eq!(portal.calls().creates, 2, "next call re-issues the request");
    assert_eq!(portal.calls().starts, 2);
    assert_eq!(portal.calls().connects, 0);
    assert_eq!(
        held_while_running, 1,
        "authorization UI appears again while the second command runs"
    );
    assert!(
        !runtime.join("agent-shell.sock").exists(),
        "second transient call must not leave a resident endpoint"
    );

    // (4) 两次退出各撤下一次界面：累计撤下数不随进程退出回落。
    let mut withdrawn = portal.calls().request_closes;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        withdrawn = portal.calls().request_closes;
        if withdrawn >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        withdrawn >= 2,
        "both transient exits must withdraw their authorization request: {:?}",
        portal.calls()
    );
    assert_eq!(
        portal.held_starts(),
        0,
        "no authorization UI may outlive the second command"
    );

    drop(eis_side);
}
