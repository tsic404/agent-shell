//! 单实例锁（§22.2 D1）。
//!
//! daemon 使用 `$XDG_RUNTIME_DIR/agent-shell.lock` 文件锁防止多实例。
//! `flock(LOCK_EX | LOCK_NB)` 失败即返回 `daemon already running`。

use std::fs::OpenOptions;
use std::path::PathBuf;

/// 文件锁持有者——Drop 时自动释放 flock。
pub struct SingleInstanceLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl SingleInstanceLock {
    /// 尝试获取单实例锁。已锁定则返回 `daemon already running`。
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
        // flock(LOCK_EX | LOCK_NB) — 非阻塞排他锁
        let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err("daemon already running".into());
            }
            return Err(format!("lock failed: {err}"));
        }

        // 写入 PID 便于调试
        let pid = std::process::id();
        let _ = std::fs::write(&path, format!("{pid}\n"));
        tracing::info!(pid, "single-instance lock acquired");
        Ok(Self { _file: file, path })
    }

    /// 主动释放锁并清理锁文件（实际清理由 Drop 执行）。
    pub fn release(self) {
        // Drop 自动清理锁文件。
    }
}

impl Drop for SingleInstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
}
