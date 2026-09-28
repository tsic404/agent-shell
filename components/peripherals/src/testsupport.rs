//! 测试脚手架：私有 session bus + CLI stub 脚本。

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

/// 独立私有 session bus：被测对象与 mock 服务共享同一地址，避免
/// `Connection::session()` 依赖进程环境变量而在并行测试间互相干扰。
pub(crate) struct TestBus {
    /// dbus-daemon 打印的 `unix:...` 地址。
    addr: String,
    /// daemon 子进程句柄（Drop 时回收）。
    child: Child,
}

impl TestBus {
    /// 启动 `dbus-daemon` 并读取 `--print-address=1` 输出。
    pub(crate) fn start() -> Self {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon must be installed for KWin backend tests");
        let stdout = child.stdout.take().expect("piped stdout");
        let addr = read_address_line(stdout);
        assert!(
            addr.starts_with("unix:"),
            "dbus-daemon printed unexpected address: {addr:?}"
        );
        Self { addr, child }
    }

    /// 连接到该私有 bus。
    pub(crate) async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.addr.as_str())
            .expect("dbus-daemon address must parse")
            .build()
            .await
            .expect("connect to private session bus")
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        // SIGKILL 跳过 dbus-daemon 的正常退出路径，/tmp/dbus-* socket 不被
        // unlink、stale 文件会累积；SIGTERM 让它自行清理。
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

/// 逐字节读地址行：`dbus-daemon --print-address=1` 恰好一行，
/// 用 `read_line` 有缓冲越界吞掉后续输出的风险。
fn read_address_line(stdout: ChildStdout) -> String {
    let mut reader = std::io::BufReader::new(stdout);
    let mut bytes = Vec::new();
    loop {
        let mut buf = [0u8; 1];
        match reader.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("read dbus-daemon address: {e}"),
        }
        bytes.push(buf[0]);
        if buf[0] == b'\n' {
            break;
        }
    }
    let line = String::from_utf8(bytes).expect("dbus-daemon address must be UTF-8");
    assert!(!line.is_empty(), "dbus-daemon printed no address line");
    line.trim_end_matches('\n').to_string()
}

/// 写一个可执行 stub 脚本，代替真实 CLI 二进制。
///
/// 先写临时名再 rename：执行的是从未以写方式打开过的 inode，避免并行测试
/// 中其它线程 fork 时继承写到一半的 fd，导致 exec 报 ETXTBSY 的偶发失败。
pub(crate) fn stub_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let tmp = dir.join(format!("{name}.tmp"));
    let path = dir.join(name);
    std::fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    path
}

/// 串行化所有会 fork 子进程的测试。
///
/// fork 出的子进程会短暂继承其它线程打开着的写 fd，即使该 fd 是 CLOEXEC
/// （execve 的 ETXTBSY 检查早于 CLOEXEC 收尾），于是并行测试里 stub 脚本
/// 偶发 `Text file busy`。持锁覆盖「写 stub → exec」全程即可根除。
pub(crate) async fn fork_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static FORK_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
    FORK_LOCK.lock().await
}
