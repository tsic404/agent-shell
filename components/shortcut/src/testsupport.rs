//! 测试支持：桩脚本 spawn 的串行化闸门。
//!
//! 并发 `fork` 会继承其它测试「写打开的」脚本 fd；`execve` 的 ETXTBSY 检查
//! 早于 CLOEXEC 生效，且写桩用的 rename 保留同一 inode，因此「写桩 + spawn」
//! 必须互斥，否则偶发 `Text file busy`。
//!
//! **规则：任何会 fork 的用例都必须持有本闸门**——不只是写桩用例。spawn
//! 不存在的二进制同样会 fork，继承的写 fd 足以让另一个测试的 exec 失败。

/// 串行化写桩脚本并启动子进程的测试。
///
/// 在用例开头取一次并持有到用例结束；spawn 子进程的结构体（如 KDE 私有
/// bus）应把守卫存为字段，使后续用例无法漏加。
pub(crate) async fn fork_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    GUARD.lock().await
}
