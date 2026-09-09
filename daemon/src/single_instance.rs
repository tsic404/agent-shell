//! 单实例锁（§22.2 D1）。
//!
//! daemon 使用 `$XDG_RUNTIME_DIR/agent-shell.lock` 文件锁防止多实例。
//! 锁竞争时排队等待前持锁者退出（有界），超时方判定 `daemon already running`。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 文件锁持有者——Drop 时自动释放 flock。
///
/// 锁文件**不**在 Drop 时删除：`remove_file` 在 `drop()` 体执行、早于
/// `_file` 字段析构，会把已 unlink 的 inode 暴露给重试窗口内的新 daemon
/// ——其拿到的 flock 与 PID 写入新建的文件不在同一 inode，第三个实例可
/// 绕过单实例锁。锁文件常驻 `$XDG_RUNTIME_DIR`（随重启清理），stale PID
/// 仅作调试提示。
pub struct SingleInstanceLock {
    _file: std::fs::File,
}

/// 默认排他锁最长等待时间。
///
/// 并发 spawn 时 CLI 每命令拉起一个 `--foreground` 瞬态 daemon，锁被前一个
/// daemon 持有至其**完整生命周期**结束（启动 ~1s + 命令服务 + 退出；`doctor`
/// 含 portal 会话探测与截图，单命令最坏可达数秒）。排队等待窗口必须覆盖单条
/// 命令时长，后一个 daemon 才能在锁释放后接续服务，而非过早判死。同时保持有界：
/// 真正的常驻实例（未来 socket activation 接线）或卡死实例仍会超时失败，不会
/// 永久阻塞。上限经 [`acquire_at`] 注入，测试用毫秒级窗口覆盖超时路径。
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// 非阻塞轮询间隔。阻塞式 `flock(LOCK_EX)` 无法从另一线程可靠打断
/// （Linux 上对阻塞中的 fd 调用 close 不会唤醒该 flock），故用 `LOCK_NB` +
/// 轮询实现有界等待；30s / 100ms = 300 次尝试，代价可忽略。
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

impl SingleInstanceLock {
    /// 获取单实例锁。锁被前一个 daemon 持有时排队等待（有界），超时方返回
    /// `daemon already running`。
    pub fn acquire() -> Result<Self, String> {
        let dir = runtime_dir();
        let path = dir.join("agent-shell.lock");
        Self::acquire_at(&path, LOCK_WAIT_TIMEOUT)
    }

    /// 在指定路径、指定等待窗口内获取锁（生产走 [`acquire`]，测试用临时路径
    /// 与毫秒级窗口隔离并行并覆盖超时路径）。
    fn acquire_at(path: &Path, timeout: Duration) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("warn: cannot create runtime dir: {e}");
            }
        }

        // `.truncate(false)`：排队等待方 open 时若 truncate 会清空持锁方写入的
        // PID（最长 30s 窗口内锁文件被清空）。PID 仅作调试提示，且 flock 不依赖
        // 文件内容；改由取得锁后经 `write` 覆写，持锁期间 PID 保持可见。
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("cannot create lock file {}: {e}", path.display()))?;

        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        match try_lock(fd, timeout) {
            Ok(true) => {}
            Ok(false) => {
                return Err(format!(
                    "daemon already running (lock wait timed out after {timeout:?})"
                ))
            }
            Err(e) => return Err(e),
        }

        // 写入 PID 便于调试（锁文件常驻，stale PID 仅作提示）。
        let pid = std::process::id();
        let _ = std::fs::write(path, format!("{pid}\n"));
        tracing::info!(pid, "single-instance lock acquired");
        Ok(Self { _file: file })
    }

    /// 显式释放锁（consume self，flock 随 `_file` 析构自动释放）。
    pub fn release(self) {
        // flock 在 _file 析构时释放；锁文件常驻，不在此删除（见结构体注释）。
    }
}

/// 有界排他锁：`flock(LOCK_EX | LOCK_NB)`，`WouldBlock` 时轮询等待直至
/// `timeout` 耗尽。
///
/// 返回 `Ok(true)` 取得锁、`Ok(false)` 等待耗尽仍未取得（真正的常驻实例
/// 冲突或前持锁者卡死）、`Err` 非 `WouldBlock` 的真实系统错误。前一个
/// 瞬态 daemon 的 flock 随其进程死亡由内核释放，无需人工干预。
fn try_lock(fd: std::os::fd::RawFd, timeout: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: `fd` 是上方 `OpenOptions` 打开的活文件描述符，flock 仅对其
        // 施加文件锁，不涉及内存安全。
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::WouldBlock {
            return Err(format!("lock failed: {err}"));
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(LOCK_POLL_INTERVAL);
    }
}

/// 获取 `$XDG_RUNTIME_DIR`，回退到 `/run/user/{uid}`。
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}"))
}

/// 获取状态目录路径 `~/.local/state/agent-shell/`。
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("agent-shell");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".local/state/agent-shell");
    }
    PathBuf::from(".local/state/agent-shell")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_dir_returns_path() {
        let dir = runtime_dir();
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn test_state_dir_ends_with_agent_shell() {
        let dir = state_dir();
        assert!(dir.to_string_lossy().ends_with("agent-shell"));
    }

    #[test]
    fn lock_acquire_and_release_roundtrip() {
        // 获取 → Drop 释放 flock 后可重新获取（锁文件常驻，不删除）。
        let lock = SingleInstanceLock::acquire().expect("first acquire");
        drop(lock);
        let relock = SingleInstanceLock::acquire().expect("reacquire after release");
        drop(relock);
    }

    #[test]
    fn lock_waits_for_previous_holder_to_exit() {
        // 并发 spawn 场景：前一个 daemon 持锁服务命令期间，后一个 daemon 必须
        // 排队等待前持锁者退出，而非立即判死。持锁 2s / 断言 ≥1.5s，留 500ms
        // 余量避免负载 CI 下唤醒延迟误报。用独立临时锁路径隔离并行测试。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.lock");

        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let holder_path = path.clone();
        let holder = std::thread::spawn(move || {
            let lock = SingleInstanceLock::acquire_at(&holder_path, LOCK_WAIT_TIMEOUT)
                .expect("holder acquire");
            acquired_tx.send(()).expect("signal acquired");
            std::thread::sleep(Duration::from_secs(2));
            drop(lock);
        });

        // 等 holder 已持锁后再发起等待，确保测的是「排队」而非「直接成功」。
        acquired_rx.recv().expect("holder acquired signal");
        let start = Instant::now();
        let lock =
            SingleInstanceLock::acquire_at(&path, LOCK_WAIT_TIMEOUT).expect("waiter acquire");
        let elapsed = start.elapsed();
        drop(lock);
        holder.join().expect("holder join");

        assert!(
            elapsed >= Duration::from_millis(1500),
            "waiter acquired too early ({elapsed:?}); should wait for previous holder"
        );
    }

    #[test]
    fn lock_wait_times_out_when_holder_persists() {
        // 真正的常驻实例（或前持锁者卡死）：等待方在注入的短窗口耗尽后失败，
        // 而非永久阻塞。持锁者不释放，等待方 150ms 窗口应超时判死。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.lock");

        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let holder_path = path.clone();
        let holder = std::thread::spawn(move || {
            let lock = SingleInstanceLock::acquire_at(&holder_path, LOCK_WAIT_TIMEOUT)
                .expect("holder acquire");
            acquired_tx.send(()).expect("signal acquired");
            // 持锁不释放，直到等待方超时失败（1s > 150ms 窗口）。
            std::thread::sleep(Duration::from_secs(1));
            drop(lock);
        });

        acquired_rx.recv().expect("holder acquired signal");
        let err = match SingleInstanceLock::acquire_at(&path, Duration::from_millis(150)) {
            Ok(_) => panic!("waiter must time out, not acquire"),
            Err(e) => e,
        };
        assert!(
            err.contains("daemon already running"),
            "unexpected error: {err}"
        );
        holder.join().expect("holder join");
    }
}
