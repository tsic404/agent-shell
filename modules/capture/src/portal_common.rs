//! freedesktop portal 公共设施（设计文档 §13.2/§13.3）。
//!
//! portal 调用的响应通过 `org.freedesktop.portal.Request` 对象的
//! `Response` 信号返回；调用方在 options 里传 `handle_token`，
//! Request 路径即 `/<sender>/<token>`。本模块提供：
//! - [`portal_proxy`]：构造指向 `org.freedesktop.portal.Desktop` 的通用代理；
//! - [`prepare_response_stream`] / [`drain_response_with_timeout`]：先订阅 `Response` 信号再发请求（避免竞态）；
//! - [`wait_for_response`]：调用后订阅的旧路径（仅适用于 `Screenshot` 等单步调用）。

use std::future::Future;
use std::time::Duration;

use agent_shell_core::error::AgentShellError;
use ordered_stream::OrderedStreamExt as _;
use zbus::zvariant::ObjectPath;

pub const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// portal/DBus 方法调用的短退避（§19 短退避）。portal GetSession 就绪竞态
/// 通常在百毫秒级恢复；Screenshot 与 ScreenCast 建会话共用此值，避免分叉调参。
pub const PORTAL_RETRY_BACKOFF: Duration = Duration::from_millis(150);

/// 构造指向 portal 服务的通用代理（interface 由调用方给定）。
pub async fn portal_proxy<'a>(
    conn: &zbus::Connection,
    interface: &'a str,
) -> zbus::Result<zbus::Proxy<'a>> {
    zbus::Proxy::new(conn, PORTAL_SERVICE, PORTAL_PATH, interface).await
}

/// 将 D-Bus unique name 编码为 portal Request 路径的 sender 段。
///
/// `":1.42"` → `"1_42"`——xdg-desktop-portal handle_token 路径编码规则：
/// 去掉前导 `':'`，将 `'.'` 替换为 `'_'`。
fn encode_sender_part(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').replace('.', "_")
}

/// 从连接的 unique name 计算 portal Request 路径的 sender 段。
///
/// 返回 `None` 当连接尚未获得 unique name（理论上 session bus 总有）。
pub fn sender_part(conn: &zbus::Connection) -> Option<String> {
    conn.unique_name().map(|n| encode_sender_part(n.as_str()))
}

/// 在发送 portal 方法调用 **之前** 订阅指定 Request 路径上的 `Response` 信号。
///
/// `request_path` 由调用方按 `handle_token` 规则预算：
/// `/org/freedesktop/portal/desktop/request/<sender>/<token>`。
/// 先订阅再发请求，避免后端在订阅前就回复导致信号丢失（竞态）。
/// 流交给 [`drain_response_with_timeout`] 消费。
pub async fn prepare_response_stream(
    conn: &zbus::Connection,
    request_path: &ObjectPath<'_>,
) -> Result<zbus::MessageStream, AgentShellError> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")
        .map_err(|e| AgentShellError::DBus(format!("match rule interface: {e}")))?
        .member("Response")
        .map_err(|e| AgentShellError::DBus(format!("match rule member: {e}")))?
        .path(request_path.clone())
        .map_err(|e| AgentShellError::DBus(format!("match rule path: {e}")))?
        .build();
    zbus::MessageStream::for_match_rule(rule, conn, Some(4))
        .await
        .map_err(|e| AgentShellError::DBus(format!("signal stream: {e}")))
}

/// 消费 [`prepare_response_stream`] 预订阅的流，等待 `Response` 信号。
///
/// 返回 `(response_code, results)`；超时按 §19.3 各通道配置由调用方控制。
/// response_code：0 = 成功，1 = 用户取消，2 = 其它错误。
///
/// `format_step` 为调用方提供的步骤标签（如 `Start`），超时时拼进错误
/// 信息——ScreenCast 多步流程里便于直接定位卡在哪一步。
pub async fn drain_response_with_timeout(
    stream: &mut zbus::MessageStream,
    timeout: Duration,
    format_step: &str,
) -> Result<(u32, std::collections::HashMap<String, zvariant::OwnedValue>), AgentShellError> {
    let wait = async {
        let Some(msg) = stream.next().await else {
            return Err(AgentShellError::DBus(
                "Request signal stream ended before Response".into(),
            ));
        };
        let msg = msg.map_err(|e| AgentShellError::DBus(format!("signal read: {e}")))?;
        let body = msg.body();
        let (code, results): (u32, std::collections::HashMap<String, zvariant::OwnedValue>) = body
            .deserialize()
            .map_err(|e| AgentShellError::DBus(format!("Response body: {e}")))?;
        if code != 0 {
            return Err(AgentShellError::Permission(format!(
                "portal request denied (response code {code})"
            )));
        }
        Ok((code, results))
    };
    tokio::time::timeout(timeout, wait).await.map_err(|_| {
        AgentShellError::Timeout(format!(
            "portal {format_step} Response timed out after {}s (no Response signal on the request path)",
            timeout.as_secs()
        ))
    })?
}

/// 等待 portal Request 的 Response 信号，返回 `(response_code, results)`。
///
/// `request_path` 来自 portal 方法的返回值；超时按 §19.3 各通道配置由调用方控制。
/// response_code：0 = 成功，1 = 用户取消，2 = 其它错误。
pub async fn wait_for_response(
    conn: &zbus::Connection,
    request_path: &ObjectPath<'_>,
    timeout: Duration,
) -> Result<(u32, std::collections::HashMap<String, zvariant::OwnedValue>), AgentShellError> {
    // Request 对象路径以 sender unique name 为前缀，直接在其上监听 Response。
    let proxy = zbus::Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        request_path,
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(|e| agent_shell_core::error::AgentShellError::DBus(format!("Request proxy: {e}")))?;

    let mut stream = proxy.receive_signal("Response").await.map_err(|e| {
        agent_shell_core::error::AgentShellError::DBus(format!("Response signal: {e}"))
    })?;

    let wait = async {
        // Response 信号首条即终态（0=成功 / 1=取消 / 2=错误）；流结束视为失败。
        let Some(msg) = stream.next().await else {
            return Err(agent_shell_core::error::AgentShellError::DBus(
                "Request signal stream ended before Response".into(),
            ));
        };
        let body = msg.body();
        let (code, results): (u32, std::collections::HashMap<String, zvariant::OwnedValue>) =
            body.deserialize().map_err(|e| {
                agent_shell_core::error::AgentShellError::DBus(format!("Response body: {e}"))
            })?;
        if code != 0 {
            return Err(agent_shell_core::error::AgentShellError::Permission(
                format!("portal request denied (response code {code})"),
            ));
        }
        Ok((code, results))
    };

    tokio::time::timeout(timeout, wait).await.map_err(|_| {
        agent_shell_core::error::AgentShellError::Timeout(format!(
            "portal Response timed out after {}s",
            timeout.as_secs()
        ))
    })?
}

/// 从结果字典取字符串字段。
pub fn string_field<'v>(
    results: &'v std::collections::HashMap<String, zvariant::OwnedValue>,
    key: &str,
) -> Option<&'v str> {
    let v = results.get(key)?;
    v.downcast_ref::<&str>().ok()
}

/// `file://` URI → 本地路径。
pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    Some(std::path::PathBuf::from(percent_decode(
        rest.split('?').next()?,
    )))
}

/// 最小百分比解码（file URI 常见 %20 等）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = match std::str::from_utf8(&bytes[i + 1..i + 3]) {
                Ok(h) => h,
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                    continue;
                }
            };
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// 短退避重试 portal/DBus 调用（§19 短退避）。
///
/// 仅重试瞬时错误：`Permission`（用户拒绝 / 无活跃图形会话的 AccessDenied）
/// 与 `Timeout` 语义明确，立即返回不重试；其余（D-Bus 竞态、Capture 失败等）
/// 按短退避重试，至多 `attempts` 次（含首次）。`attempts == 0` 归一到 1 次
/// 尝试（而非 panic）。
pub async fn retry_transient<T, F, Fut>(
    attempts: u32,
    backoff: Duration,
    mut f: F,
) -> agent_shell_core::error::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = agent_shell_core::error::Result<T>>,
{
    let mut last = None;
    for attempt in 0..attempts.max(1) {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e @ (AgentShellError::Permission(_) | AgentShellError::Timeout(_))) => {
                return Err(e);
            }
            Err(e) => {
                if attempt + 1 < attempts {
                    tracing::warn!(attempt, "portal/DBus call failed, retrying: {e}");
                    tokio::time::sleep(backoff).await;
                }
                last = Some(e);
            }
        }
    }
    Err(last.expect("retry loop ran at least once"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_parses() {
        assert_eq!(
            uri_to_path("file:///home/u/screenshot.png"),
            Some(std::path::PathBuf::from("/home/u/screenshot.png"))
        );
    }

    #[test]
    fn file_uri_with_query_and_percent() {
        assert_eq!(
            uri_to_path("file:///tmp/a%20b.png?token=1"),
            Some(std::path::PathBuf::from("/tmp/a b.png"))
        );
    }

    #[test]
    fn non_file_uri_rejected() {
        assert_eq!(uri_to_path("https://example.com/x.png"), None);
    }

    #[test]
    fn percent_decode_plain() {
        assert_eq!(percent_decode("/a/b%20c"), "/a/b c");
        assert_eq!(percent_decode("/plain"), "/plain");
    }

    #[test]
    fn encode_sender_part_strips_colon_and_dots() {
        // ":1.42" → "1_42"——portal handle_token 路径编码规则。
        assert_eq!(encode_sender_part(":1.42"), "1_42");
        assert_eq!(encode_sender_part(":1.99"), "1_99");
        assert_eq!(encode_sender_part(":1.0"), "1_0");
    }

    #[tokio::test]
    async fn retry_transient_retries_db_errors_and_gives_up() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let err = retry_transient(3, Duration::from_millis(1), || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err::<(), _>(AgentShellError::DBus("transient".into()))
            } else {
                Err::<(), _>(AgentShellError::Capture("final".into()))
            }
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(matches!(err, AgentShellError::Capture(_)));
    }
    #[tokio::test]
    async fn retry_transient_succeeds_after_transient_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let v = retry_transient(3, Duration::from_millis(1), || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(AgentShellError::DBus("transient".into()))
            } else {
                Ok(42u32)
            }
        })
        .await
        .expect("second attempt succeeds");
        assert_eq!(v, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_transient_does_not_retry_permission_or_timeout() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for kind in ["permission", "timeout"] {
            let calls = AtomicUsize::new(0);
            let ret = retry_transient(3, Duration::from_millis(1), || {
                let calls = &calls;
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let err = if kind == "permission" {
                        AgentShellError::Permission("denied".into())
                    } else {
                        AgentShellError::Timeout("timed out".into())
                    };
                    Err::<u32, _>(err)
                }
            })
            .await;
            assert!(ret.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1, "no retry for {kind}");
        }
    }

    /// `attempts == 0` 归一到 1 次尝试，返回错误而非 panic（TSI-2877 审查项 #2）。
    #[tokio::test]
    async fn retry_transient_zero_attempts_returns_error_not_panic() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let err = retry_transient(0, Duration::from_millis(1), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(AgentShellError::DBus("fail".into()))
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AgentShellError::DBus(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
