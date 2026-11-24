//! portal Screenshot 单帧捕获（设计文档 §13.3）。
//!
//! `org.freedesktop.portal.Screenshot.Screenshot` 返回 Request 对象；
//! 必须等 `Response` 信号取回结果字典中的 `uri`，再解析为本地路径。
//! 超时/重试：非交互 screenshot 3s / 最多 2 次；交互（弹授权窗）单次，方法
//! 调用另有固定上限（超时后二次弹窗无意义），返回后的确认等待由调用方预算
//! 承担——GNOME 的确认框会让方法调用本身阻塞到用户点击。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_shell_core::error::{dbus_error, AgentShellError, Result};
use ordered_stream::OrderedStreamExt as _;
use zbus::zvariant::{self, ObjectPath};

use crate::portal_common::{
    close_portal_request, drain_response_with_timeout, portal_proxy, prepare_response_stream,
    sender_part, string_field, uri_to_path, wait_for_response, PORTAL_RETRY_BACKOFF,
    PORTAL_SERVICE,
};

/// screenshot 通道默认超时（非交互 doctor/probe 路径）。
///
/// 必须低于 C-154 的 5s 命令预算；方法调用本身也受此值约束，因为 KDE 的
/// 授权弹窗会让 `Screenshot` 调用直接等待用户输入。
pub const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(3);
/// screenshot 交互（弹授权窗）的**方法调用**上限：GNOME/KDE 的确认框会让
/// 这次调用本身阻塞到用户点击，固定上限保证无人值守时不会一直挂着；放弃
/// 方法调用不等于放弃确认，返回后的等待另由 [`ScreenshotBudget::deadline`]
/// 覆盖。
pub const SCREENSHOT_TIMEOUT_INTERACTIVE: Duration = Duration::from_secs(3);
/// screenshot 通道最大尝试次数（§19.3；含首次，共 2 次尝试）。
pub const SCREENSHOT_MAX_ATTEMPTS: u32 = 2;

/// 被放弃的 Screenshot Request 的收尾窗口。
///
/// 静默探测在 1s 预算到期时丢弃整个 `capture()` future，portal 仍可能在此后
/// 1–3 秒写出 PNG。守卫在这个窗口内等晚到的 `Response` 取 URI 并删源图；
/// 它在独立任务里计时，不延长任何一级预算。
const ABANDONED_REQUEST_GRACE: Duration = Duration::from_secs(3);
/// 收尾任务里 `Request.Close` 的单次上限；与分级预算不叠加。
const ABANDONED_REQUEST_CLOSE_BUDGET: Duration = Duration::from_millis(200);

/// 交互与否对应的 Screenshot 超时（§13.1 全链预算 / §19.3）。
pub fn screenshot_timeout(interactive: bool) -> Duration {
    if interactive {
        SCREENSHOT_TIMEOUT_INTERACTIVE
    } else {
        SCREENSHOT_TIMEOUT
    }
}

/// 交互与否对应的最大尝试次数：交互单次（超时/取消后重试只会二次弹窗），
/// 非交互保留 §19.3 的 2 次（覆盖 GetSession 就绪竞态等瞬态错误）。
pub fn screenshot_attempts(interactive: bool) -> u32 {
    if interactive {
        1
    } else {
        SCREENSHOT_MAX_ATTEMPTS
    }
}

/// 一次 portal Screenshot 尝试的预算。
///
/// 方法调用与 Response 等待分开计时：KDE 的授权弹窗会让方法调用本身阻塞，
/// 必须先以短预算判定并降级；GNOME 的确认对话框既可能阻塞方法调用、也可能
/// 在方法返回后才等用户点击，两种情况都由 `deadline` 覆盖返回后的等待。
/// 两者共用一个绝对截止，避免两级预算叠加把单次调用放大（C-154）。
#[derive(Clone, Copy, Debug)]
pub struct ScreenshotBudget {
    /// 方法调用的单次上限。
    pub call: Duration,
    /// 方法调用与 Response 等的整体截止。
    pub deadline: tokio::time::Instant,
    /// 允许的尝试次数；等待用户确认的路径只允许一次。
    pub attempts: u32,
}

impl ScreenshotBudget {
    /// 常规预算：方法调用与 Response 各用通道默认超时。
    pub fn standard(interactive: bool) -> Self {
        let timeout = screenshot_timeout(interactive);
        Self {
            call: timeout,
            deadline: tokio::time::Instant::now() + timeout,
            attempts: screenshot_attempts(interactive),
        }
    }
}

/// 单次 portal Screenshot 的去向。
#[derive(Debug)]
pub enum ScreenshotAttempt {
    /// 成功落盘。
    Captured(std::path::PathBuf),
    /// 门户已受理请求，但用户在预算内没有完成确认界面（如 GNOME 的截图确认框）。
    AwaitingConfirmation(AgentShellError),
    /// 后端不可用、被拒绝或方法调用失败。
    Unavailable(AgentShellError),
}

/// 生成 Screenshot 的 `handle_token`（pid + 序列号）。
fn screenshot_token(pid: u32, seq: u64) -> String {
    format!("agent_shell_screenshot_{pid}_{seq}")
}

/// 从持久计数器取下一个 token（跨 `capture()` 调用递增）。
///
/// 计数器必须是 `ScreenshotPortal` 字段（daemon 长生命周期内跨调用递增），
/// 而非 `capture()` 局部变量——否则连续两次 screenshot 首 token 恒为
/// `{pid}_0`，portal Request 路径仍可能冲突。
fn next_token(counter: &AtomicU64, pid: u32) -> String {
    let n = counter.fetch_add(1, Ordering::SeqCst);
    screenshot_token(pid, n)
}

/// 单个 Screenshot Request 的生命周期守卫。
///
/// `capture()` 被外层探测预算整体丢弃时，本地 future 里的信号订阅与 Request
/// 句柄一起消失；portal 未必随之中止，晚到的 `Response` 里仍带源图 URI。
/// 守卫持有预订阅的流，在 Drop 时派生一个有界收尾任务：先发
/// `org.freedesktop.portal.Request.Close`，再等晚到的成功 `Response`，
/// 取到 URI 即删除源图。晚到的图只用于清理，不会回流成本次捕获结果。
///
/// 成功返回（调用方接管产物）或明确拒绝（无产物）时 [`Self::disarm`]；
/// 确认超时保持武装，由守卫在 Drop 里收尾。重试路径也保持武装，每次尝试
/// 各自收尾。
struct ScreenshotRequestGuard {
    conn: zbus::Connection,
    request_path: ObjectPath<'static>,
    stream: Option<zbus::MessageStream>,
    armed: bool,
}

impl ScreenshotRequestGuard {
    fn new(
        conn: zbus::Connection,
        request_path: ObjectPath<'static>,
        stream: zbus::MessageStream,
    ) -> Self {
        Self {
            conn,
            request_path,
            stream: Some(stream),
            armed: true,
        }
    }

    fn stream_mut(&mut self) -> Option<&mut zbus::MessageStream> {
        self.stream.as_mut()
    }

    /// 后端未按 handle_token 返回路径：改收尾目标为实际路径。
    ///
    /// 该路径在调用返回后才可知，预订阅流对不上任何 Response；收尾退化为
    /// 有界 `Request.Close`。非常规后端才会走到这里。
    fn retarget(&mut self, path: &str) {
        match ObjectPath::try_from(path.to_owned()) {
            Ok(path) => self.request_path = path,
            Err(e) => tracing::warn!("Screenshot cleanup path {path}: {e}"),
        }
        self.stream = None;
    }

    /// 调用方已接管产物或确认无产物：不再收尾。
    fn disarm(&mut self) {
        self.armed = false;
        self.stream = None;
    }
}

impl Drop for ScreenshotRequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("no tokio runtime for portal Screenshot cleanup; skipping");
            return;
        };
        let conn = self.conn.clone();
        let request_path = self.request_path.clone();
        let stream = self.stream.take();
        handle.spawn(async move {
            let _ = tokio::time::timeout(
                ABANDONED_REQUEST_CLOSE_BUDGET,
                close_portal_request(&conn, request_path.as_str()),
            )
            .await;
            let Some(mut stream) = stream else {
                return;
            };
            // Close 的等待期间流仍在缓冲：先 Close 再消费不会丢已到达的信号。
            let deadline = tokio::time::Instant::now() + ABANDONED_REQUEST_GRACE;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let msg = match tokio::time::timeout(remaining, stream.next()).await {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(e))) => {
                        tracing::debug!("late portal Screenshot Response read failed: {e}");
                        break;
                    }
                    Ok(None) | Err(_) => break,
                };
                let body = msg.body();
                let parsed: std::result::Result<
                    (u32, std::collections::HashMap<String, zvariant::OwnedValue>),
                    _,
                > = body.deserialize();
                let Ok((code, results)) = parsed else {
                    continue;
                };
                // Response 是请求终态：只可能有一条。
                if code == 0 {
                    if let Some(path) = string_field(&results, "uri").and_then(uri_to_path) {
                        if let Err(e) = std::fs::remove_file(&path) {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                tracing::warn!(
                                    "late portal Screenshot cleanup failed {}: {e}",
                                    path.display()
                                );
                            }
                        }
                    }
                }
                break;
            }
        });
    }
}

/// 把成功 Response 的结果字典转成捕获去向，并解除守卫的收尾职责。
///
/// Response 是请求终态，成功路径不会再晚到第二条：调用方接管产物，守卫
/// 不再 `Request.Close`，也不删除门户落盘文件。
fn finish_screenshot_response(
    request: &mut ScreenshotRequestGuard,
    results: &std::collections::HashMap<String, zvariant::OwnedValue>,
) -> ScreenshotAttempt {
    request.disarm();
    let Some(uri) = string_field(results, "uri") else {
        return ScreenshotAttempt::Unavailable(AgentShellError::Capture(
            "portal Screenshot: no uri in response".into(),
        ));
    };
    match uri_to_path(uri) {
        Some(path) => ScreenshotAttempt::Captured(path),
        None => ScreenshotAttempt::Unavailable(AgentShellError::Capture(format!(
            "portal Screenshot: bad uri {uri:?}"
        ))),
    }
}

/// portal Screenshot 组件：单帧 PNG 落盘路径获取。
pub struct ScreenshotPortal {
    conn: zbus::Connection,
    /// Screenshot 方法调用的单调序列号，跨 `capture()` 调用递增——保证
    /// handle_token 在 daemon 长生命周期内唯一。
    token_seq: AtomicU64,
}

impl ScreenshotPortal {
    /// 连接 session bus（不探测 portal 存在性——`available()` 懒探测）。
    pub async fn new() -> Result<Self> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
        Ok(Self {
            conn,
            token_seq: AtomicU64::new(0),
        })
    }

    /// session bus 连接（供装配层复用同一连接）。
    pub fn with_connection(conn: zbus::Connection) -> Self {
        Self {
            conn,
            token_seq: AtomicU64::new(0),
        }
    }

    /// portal 服务是否在 bus 上（NameHasOwner 探测）。
    pub async fn available(&self) -> bool {
        use zbus::fdo::DBusProxy;
        match DBusProxy::new(&self.conn).await {
            Ok(dbus) => dbus
                .name_has_owner(PORTAL_SERVICE.try_into().unwrap())
                .await
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// 截取屏幕，返回 PNG 本地路径。
    ///
    /// `interactive: true` 时允许 portal 弹授权窗；false 时仅当已有
    /// 持久化授权（或后端免弹窗配置）才成功。
    pub async fn capture(&self, interactive: bool) -> Result<std::path::PathBuf> {
        match self
            .capture_with_budget(interactive, ScreenshotBudget::standard(interactive))
            .await
        {
            ScreenshotAttempt::Captured(path) => Ok(path),
            ScreenshotAttempt::AwaitingConfirmation(e) | ScreenshotAttempt::Unavailable(e) => {
                Err(e)
            }
        }
    }

    /// 在显式预算内截屏，区分「等待用户确认」与「后端不可用」。
    ///
    /// 等待确认的失败不能直接当作后端不可用去降级：门户界面已经弹出，
    /// 再发起下一级只会叠加一个无人应答的界面。
    pub async fn capture_with_budget(
        &self,
        interactive: bool,
        budget: ScreenshotBudget,
    ) -> ScreenshotAttempt {
        // 整级绝对截止：proxy 建立、Response 订阅、方法调用与 Response 等待
        // 共用 `budget.deadline`。任一段变慢都不能借用后续确认的预算，单次
        // 方法调用另有 `budget.call` 固定上限（C-154）。
        let level_budget = budget
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        let remaining = || {
            budget
                .deadline
                .saturating_duration_since(tokio::time::Instant::now())
        };

        let proxy = match tokio::time::timeout(
            remaining(),
            portal_proxy(&self.conn, "org.freedesktop.portal.Screenshot"),
        )
        .await
        {
            Ok(Ok(proxy)) => proxy,
            Ok(Err(e)) => {
                return ScreenshotAttempt::Unavailable(AgentShellError::DBus(format!(
                    "Screenshot portal proxy: {e}"
                )))
            }
            Err(_) => {
                return ScreenshotAttempt::Unavailable(AgentShellError::Timeout(format!(
                    "portal Screenshot proxy did not answer within the {}ms level budget",
                    level_budget.as_millis()
                )))
            }
        };
        let sender = match sender_part(&self.conn) {
            Some(sender) => sender,
            None => {
                return ScreenshotAttempt::Unavailable(AgentShellError::DBus(
                    "no unique name on session bus".into(),
                ))
            }
        };

        // §19.3：非交互最多 2 次尝试；交互单次（超时/取消后重试只会二次弹窗）。
        // 每次生成新 token（pid 进程内不变，重试复用同一
        // token 会致 portal Request 路径冲突）；关键：先在 handle_token 预算的
        // 路径上订阅 Response 信号，再发 Screenshot 调用——否则 portal 对无授权
        // `interactive=false` 请求的即时取消（code=1）可能在订阅建立前就到达，
        // 被 `wait_for_response` 遗漏而误判为超时（debug 下订阅延迟
        // 放大该竞态）。
        let timeout = budget.call;
        let attempts = budget.attempts;
        let mut last_err = None;
        for attempt in 0..attempts {
            let token = next_token(&self.token_seq, std::process::id());
            let request_path = ObjectPath::try_from(format!(
                "/org/freedesktop/portal/desktop/request/{sender}/{token}"
            ))
            .map_err(|e| AgentShellError::DBus(format!("request path: {e}")));
            let request_path = match request_path {
                Ok(path) => path,
                Err(e) => return ScreenshotAttempt::Unavailable(e),
            };
            // 先订阅再发请求（避免竞态）。流建立失败（瞬时 AddMatch 竞态）按
            // 可重试 attempt 失败处理，而非 `?` 整体中断——与下方 proxy.call
            // 瞬态分支同口径，保留 §19.3 的第二次尝试。
            let setup_budget = remaining();
            if setup_budget.is_zero() {
                return ScreenshotAttempt::Unavailable(AgentShellError::Timeout(format!(
                    "portal Screenshot level budget exhausted before attempt {}",
                    attempt + 1
                )));
            }
            let stream = match tokio::time::timeout(
                setup_budget,
                prepare_response_stream(&self.conn, &request_path),
            )
            .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(
                        attempt,
                        "portal Screenshot response stream setup failed: {e}"
                    );
                    last_err = Some(e);
                    if attempt + 1 < attempts {
                        let backoff = PORTAL_RETRY_BACKOFF.min(remaining());
                        if !backoff.is_zero() {
                            tokio::time::sleep(backoff).await;
                        }
                    }
                    continue;
                }
                Err(_) => {
                    return ScreenshotAttempt::Unavailable(AgentShellError::Timeout(format!(
                        "portal Screenshot response subscription exceeded the {}ms level budget",
                        level_budget.as_millis()
                    )))
                }
            };
            // 从这里起 Request 已可能被 portal 接受：守卫持有预订阅流，任何
            // 非成功/非拒绝的离开路径都由它在 Drop 里收尾（含外层 1s 预算
            // 直接丢弃本 future 的情况）。
            let mut request =
                ScreenshotRequestGuard::new(self.conn.clone(), request_path.clone(), stream);

            let mut options = std::collections::HashMap::<&str, zvariant::Value>::new();
            options.insert("handle_token", zvariant::Value::from(token.as_str()));
            options.insert("interactive", zvariant::Value::from(interactive));
            let body = ("", options);

            // 方法调用：AccessDenied 归一到 Permission（用户拒绝/无活跃会话），
            // 与 Timeout 同为语义明确错误，不重试；其余瞬态错误（GetSession
            // 就绪竞态）短退避后进入下一尝试。
            let call_budget = timeout.min(remaining());
            if call_budget.is_zero() {
                return ScreenshotAttempt::Unavailable(AgentShellError::Timeout(format!(
                    "portal Screenshot method call budget exhausted before attempt {}",
                    attempt + 1
                )));
            }
            let returned: Option<zvariant::OwnedObjectPath> =
                match tokio::time::timeout(call_budget, proxy.call("Screenshot", &body)).await {
                    Ok(Ok(p)) => Some(p),
                    Ok(Err(e)) => {
                        let e = dbus_error(format!("Screenshot call: {e}"));
                        if matches!(
                            e,
                            AgentShellError::Permission(_) | AgentShellError::Timeout(_)
                        ) {
                            return ScreenshotAttempt::Unavailable(e);
                        }
                        tracing::warn!(attempt, "portal Screenshot call failed: {e}");
                        last_err = Some(e);
                        if attempt + 1 < attempts {
                            let backoff = PORTAL_RETRY_BACKOFF.min(remaining());
                            if !backoff.is_zero() {
                                tokio::time::sleep(backoff).await;
                            }
                        }
                        continue;
                    }
                    Err(_) => {
                        let e = AgentShellError::Timeout(format!(
                            "portal Screenshot method did not return within {}ms \
                         (authorization dialog may be waiting for input)",
                            call_budget.as_millis()
                        ));
                        tracing::warn!(attempt, "{e}");
                        // 交互级：GNOME 的确认框在用户点击前一直阻塞方法调用，
                        // 请求本身已经建立。固定上限只决定何时放弃这一次方法
                        // 调用；不能连确认等待一起判死——否则 `--wait` 永远
                        // 兑现不了。保留预订阅流，继续在同一条 Request 上等
                        // Response（C-154：放宽的是返回后的等待）。
                        if !interactive {
                            return ScreenshotAttempt::Unavailable(e);
                        }
                        // 整级预算已在这段方法等待里耗尽（默认 3s 预算与 3s
                        // 方法上限相等时必然如此）：请求已发出、门户仍在等
                        // 确认，收场是「未确认」而不是「后端不可用」。守卫
                        // 保持武装，由 Drop 做有界收尾。
                        if remaining().is_zero() {
                            return ScreenshotAttempt::AwaitingConfirmation(e);
                        }
                        None
                    }
                };

            // 方法调用已返回，或交互级已放弃方法而请求仍在：剩余预算属于
            // 门户确认界面。GNOME 的 `interactive=false` 也弹确认框，且只在
            // 用户点击后才发 Response。
            let response_budget = remaining();

            // portal 若未按 handle_token 规范返回预算路径（非常规实现），回退
            // 到调用后订阅的旧路径——该路径有竞态，但仅非常规后端才会触发。
            let result = match returned.as_ref() {
                Some(returned) if returned.as_str() != request_path.as_str() => {
                    tracing::warn!(
                        "Screenshot path mismatch: expected {}, got {}",
                        request_path,
                        returned
                    );
                    request.retarget(returned.as_str());
                    wait_for_response(&self.conn, returned, response_budget).await
                }
                _ => match request.stream_mut() {
                    Some(stream) => {
                        drain_response_with_timeout(stream, response_budget, "Screenshot").await
                    }
                    None => wait_for_response(&self.conn, &request_path, response_budget).await,
                },
            };

            match result {
                Ok((_, results)) => {
                    return finish_screenshot_response(&mut request, &results);
                }
                // 用户拒绝/取消不重试——语义明确，重试只会再次弹窗。
                Err(e @ AgentShellError::Permission(_)) => {
                    // 明确拒绝/取消不产出文件，无需晚响应收尾。
                    request.disarm();
                    return ScreenshotAttempt::Unavailable(e);
                }
                // Request 已建立却等不到 Response：确认界面没人点击，不是
                // 后端不可用。保留为独立去向，由调用方决定报错文案。
                Err(e @ AgentShellError::Timeout(_)) => {
                    return ScreenshotAttempt::AwaitingConfirmation(e);
                }
                Err(e) => {
                    tracing::warn!(attempt, "portal Screenshot attempt failed: {e}");
                    last_err = Some(e);
                    if attempt + 1 < attempts {
                        let backoff = Duration::from_millis(1500).min(remaining());
                        if !backoff.is_zero() {
                            tokio::time::sleep(backoff).await;
                        }
                    }
                }
            }
        }
        ScreenshotAttempt::Unavailable(
            last_err.unwrap_or_else(|| AgentShellError::Capture("portal Screenshot failed".into())),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 重试 token 必须不同：`seq` 递增 → 不同 token，不同 pid 也隔离。
    /// 若 token 仅由 pid 派生（进程内不变），重试会复用同一 token 致
    /// portal Request 路径冲突。
    #[test]
    fn screenshot_token_is_unique_per_sequence() {
        let pid = std::process::id();
        assert_ne!(screenshot_token(pid, 0), screenshot_token(pid, 1));
        assert_ne!(screenshot_token(pid, 0), screenshot_token(pid, 2));
        // 不同 pid 也隔离（跨进程）。
        assert_ne!(
            screenshot_token(pid, 0),
            screenshot_token(pid.wrapping_add(1), 0)
        );
    }

    /// 持久计数器跨调用递增：同一个计数器（`ScreenshotPortal` 字段）连续两次
    /// 取号得到不同 token——若计数器是 `capture()` 局部变量（每次重置为 0），
    /// 两次「首 token」都会是 `{pid}_0`。
    #[test]
    fn next_token_persists_counter_across_calls() {
        let counter = AtomicU64::new(0);
        let pid = std::process::id();
        let first = next_token(&counter, pid);
        let second = next_token(&counter, pid);
        assert_ne!(first, second);
        assert_eq!(first, screenshot_token(pid, 0));
        assert_eq!(second, screenshot_token(pid, 1));
    }

    /// 交互与非交互的尝试次数分离：交互单次（超时/取消后重试只会二次弹窗）；
    /// 非交互保留默认重试。两者超时都必须低于 5s 命令预算。
    #[test]
    fn screenshot_timeout_and_attempts_split_by_interactive() {
        assert_eq!(screenshot_timeout(false), SCREENSHOT_TIMEOUT);
        assert_eq!(screenshot_timeout(true), SCREENSHOT_TIMEOUT_INTERACTIVE);
        assert_eq!(screenshot_attempts(false), SCREENSHOT_MAX_ATTEMPTS);
        assert_eq!(screenshot_attempts(true), 1);
        assert!(SCREENSHOT_TIMEOUT < Duration::from_secs(5));
        assert!(SCREENSHOT_TIMEOUT_INTERACTIVE < Duration::from_secs(5));
    }
}
