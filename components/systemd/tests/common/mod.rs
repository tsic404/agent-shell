//! 共享测试 helper：环境受限时的跳过。

/// 环境受限（polkit 拒绝 / 单元缺失）时的跳过：打印带原因的 SKIP 行后由
/// 调用方提前 `return`，测试按 ok 收尾。
///
/// 不用 `std::process::exit`：那会终止整个测试进程，并行测试被静默丢弃；
/// 不用 panic 冒充跳过：标准 libtest 没有 "skipped" panic 协议（那是
/// libtest-mimic 的约定），panic 一律计为 FAILED 且退出码非 0。
/// 跳过原因始终输出可见，避免静默跳过掩盖验收覆盖面。
pub fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

/// 环境受限（缺 polkit 授权 / manager degraded / 状态变更被拒）时的跳过：
/// 在 `skip` 基础上追加「环境性跳过」意图短语，与 logind 侧
/// 「缺 polkit 授权（环境性跳过）」共用同一 grep 关键字，
/// 便于 CI 区分环境跳过与授权回归误报。
pub fn skip_environment(reason: &str) {
    eprintln!("SKIP: {reason}（环境性跳过）");
}
