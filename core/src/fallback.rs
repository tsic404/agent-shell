//! 通用降级链。
//!
//! 对应设计文档 `design/11-error-handling/README.md` §19.1–§19.4：
//! 按顺序尝试步骤，首个成功即返回，全部失败返回最后一步的错误。
//! 步骤名进入 `tracing` 日志（成功 debug 级、失败 warn 级），doctor 与排障依赖该输出。
//!
//! # 典型降级链（§19.2，下游使用范例）
//!
//! ```text
//! 聚焦窗口:  backend.focus_window (DE 原生) → a11y 窗口节点 activate() → input.click(标题栏中心坐标)
//! 输入文本:  a11y EditableText.setText → input.type_text (libei) → input.type_text (ydotool)
//! 截图:      portal ScreenCast (PipeWire) → DE 原生截图 (KWin D-Bus) → portal Screenshot → xwd/import (X11)
//! ```
//!
//! # 超时与重试默认值（§19.3/§19.4）
//!
//! 供调用方在 FallbackChain 外层包装超时与重试时引用；本模块以常量表形式导出。

use crate::error::{AgentShellError, Result};
use std::future::Future;
use std::pin::Pin;

/// 单命令默认超时：5s。
pub const DEFAULT_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// 等待类命令默认超时：15s（可配置）。
pub const DEFAULT_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// D-Bus 调用超时：3s。
pub const DBUS_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// D-Bus 调用（窗口查询）：默认超时 5s。
pub const DBUS_WINDOW_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// D-Bus 调用（窗口查询）：重试次数 2，间隔 500ms ×2。
pub const DBUS_WINDOW_QUERY_RETRIES: RetryPolicy = RetryPolicy {
    retries: 2,
    backoff_ms: 500,
};

/// KWin Scripting run_script：默认超时 5s。
pub const KWIN_RUN_SCRIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// KWin Scripting run_script：重试次数 1，间隔 1000ms。
pub const KWIN_RUN_SCRIPT_RETRIES: RetryPolicy = RetryPolicy {
    retries: 1,
    backoff_ms: 1000,
};

/// hyprctl socket 请求：默认超时 2s。
pub const HYPRCTL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// hyprctl socket 请求：重试次数 2，间隔 200ms ×2。
pub const HYPRCTL_REQUEST_RETRIES: RetryPolicy = RetryPolicy {
    retries: 2,
    backoff_ms: 200,
};

/// portal ScreenCast 会话：默认超时 10s。
pub const PORTAL_SCREENCAST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// portal ScreenCast 会话：重试次数 1，间隔 2000ms。
pub const PORTAL_SCREENCAST_RETRIES: RetryPolicy = RetryPolicy {
    retries: 1,
    backoff_ms: 2000,
};

/// 输入注入 (fake_input/XTest)：默认超时 1s。
pub const INPUT_INJECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
/// 输入注入 (fake_input/XTest)：重试次数 3，间隔 100ms ×2。
pub const INPUT_INJECT_RETRIES: RetryPolicy = RetryPolicy {
    retries: 3,
    backoff_ms: 100,
};

/// 截图 (portal→X11)：默认超时 8s。
pub const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
/// 截图 (portal→X11)：重试次数 2，间隔 1500ms。
pub const CAPTURE_RETRIES: RetryPolicy = RetryPolicy {
    retries: 2,
    backoff_ms: 1500,
};

/// 重试策略（次数 + 固定退避间隔）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 重试次数（不含首次尝试）。
    pub retries: u32,
    /// 每次重试前的固定退避间隔（毫秒）。
    pub backoff_ms: u64,
}

/// 降级链单个步骤。
///
/// `AsyncFn` 在当前稳定版尚不能作为 trait object 存储，等价的 boxed future
/// trait 表达如下——对外签名语义不变（设计 §19.1 允许）。
pub trait FallbackStep<T>: Send + Sync {
    /// 步骤名（进入 tracing 日志）。
    fn name(&self) -> &'static str;
    /// 执行步骤。
    fn run(&self) -> Pin<Box<dyn Future<Output = Result<T>> + Send + '_>>;
}

/// 通用降级链：按顺序尝试步骤，首个成功即返回，全部失败返回最后错误。
///
/// builder 语义，支持链式追加多个 `.step(...)`：
///
/// ```ignore
/// let chain = FallbackChain::new()
///     .step("native", || async { backend.focus_window(id).await })
///     .step("a11y", || async { a11y.activate(id).await });
/// let win = chain.execute().await?;
/// ```
pub struct FallbackChain<T> {
    steps: Vec<Box<dyn FallbackStep<T>>>,
}

impl<T> FallbackChain<T> {
    /// 创建空链。空链执行返回 [`AgentShellError::BackendUnavailable`]("empty chain")。
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// 追加一个命名步骤（builder 语义）。
    pub fn step<F, Fut>(mut self, name: &'static str, f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        self.steps.push(Box::new(ClosureStep { name, f }));
        self
    }

    /// 按顺序执行步骤，全部失败返回最后错误。
    pub async fn execute(&self) -> Result<T> {
        let mut last_err = None;
        for step in &self.steps {
            match step.run().await {
                Ok(r) => {
                    tracing::debug!("step '{}' ok", step.name());
                    return Ok(r);
                }
                Err(e) => {
                    tracing::warn!("step '{}' failed: {}", step.name(), e);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| AgentShellError::BackendUnavailable("empty chain".into())))
    }
}

impl<T> Default for FallbackChain<T> {
    fn default() -> Self {
        Self::new()
    }
}

struct ClosureStep<F> {
    name: &'static str,
    f: F,
}

impl<T, F, Fut> FallbackStep<T> for ClosureStep<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn run(&self) -> Pin<Box<dyn Future<Output = Result<T>> + Send + '_>> {
        Box::pin((self.f)())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, LazyLock, Mutex};

    async fn run_chain(chain: &FallbackChain<&'static str>) -> Result<&'static str> {
        chain.execute().await
    }

    #[tokio::test]
    async fn first_step_success_returns_immediately_and_skips_rest() {
        let later_ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&later_ran);
        let chain: FallbackChain<&'static str> = FallbackChain::new()
            .step("primary", || async { Ok("primary") })
            .step("fallback", move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Err(AgentShellError::BackendUnavailable("never".into()))
                }
            });

        assert_eq!(run_chain(&chain).await.unwrap(), "primary");
        assert_eq!(later_ran.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn intermediate_failure_falls_through_to_next_step() {
        let chain: FallbackChain<&'static str> = FallbackChain::new()
            .step("a11y", || async {
                Err(AgentShellError::Input("atspi down".into()))
            })
            .step("libei", || async { Ok("typed via libei") });

        assert_eq!(run_chain(&chain).await.unwrap(), "typed via libei");
    }

    #[tokio::test]
    async fn all_steps_fail_returns_last_error() {
        let chain: FallbackChain<()> = FallbackChain::new()
            .step("first", || async {
                Err(AgentShellError::Capture("portal denied".into()))
            })
            .step("last", || async {
                Err(AgentShellError::Input("xwd failed".into()))
            });

        let err = chain.execute().await.unwrap_err();
        assert!(matches!(&err, AgentShellError::Input(m) if m == "xwd failed"));
    }

    #[tokio::test]
    async fn empty_chain_reports_backend_unavailable() {
        let err = FallbackChain::<()>::new().execute().await.unwrap_err();
        assert!(matches!(&err, AgentShellError::BackendUnavailable(m) if m == "empty chain"),);
    }

    #[tokio::test]
    async fn logs_contain_step_names_on_success_and_failure() {
        tracing_subscriber_guard();
        let chain: FallbackChain<&'static str> = FallbackChain::new()
            .step("kwin-scripting", || async {
                Err(AgentShellError::DBus("script timeout".into()))
            })
            .step("atspi-activate", || async { Ok("activated") });

        run_chain(&chain).await.unwrap();

        let logs = captured_logs();
        assert!(
            logs.contains("step 'kwin-scripting' failed"),
            "logs: {logs}"
        );
        assert!(logs.contains("step 'atspi-activate' ok"), "logs: {logs}");
    }

    /// 注册一次性全局测试 subscriber，捕获本模块日志行到进程级缓冲。
    ///
    /// `cargo test` 并行运行时仅首个到达的线程成功注册；其余测试的断言
    /// 退化为读取共享缓冲（内容为全部日志行），不会 panic。
    fn tracing_subscriber_guard() {
        use std::sync::OnceLock;
        use tracing::subscriber::set_global_default;
        use tracing_subscriber::fmt;

        static SUBSCRIBER: OnceLock<()> = OnceLock::new();
        if SUBSCRIBER.set(()).is_err() {
            return;
        }
        let _ = set_global_default(
            fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(|| LogWriter)
                .finish(),
        );
    }

    static LOG_CAPTURE: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));

    /// 进程级日志缓冲快照（每行一条日志）。
    fn captured_logs() -> String {
        log_buf().lock().unwrap().join("\n")
    }

    struct LogWriter;

    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let line = String::from_utf8_lossy(buf).into_owned();
            log_buf().lock().unwrap().push(line);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn log_buf() -> &'static Mutex<Vec<String>> {
        &LOG_CAPTURE
    }
}
