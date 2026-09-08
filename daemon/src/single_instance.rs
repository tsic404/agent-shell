//! 单实例锁（§22.2 D1）。
//!
//! daemon 使用 `$XDG_RUNTIME_DIR/agent-shell.lock` 文件锁防止多实例。
//! `flock(LOCK_EX | LOCK_NB)` 失败即返回 `daemon already running`。

use std::fs::OpenOptions;
use std::path::PathBuf;

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

impl SingleInstanceLock {
    /// 尝试获取单实例锁。已锁定则返回 `daemon already running`。
    ///
    /// CLI 每命令 spawn 一个 `--foreground` 瞬态 daemon，退出时 SIGKILL +
    /// Drop 释放 flock 存在毫秒级竞态——下一个命令的 daemon 可能撞上「前一
    /// 个实例尚在退出」。故 `WouldBlock` 时短暂重试（flock 随进程死亡由内核
    /// 释放），而非立即判死。重试窗口有界，真正的常驻实例冲突仍会失败。
    pub fn acquire() -> Result<Self, String> {
        let dir = runtime_dir();
        let path = dir.join("agent-shell.lock");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("warn: cannot create runtime dir: {e}");
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("cannot create lock file {}: {e}", path.display()))?;

        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        match try_lock(fd) {
            Ok(true) => {}
            Ok(false) => return Err("daemon already running".into()),
            Err(e) => return Err(e),
        }

        // 写入 PID 便于调试（锁文件常驻，stale PID 仅作提示）。
        let pid = std::process::id();
        let _ = std::fs::write(&path, format!("{pid}\n"));
        tracing::info!(pid, "single-instance lock acquired");
        Ok(Self { _file: file })
    }

    /// 显式释放锁（consume self，flock 随 `_file` 析构自动释放）。
    pub fn release(self) {
        // flock 在 _file 析构时释放；锁文件常驻，不在此删除（见结构体注释）。
    }
}

/// `WouldBlock` 时的重试次数与间隔（40 × 25ms ≈ 1s 上限，含首次共 41 次尝试）。
const LOCK_RETRY_ATTEMPTS: u32 = 40;
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// 非阻塞排他锁：`flock(LOCK_EX | LOCK_NB)`，`WouldBlock` 时短暂重试。
///
/// 返回 `Ok(true)` 取得锁、`Ok(false)` 重试耗尽仍未取得（真正的常驻实例
/// 冲突）、`Err` 非 `WouldBlock` 的真实系统错误。重试只在 `WouldBlock` 上
/// 进行——瞬态 daemon 的 flock 随其进程死亡由内核释放，无需等待人工干预。
fn try_lock(fd: std::os::fd::RawFd) -> Result<bool, String> {
    for attempt in 0..=LOCK_RETRY_ATTEMPTS {
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            if attempt == LOCK_RETRY_ATTEMPTS {
                return Ok(false);
            }
            std::thread::sleep(LOCK_RETRY_DELAY);
            continue;
        }
        return Err(format!("lock failed: {err}"));
    }
    Ok(false)
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
}
