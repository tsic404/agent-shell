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
/// 并发 spawn 时 CLI 每命令拉起一个瞬态 daemon，锁被前一个
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

/// 锁路径环境变量：覆盖默认会话级锁路径（`--lock-path` 优先于本变量）。
pub const ENV_LOCK_PATH: &str = "AGENT_SHELL_LOCK";

/// 解析单实例锁路径：`--lock-path` 显式值 > `AGENT_SHELL_LOCK` env > 默认路径。
///
/// 独立路径仍是独立命名空间：并行测试/无图形会话的隔离实例互不阻塞，同一路径
/// 上的第二个实例照旧被拦下。空值（含全空白）视为未设置，避免误落到 CWD。
pub fn resolve_lock_path(flag: Option<&Path>) -> PathBuf {
    resolve_lock_path_from(flag, std::env::var(ENV_LOCK_PATH).ok().as_deref())
}

/// [`resolve_lock_path`] 的纯函数形态（env 取值显式传入，单测不碰进程环境）。
fn resolve_lock_path_from(flag: Option<&Path>, env: Option<&str>) -> PathBuf {
    if let Some(path) = flag {
        return path.to_path_buf();
    }
    if let Some(path) = env.filter(|p| !p.trim().is_empty()) {
        return PathBuf::from(path);
    }
    default_lock_path()
}

impl SingleInstanceLock {
    /// 以默认等待窗口在指定路径获取锁（`--lock-path`/`AGENT_SHELL_LOCK` 隔离实例）。
    ///
    /// 使用方须已按 [`resolve_lock_path`] 解析路径；等待语义与默认会话锁相同。
    pub fn acquire_at_path(path: &Path) -> Result<Self, String> {
        Self::acquire_at(path, LOCK_WAIT_TIMEOUT)
    }

    /// 在指定路径、指定等待窗口内获取锁（生产走 [`Self::acquire_at_path`]，测试用
    /// 临时路径与毫秒级窗口隔离并行并覆盖超时路径）。
    ///
    /// 路径可来自 `--lock-path`/`AGENT_SHELL_LOCK`，可能落在共享目录：打开前先按
    /// 类型拒绝非普通文件（符号链接/目录/FIFO…），打开用 `O_NOFOLLOW` 关掉
    /// check→open 窗口，PID 也只经已打开的 fd 覆写——三段一起堵死「把锁路径做成
    /// 指向他人文件的符号链接，受害者一启动就截断改写之」。
    fn acquire_at(path: &Path, timeout: Duration) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("warn: cannot create runtime dir: {e}");
            }
        }

        match std::fs::symlink_metadata(path) {
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(format!(
                    "refusing to use lock path {}: existing path is a {}, not a regular file \
                     (use a path under a user-private directory such as $XDG_RUNTIME_DIR)",
                    path.display(),
                    crate::path_guard::describe_file_type(&meta.file_type())
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot inspect lock path {}: {e}", path.display())),
        }

        // `.truncate(false)`：排队等待方 open 时若 truncate 会清空持锁方写入的
        // PID（最长 30s 窗口内锁文件被清空）。PID 仅作调试提示，且 flock 不依赖
        // 文件内容；改由取得锁后经 fd 覆写，持锁期间 PID 保持可见。
        // `O_NOFOLLOW`：上面刚判过类型，这里再让内核兜底拒绝符号链接。
        use std::os::unix::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
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

        // 写入 PID 便于调试（锁文件常驻，stale PID 仅作提示）。经已打开的 fd 覆写，
        // 不再按路径二次打开——那等于把刚堵上的符号链接/替换窗口再开一次。
        let pid = std::process::id();
        if let Err(e) = write_pid(&file, pid) {
            tracing::warn!("cannot record pid in {}: {e}", path.display());
        }
        tracing::info!(pid, "single-instance lock acquired");
        Ok(Self { _file: file })
    }

    /// 显式释放锁（consume self，flock 随 `_file` 析构自动释放）。
    pub fn release(self) {
        // flock 在 _file 析构时释放；锁文件常驻，不在此删除（见结构体注释）。
    }
}

/// 经已打开的锁文件 fd 覆写 PID：先截断再写，位置固定在文件头。
///
/// 只用 fd 不用路径：路径可被替换，写穿符号链接会截断他人文件。
fn write_pid(file: &std::fs::File, pid: u32) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = file;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(format!("{pid}\n").as_bytes())?;
    file.flush()
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

/// 默认锁文件路径 `$XDG_RUNTIME_DIR/agent-shell.lock`。独立于 [`resolve_lock_path`]
/// 存在，使路径构造可被单测覆盖——测试只能做路径级断言，触碰全局锁即与常驻 daemon 竞争。
fn default_lock_path() -> PathBuf {
    runtime_dir().join("agent-shell.lock")
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
    fn test_default_lock_path_is_runtime_dir_lock_file() {
        // 路径级断言：不取锁、不改 `XDG_RUNTIME_DIR`（并行测试下改环境变量本身就是
        // 新的 flaky 源），只钉住「runtime_dir() 下名为 agent-shell.lock」这一约定。
        let path = default_lock_path();
        assert_eq!(path, runtime_dir().join("agent-shell.lock"));
        assert_eq!(
            path.file_name(),
            Some(std::ffi::OsStr::new("agent-shell.lock"))
        );
    }

    #[test]
    fn test_state_dir_ends_with_agent_shell() {
        let dir = state_dir();
        assert!(dir.to_string_lossy().ends_with("agent-shell"));
    }

    #[test]
    fn resolve_lock_path_prefers_explicit_flag_over_env() {
        let path = resolve_lock_path_from(Some(Path::new("/tmp/flag.lock")), Some("/tmp/env.lock"));
        assert_eq!(path, PathBuf::from("/tmp/flag.lock"));
    }

    #[test]
    fn resolve_lock_path_uses_env_when_flag_absent() {
        assert_eq!(
            resolve_lock_path_from(None, Some("/tmp/env.lock")),
            PathBuf::from("/tmp/env.lock")
        );
    }

    #[test]
    fn resolve_lock_path_falls_back_to_default_when_unset_or_blank() {
        // 空值/全空白视为未设置：`AGENT_SHELL_LOCK=` 不得把锁落到相对路径。
        for env in [None, Some(""), Some("   ")] {
            assert_eq!(resolve_lock_path_from(None, env), default_lock_path());
        }
    }

    /// 锁路径是符号链接时必须拒绝：共享目录里他人预埋链接，受害者的「打开锁文件 +
    /// 截断写 PID」会变成改写链接目标（如 `~/.bashrc`）。拒绝后目标内容不变。
    #[test]
    fn lock_path_symlink_is_refused_and_target_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("victim.txt");
        std::fs::write(&target, "precious content").expect("write target");
        let lock = dir.path().join("agent-shell.lock");
        std::os::unix::fs::symlink(&target, &lock).expect("plant symlink");

        let err = match SingleInstanceLock::acquire_at(&lock, Duration::from_millis(200)) {
            Ok(_) => panic!("symlinked lock path must be refused"),
            Err(e) => e,
        };
        assert!(
            err.contains("symlink") && err.contains(&lock.display().to_string()),
            "error must name the symlink and the path: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("target readable"),
            "precious content",
            "symlink target must not be truncated or rewritten"
        );
    }

    /// 悬空符号链接同样拒绝（`O_NOFOLLOW` 与类型检查都不允许它变成「新建目标文件」）。
    #[test]
    fn dangling_lock_symlink_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join("agent-shell.lock");
        std::os::unix::fs::symlink(dir.path().join("missing.txt"), &lock).expect("plant symlink");
        let err = match SingleInstanceLock::acquire_at(&lock, Duration::from_millis(200)) {
            Ok(_) => panic!("dangling symlink must be refused"),
            Err(e) => e,
        };
        assert!(err.contains("symlink"), "error must name the type: {err}");
        assert!(
            !dir.path().join("missing.txt").exists(),
            "must not create the symlink target"
        );
    }

    /// 目录占位同样拒绝（`flock` 对目录语义不同，不能当锁文件用）。
    #[test]
    fn lock_path_directory_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join("lock-dir");
        std::fs::create_dir(&lock).expect("create dir");
        let err = match SingleInstanceLock::acquire_at(&lock, Duration::from_millis(200)) {
            Ok(_) => panic!("directory lock path must be refused"),
            Err(e) => e,
        };
        assert!(err.contains("directory"), "error must name the type: {err}");
        assert!(lock.is_dir(), "directory must survive");
    }

    /// 拒绝符号链接后，换成普通文件路径可正常取锁，且同一路径上的第二个实例
    /// 仍被拦下——清理掉预埋链接并不能绕过单实例语义。
    #[test]
    fn lock_still_excludes_second_instance_after_symlink_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("victim.txt");
        std::fs::write(&target, "precious").expect("write target");
        let lock = dir.path().join("agent-shell.lock");
        std::os::unix::fs::symlink(&target, &lock).expect("plant symlink");
        assert!(SingleInstanceLock::acquire_at(&lock, Duration::from_millis(100)).is_err());

        std::fs::remove_file(&lock).expect("remove planted symlink");
        let held = SingleInstanceLock::acquire_at(&lock, Duration::from_millis(200))
            .expect("plain path acquires");
        let err = match SingleInstanceLock::acquire_at(&lock, Duration::from_millis(150)) {
            Ok(_) => panic!("second instance must be refused while the lock is held"),
            Err(e) => e,
        };
        assert!(
            err.contains("daemon already running"),
            "unexpected error: {err}"
        );
        drop(held);
        assert_eq!(
            std::fs::read_to_string(&target).expect("target readable"),
            "precious",
            "refused symlink attempt must leave the target alone"
        );
    }

    #[test]
    fn lock_acquire_and_release_roundtrip() {
        // 获取 → Drop 释放 flock 后可重新获取（锁文件常驻，不删除）。
        // 独立临时锁路径：默认 `$XDG_RUNTIME_DIR/agent-shell.lock` 会被同宿主
        // 常驻 daemon 或并行测试进程占用，全量测试下必然等到窗口耗尽而误报。
        // 窗口取短值：私有路径无竞争，取得锁即时成功；若 Drop 未释放 flock，
        // 重新获取在窗口内失败暴露回归，而非挂在默认 30s 等待上。
        const ROUNDTRIP_WAIT: Duration = Duration::from_millis(500);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.lock");

        let lock = SingleInstanceLock::acquire_at(&path, ROUNDTRIP_WAIT).expect("first acquire");
        drop(lock);
        let relock =
            SingleInstanceLock::acquire_at(&path, ROUNDTRIP_WAIT).expect("reacquire after release");
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
