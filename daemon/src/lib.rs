//! agent-shell-daemon 库入口（§22.2 D1 / §23.2）。
//!
//! 二进制（`src/main.rs`）只做参数解析与进程生命周期编排：装配、请求分派与服务
//! 循环都在本库中。实现仓的验收用例（`tests/acceptance/`）因此能驱动同一份
//! `dispatch` 与 `serve` 做端到端判定，而不必复制一份实现、或退化成只能从进程
//! 外部黑盒观察。

pub mod a11y;
pub mod capture;
pub mod dispatch;
pub mod ime_session;
pub mod input;
pub mod path_guard;
pub mod portal_sessions;
pub mod rootd_client;
pub mod serve;
pub mod single_instance;
pub mod state;

/// 退出路径的日志：非阻塞尽力写 stderr（`serve` 的瞬态连接与看门狗共用）。
///
/// 退出路径不能走 tracing（阻塞写）：stderr 读端仍在但不排空时写满管道会让调用
/// 线程无限阻塞，`std::process::exit` 执行不到，进程带着单实例锁残留——正是本
/// crate 要防的故障形态。这里 dup fd 2、置 `O_NONBLOCK` 后写一次：管道满（EAGAIN）
/// 或无读者（EPIPE；Rust 运行时已忽略 SIGPIPE）都立即返回，写不出去即丢弃。
/// 消息保持纯文本、单行——只求不阻塞、不 panic。写入时自动补行尾 `\n`：多轮瞬态
/// daemon 的退出消息各自成行，不会在 stderr 上粘成一行、与 shell 提示符相接。
pub fn log_exit(msg: &str) {
    // SAFETY: 只操作本进程的 stderr；dup 出的 fd 在本次调用内关闭，不与其它线程
    // 共享；写入缓冲区是本函数的有效切片，长度与指针同源。
    unsafe {
        let fd = libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 3);
        if fd < 0 {
            return;
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        libc::write(fd, msg.as_ptr().cast(), msg.len());
        libc::write(fd, b"\n".as_ptr().cast(), 1);
        libc::close(fd);
    }
}
