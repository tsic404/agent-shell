//! 通知组件（设计文档 §21.3 通知、§21.4 NotificationComponent）。
//!
//! 降级链：DE 封装（backends/，如 KdeNotification）→
//! `org.freedesktop.Notifications`（session bus 标准规范，KDE/GNOME/DDE 均实现）
//! → portal `org.freedesktop.portal.Notification`。
//!
//! Hyprland 的 xdg-desktop-portal-hyprland 不支持 Notification portal（§3.5.3），
//! 因此公共兜底是 freedesktop Notifications 而非 portal。

use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, NotificationComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{NotificationSpec, NotificationUrgency};
use async_trait::async_trait;
use zbus::proxy;

const OWNER_NAME: &str = "org.freedesktop.Notifications";
const PORTAL_OWNER: &str = "org.freedesktop.portal.Desktop";

/// org.freedesktop.Notifications 标准代理。
#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait FreedesktopNotifications {
    /// 返回通知 id；`replaces_id > 0` 时替换既有通知。
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;
}

fn urgency_value(urgency: NotificationUrgency) -> u8 {
    match urgency {
        NotificationUrgency::Low => 0,
        NotificationUrgency::Normal => 1,
        // Critical 不被自动超时吞掉：urgency=2 且 timeout=0（常驻）。
        NotificationUrgency::Critical => 2,
    }
}

/// 组装 Notify 调用的 hints 表（urgency；动作经 actions 数组传递）。
fn build_hints(
    spec: &NotificationSpec,
) -> std::collections::HashMap<&'static str, zbus::zvariant::Value<'static>> {
    let mut hints = std::collections::HashMap::new();
    hints.insert(
        "urgency",
        zbus::zvariant::Value::from(urgency_value(spec.urgency)),
    );
    // desktop-entry 标识通知来源应用，与是否有动作无关——总是设置。
    hints.insert("desktop-entry", zbus::zvariant::Value::from("agent-shell"));
    hints
}

fn actions_flat(spec: &NotificationSpec) -> Vec<String> {
    let mut flat = Vec::with_capacity(spec.actions.len() * 2);
    for (key, label) in &spec.actions {
        flat.push(key.clone());
        flat.push(label.clone());
    }
    flat
}

/// freedesktop 标准通知组件（session bus 公共路径）。
pub struct FreedesktopNotification {
    conn: zbus::Connection,
    available: bool,
}

impl FreedesktopNotification {
    pub async fn new() -> Result<Self> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
        Ok(Self {
            conn,
            available: true,
        })
    }

    async fn proxy(&self) -> Result<FreedesktopNotificationsProxy<'static>> {
        FreedesktopNotificationsProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("Notifications proxy: {e}")))
    }

    fn map<T>(r: zbus::Result<T>, what: &'static str) -> Result<T> {
        r.map_err(|e| AgentShellError::DBus(format!("Notifications {what}: {e}")))
    }
}

#[async_trait]
impl DesktopComponent for FreedesktopNotification {
    fn name(&self) -> &'static str {
        "freedesktop-notification"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Notification
    }

    fn is_available(&self) -> bool {
        self.available
    }

    async fn health(&self) -> ComponentHealth {
        use zbus::fdo::DBusProxy;
        match DBusProxy::new(&self.conn).await {
            Ok(dbus) => match dbus.name_has_owner(OWNER_NAME.try_into().unwrap()).await {
                Ok(true) => ComponentHealth::Healthy,
                Ok(false) => {
                    ComponentHealth::Degraded("no org.freedesktop.Notifications owner".into())
                }
                Err(e) => ComponentHealth::Degraded(format!("{e}")),
            },
            Err(e) => ComponentHealth::Degraded(format!("{e}")),
        }
    }
}

#[async_trait]
impl NotificationComponent for FreedesktopNotification {
    async fn send_notification(&self, notif: &NotificationSpec) -> Result<u32> {
        let actions = actions_flat(notif);
        let action_refs: Vec<&str> = actions.iter().map(String::as_str).collect();
        let hints = build_hints(notif);
        // Critical 常驻（0 = 不超时）；其余用 spec 超时或 -1（服务默认）。
        let timeout = if notif.urgency == NotificationUrgency::Critical {
            0
        } else {
            notif.timeout_ms.unwrap_or(-1)
        };
        Self::map(
            self.proxy()
                .await?
                .notify(
                    "agent-shell",
                    0,
                    notif.icon.as_deref().unwrap_or("dialog-information"),
                    &notif.summary,
                    notif.body.as_deref().unwrap_or(""),
                    action_refs,
                    hints,
                    timeout,
                )
                .await,
            "Notify",
        )
    }

    async fn close_notification(&self, id: u32) -> Result<()> {
        Self::map(
            self.proxy()
                .await?
                .close_notification(id)
                .await,
            "CloseNotification",
        )
    }
}

/// portal 通知降级（org.freedesktop.portal.Notification）。
///
/// 注意 portal AddNotification 的 id 由调用方指定（string id），返回值语义与
/// freedesktop 规范的递增 uint 不同——此处用内部计数器映射为 u32。
pub struct PortalNotification {
    conn: zbus::Connection,
    counter: std::sync::atomic::AtomicU32,
    available: bool,
}

impl PortalNotification {
    pub async fn new() -> Result<Self> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
        Ok(Self {
            conn,
            counter: std::sync::atomic::AtomicU32::new(1),
            available: true,
        })
    }

    async fn portal(&self) -> Result<zbus::Proxy<'_>> {
        zbus::Proxy::new(
            &self.conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Notification",
        )
        .await
        .map_err(|e| AgentShellError::DBus(format!("portal Notification proxy: {e}")))
    }

    fn spec_to_portal_body(notif: &NotificationSpec) -> zbus::zvariant::Value<'static> {
        use zbus::zvariant::Value;
        let mut fields: std::collections::HashMap<String, Value<'static>> =
            std::collections::HashMap::new();
        fields.insert("title".into(), Value::from(notif.summary.clone()));
        if let Some(body) = &notif.body {
            fields.insert("body".into(), Value::from(body.clone()));
        }
        fields.insert(
            "priority".into(),
            Value::from(match notif.urgency {
                NotificationUrgency::Low => "low".to_string(),
                NotificationUrgency::Normal => "normal".to_string(),
                NotificationUrgency::Critical => "urgent".to_string(),
            }),
        );
        Value::from(fields)
    }
}

#[async_trait]
impl DesktopComponent for PortalNotification {
    fn name(&self) -> &'static str {
        "portal-notification"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Notification
    }

    fn is_available(&self) -> bool {
        self.available
    }

    async fn health(&self) -> ComponentHealth {
        use zbus::fdo::DBusProxy;
        let dbus = match DBusProxy::new(&self.conn).await {
            Ok(d) => d,
            Err(e) => return ComponentHealth::Degraded(format!("{e}")),
        };
        match dbus
            .name_has_owner(PORTAL_OWNER.try_into().unwrap())
            .await
        {
            Ok(true) => ComponentHealth::Healthy,
            Ok(false) => ComponentHealth::Unavailable,
            Err(e) => ComponentHealth::Degraded(format!("{e}")),
        }
    }
}

#[async_trait]
impl NotificationComponent for PortalNotification {
    async fn send_notification(&self, notif: &NotificationSpec) -> Result<u32> {
        let id = self.counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let portal_id = format!("agent-shell-{id}");
        let body = Self::spec_to_portal_body(notif);
        let portal = self.portal().await?;
        let arg: (&str, zbus::zvariant::Value) = (portal_id.as_str(), body);
        let _reply: () = portal
            .call("AddNotification", &arg)
            .await
            .map_err(|e| AgentShellError::DBus(format!("AddNotification: {e}")))?;
        Ok(id)
    }

    async fn close_notification(&self, id: u32) -> Result<()> {
        let portal = self.portal().await?;
        let pid = format!("agent-shell-{id}");
        let arg: (&str,) = (pid.as_str(),);
        let _reply: () = portal
            .call("RemoveNotification", &arg)
            .await
            .map_err(|e| AgentShellError::DBus(format!("RemoveNotification: {e}")))?;
        Ok(())
    }
}
