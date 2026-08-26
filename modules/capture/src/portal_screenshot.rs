//! portal Screenshot 单帧捕获（设计文档 §13.3）。
//!
//! `org.freedesktop.portal.Screenshot.Screenshot` 返回 Request 对象；
//! 必须等 `Response` 信号取回结果字典中的 `uri`，再解析为本地路径。
//! 超时/重试按 §19.3：screenshot 8s / 最多尝试 2 次。

use std::time::Duration;

use agent_shell_core::error::{AgentShellError, Result};
use zbus::zvariant::{self};

use crate::portal_common::{
    portal_proxy, string_field, uri_to_path, wait_for_response, PORTAL_SERVICE,
};

/// screenshot 通道默认超时（§19.3）。
pub const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(8);
/// screenshot 通道最大尝试次数（§19.3；含首次，共 2 次尝试）。
pub const SCREENSHOT_MAX_ATTEMPTS: u32 = 2;

/// portal Screenshot 组件：单帧 PNG 落盘路径获取。
pub struct ScreenshotPortal {
    conn: zbus::Connection,
}

impl ScreenshotPortal {
    /// 连接 session bus（不探测 portal 存在性——`available()` 懒探测）。
    pub async fn new() -> Result<Self> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
        Ok(Self { conn })
    }

    /// session bus 连接（供装配层复用同一连接）。
    pub fn with_connection(conn: zbus::Connection) -> Self {
        Self { conn }
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

        let token = format!("agent_shell_screenshot_{}", std::process::id());
        let mut options = std::collections::HashMap::<&str, zvariant::Value>::new();
        options.insert("handle_token", zvariant::Value::from(token.as_str()));
        options.insert("interactive", zvariant::Value::from(interactive));
        let request_path: zvariant::OwnedObjectPath = proxy
            .call("Screenshot", &("", options))
            .await
            .map_err(|e| AgentShellError::DBus(format!("Screenshot call: {e}")))?;

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
