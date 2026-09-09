//! portal Screenshot 单帧捕获（设计文档 §13.3）。
//!
//! `org.freedesktop.portal.Screenshot.Screenshot` 返回 Request 对象；
//! 必须等 `Response` 信号取回结果字典中的 `uri`，再解析为本地路径。
//! 超时/重试按 §19.3：screenshot 8s / 最多尝试 2 次。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_shell_core::error::{dbus_error, AgentShellError, Result};
use zbus::zvariant::{self};

use crate::portal_common::{
    portal_proxy, retry_transient, string_field, uri_to_path, wait_for_response,
    PORTAL_RETRY_BACKOFF, PORTAL_SERVICE,
};

/// screenshot 通道默认超时（§19.3）。
pub const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(8);
/// screenshot 通道最大尝试次数（§19.3；含首次，共 2 次尝试）。
pub const SCREENSHOT_MAX_ATTEMPTS: u32 = 2;
/// 生成 Screenshot 的 `handle_token`（pid + 序列号）。
fn screenshot_token(pid: u32, seq: u64) -> String {
    format!("agent_shell_screenshot_{pid}_{seq}")
}

/// 从持久计数器取下一个 token（跨 `capture()` 调用递增）。
///
/// 计数器必须是 `ScreenshotPortal` 字段（daemon 长生命周期内跨调用递增），
/// 而非 `capture()` 局部变量——否则连续两次 screenshot 首 token 恒为
/// `{pid}_0`，portal Request 路径仍可能冲突（TSI-2877 审查项 #1）。
fn next_token(counter: &AtomicU64, pid: u32) -> String {
    let n = counter.fetch_add(1, Ordering::SeqCst);
    screenshot_token(pid, n)
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
        let proxy = portal_proxy(&self.conn, "org.freedesktop.portal.Screenshot")
            .await
            .map_err(|e| AgentShellError::DBus(format!("Screenshot portal proxy: {e}")))?;
        // 方法调用本身短退避重试（portal GetSession 就绪竞态）；AccessDenied
        // 归一到 Permission（用户拒绝/无活跃会话），不重试。
        //
        // `handle_token` 每次尝试用递增序列号重新生成：portal Request 路径含
        // token，pid 进程内不变，若不叠加序列号，重试复用同一 token 仍会路径
        // 冲突（TSI-2877 审查项 #1）。
        // 序列号计数器取 self 字段（daemon 长生命周期内跨 capture 调用递增），
        // 保证连续两次 screenshot 首 token 也不同（TSI-2877 审查项 #1）。
        let token_seq = &self.token_seq;
        let request_path: zvariant::OwnedObjectPath =
            retry_transient(SCREENSHOT_MAX_ATTEMPTS, PORTAL_RETRY_BACKOFF, || {
                let proxy = &proxy;
                async move {
                    let token = next_token(token_seq, std::process::id());
                    let mut options = std::collections::HashMap::<&str, zvariant::Value>::new();
                    options.insert("handle_token", zvariant::Value::from(token.as_str()));
                    options.insert("interactive", zvariant::Value::from(interactive));
                    let body = ("", options);
                    proxy
                        .call("Screenshot", &body)
                        .await
                        .map_err(|e| dbus_error(format!("Screenshot call: {e}")))
                }
            })
            .await?;

        let mut last_err = None;
        for attempt in 0..SCREENSHOT_MAX_ATTEMPTS {
            match wait_for_response(&self.conn, &request_path, SCREENSHOT_TIMEOUT).await {
                Ok((_, results)) => {
                    let uri = string_field(&results, "uri").ok_or_else(|| {
                        AgentShellError::Capture("portal Screenshot: no uri in response".into())
                    })?;
                    return uri_to_path(uri).ok_or_else(|| {
                        AgentShellError::Capture(format!("portal Screenshot: bad uri {uri:?}"))
                    });
                }
                // 用户拒绝/取消不重试——语义明确，重试只会再次弹窗。
                Err(e @ AgentShellError::Permission(_)) => return Err(e),
                Err(e) => {
                    tracing::warn!(attempt, "portal Screenshot attempt failed: {e}");
                    last_err = Some(e);
                    if attempt + 1 < SCREENSHOT_MAX_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| AgentShellError::Capture("portal Screenshot failed".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 重试 token 必须不同：`seq` 递增 → 不同 token，不同 pid 也隔离。
    /// 若 token 仅由 pid 派生（进程内不变），重试会复用同一 token 致
    /// portal Request 路径冲突（TSI-2877 审查项 #1）。
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
    /// 两次「首 token」都会是 `{pid}_0`（TSI-2877 审查项 #1）。
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
}
