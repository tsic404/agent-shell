//! daemon 进程生命周期回归：客户端消亡必须在任何阶段结束 daemon 进程，且退出要
//! 干净（exit 0，systemd `Restart=on-failure` 语义下不触发重启）。
//!
//! 四条路径覆盖 EOF 观察不到、被连带炸掉或被阻塞写拖住的情形：
//!
//! 1. 卡在单实例锁等待（前一个实例持锁）：此时不在请求间隙读 stdin，挂断须立即
//!    结束进程而非等满锁超时——残留的持锁 daemon 会让随后每条命令都超时失败；
//! 2. 到达空闲超时且客户端仍连着：进程须真正退出，不能因挂起的 stdin 阻塞读卡在
//!    运行时析构里；
//! 3. 客户端带走了 stderr 读者（daemon 的 stderr 就是调用方管道）：退出路径的日志
//!    写入失败不得把线程炸成 panic，否则退出码变 101；
//! 4. stderr 读端仍在但从不排空（管道写满）：退出路径的日志写入不得阻塞，否则
//!    `std::process::exit` 同样执行不到；
//! 5. 退出消息须自带行尾换行：多轮瞬态 daemon 的消息不能粘在 stderr 同一行上。
//!
//! 三个目录环境变量全部指向临时目录：锁文件、状态文件与本机 daemon / 并行测试
//! 隔离，不读写开发者 HOME。

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// 在 daemon 的锁文件上持排他 flock——daemon 启动后因此停留在锁等待里。
fn hold_lock(path: &Path) -> File {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .expect("open lock file");
    // SAFETY: `file` 持有打开的可读写 fd，`flock` 只对其施加文件锁。
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(
        rc,
        0,
        "lock file must be free: {}",
        std::io::Error::last_os_error()
    );
    file
}

/// spawn daemon：stdin 为管道（客户端通道），stdout 丢弃，stderr 由调用方决定。
fn spawn_daemon(dir: &Path, args: &[&str], stderr: Stdio) -> Child {
    Command::new(env!("CARGO_BIN_EXE_agent-shell-daemon"))
        .args(args)
        .env("XDG_RUNTIME_DIR", dir)
        .env("XDG_STATE_HOME", dir)
        .env("HOME", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn agent-shell-daemon")
}

/// 建管道（两端 CLOEXEC）。
fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` 是长度 2 的数组，`pipe2` 只写入这两个槽位。
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    assert_eq!(rc, 0, "pipe2: {}", std::io::Error::last_os_error());
    // SAFETY: 两个 fd 由 `pipe2` 新建，所有权移交本函数，未在别处使用。
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// 用非阻塞写把管道填满——模拟「读端存在但从不排空」的调用方。
fn fill_pipe(write_end: &OwnedFd) {
    let fd = write_end.as_raw_fd();
    // SAFETY: `fd` 是调用方持有的有效管道写端；`fcntl` 只改该 fd 的状态标志。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL: {}", std::io::Error::last_os_error());
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_eq!(rc, 0, "F_SETFL: {}", std::io::Error::last_os_error());
    let chunk = [0u8; 4096];
    loop {
        // SAFETY: 缓冲区是本函数的有效数组，长度与指针同源。
        let written = unsafe { libc::write(fd, chunk.as_ptr().cast(), chunk.len()) };
        if written <= 0 {
            break; // EAGAIN：管道已写满
        }
    }
    // 复位为阻塞写：该 fd 随后交给 daemon 作 stderr，必须保留「写满即阻塞」的
    // 语义，否则测不到本用例要覆盖的阻塞路径。
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    assert_eq!(
        rc,
        0,
        "F_SETFL restore: {}",
        std::io::Error::last_os_error()
    );
}

/// 等待子进程退出；超时先杀掉再失败，不给后续测试留残留 daemon。
///
/// 60s 上限而非正常耗时：daemon 装配含 D-Bus/portal 探测，无桌面环境下失败路径
/// 可能偏慢；此处只拦「永不退出」的挂死（修复前的失败形态）。
fn wait_exit(child: &mut Child, timeout: Duration, ctx: &str) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("try_wait failed ({ctx}): {e}"),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("daemon must exit within {timeout:?} {ctx}");
}

/// 客户端挂断（stdin 写端关闭）时 daemon 停在锁等待里：进程须立即退出，
/// 而非等满 30s 锁超时——残留的持锁 daemon 会让随后每条命令都超时失败。
#[test]
fn exits_when_client_hangs_up_while_waiting_for_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = hold_lock(&dir.path().join("agent-shell.lock"));
    let mut child = spawn_daemon(dir.path(), &[], Stdio::null());

    // CLI 死亡 = stdin 写端关闭。
    drop(child.stdin.take());
    let status = wait_exit(
        &mut child,
        Duration::from_secs(5),
        "after the client hung up while the daemon waited for the lock",
    );
    assert!(status.success(), "clean exit expected, got {status:?}");

    drop(lock);
}

/// 空闲超时（客户端仍连着、stdin 仍打开）时 daemon 必须真正结束进程：
/// 挂起的 stdin 阻塞读不得把进程留在运行时析构里持锁不放。
#[test]
fn exits_after_idle_timeout_while_client_still_attached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = spawn_daemon(dir.path(), &["--idle-timeout-secs", "1"], Stdio::null());
    // 保持 stdin 打开：等价于 CLI 仍在连接（如 events.subscribe 空闲等待）。
    let _stdin = child.stdin.take().expect("daemon stdin");

    let status = wait_exit(
        &mut child,
        Duration::from_secs(60),
        "after the idle timeout with the client still attached",
    );
    assert!(status.success(), "clean exit expected, got {status:?}");
}

/// 调用方退出即 daemon 的 stderr 无读者：退出路径照常写日志，写入失败（broken
/// pipe）不得让日志调用 panic——那会把线程炸掉、退出码变 101（常驻形态下被
/// `Restart=on-failure` 反复重启）。
#[test]
fn exits_cleanly_when_stderr_reader_is_gone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = spawn_daemon(dir.path(), &[], Stdio::piped());

    // 先收走 stderr 读者，再断开客户端：daemon 在 EOF 退出路径上写最后一条日志。
    drop(child.stderr.take());
    drop(child.stdin.take());
    let status = wait_exit(
        &mut child,
        Duration::from_secs(60),
        "after the client hung up with no stderr reader left",
    );
    assert!(status.success(), "clean exit expected, got {status:?}");
}

/// stderr 读端仍在但从不排空（管道写满）时挂断：退出不得依赖 stderr 写入——
/// 写满管道上的阻塞写（`tracing::info!` / `write_all`）会一直卡住，`exit(0)`
/// 执行不到，daemon 带着单实例锁残留，正是本 PR 要修的故障形态。
#[test]
fn exits_when_hung_up_with_stderr_pipe_full_and_undrained() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (stderr_read, stderr_write) = pipe();
    // 先把管道写满，再交给 daemon：daemon 后续每条日志都会撞上满管。
    fill_pipe(&stderr_write);
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-shell-daemon"))
        .env("XDG_RUNTIME_DIR", dir.path())
        .env("XDG_STATE_HOME", dir.path())
        .env("HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // 读端留在本进程且永不读取：写端有读者，故不会 EPIPE，只会写满阻塞。
        .stderr(Stdio::from(stderr_write))
        .spawn()
        .expect("spawn agent-shell-daemon");

    drop(child.stdin.take());
    let status = wait_exit(
        &mut child,
        Duration::from_secs(15),
        "after the client hung up with a full, never-drained stderr pipe",
    );
    assert!(status.success(), "clean exit expected, got {status:?}");
    drop(stderr_read);
}

/// 退出消息必须自成一行（行尾 `\n`）：瞬态 daemon 每命令一个，粘行会在真实终端
/// 上与 shell 提示符相接。
///
/// 自持 flock 让 daemon 停在锁等待里，退出只能走 watchdog 的 `log_exit`——stderr
/// 上除它之外没有任何 tracing 输出，末尾字节因此是确定性的。
#[test]
fn exit_message_ends_with_newline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = hold_lock(&dir.path().join("agent-shell.lock"));
    let mut child = spawn_daemon(dir.path(), &[], Stdio::piped());
    let mut stderr = child.stderr.take().expect("daemon stderr");

    drop(child.stdin.take());
    let status = wait_exit(
        &mut child,
        Duration::from_secs(5),
        "after the client hung up (checking stderr message framing)",
    );
    assert!(status.success(), "clean exit expected, got {status:?}");

    // 子进程已退出：写端关闭，读到 EOF 即拿到完整输出。
    let mut output = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut output).expect("read daemon stderr");
    assert_eq!(
        output, "stdin hangup; exiting\n",
        "exit message must be one newline-terminated line, got {output:?}"
    );

    drop(lock);
}
