//! 测试支持：桩脚本 spawn 的串行化闸门。
//!
//! 并发 `fork` 会继承其它测试「写打开的」脚本 fd；`execve` 的 ETXTBSY 检查
//! 早于 CLOEXEC 生效，且写桩用的 rename 保留同一 inode，因此「写桩 + spawn」
//! 必须互斥，否则偶发 `Text file busy`。

/// 串行化写桩脚本并启动子进程的测试。
pub(crate) async fn fork_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    GUARD.lock().await
}
