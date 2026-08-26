//! freedesktop portal 公共设施（设计文档 §13.2/§13.3）。
//!
//! portal 调用的响应通过 `org.freedesktop.portal.Request` 对象的
//! `Response` 信号返回；调用方在 options 里传 `handle_token`，
//! Request 路径即 `/<sender>/<token>`。本模块提供：
//!
//! - [`portal_proxy`]：构造指向 `org.freedesktop.portal.Desktop` 的通用代理；
//! - [`wait_for_response`]：等待指定 Request 路径上的 Response 信号并解析结果字典。

use std::time::Duration;

use agent_shell_core::error::AgentShellError;
use ordered_stream::OrderedStreamExt as _;
use zbus::zvariant::ObjectPath;

pub const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// 构造指向 portal 服务的通用代理（interface 由调用方给定）。
pub async fn portal_proxy<'a>(
    conn: &zbus::Connection,
    interface: &'a str,
) -> zbus::Result<zbus::Proxy<'a>> {
    zbus::Proxy::new(conn, PORTAL_SERVICE, PORTAL_PATH, interface).await
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
}
