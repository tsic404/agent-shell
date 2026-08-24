//! logind 会话管理组件（设计文档 §21.3「电源与会话 (Power / Lock)」跨 DE 行）。
//!
//! [`LogindComponent`] 封装 system bus 上的 `org.freedesktop.login1`
//! （Manager / Session 对象），实现 core 的 [`SessionManagerComponent`] 与
//! [`DesktopComponent`]。不依赖 DE——TTY 后端装配时的必选组件之一；
//! elogind 服务名兼容留待实测，先按标准 logind 实现。

use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, SessionManagerComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::SessionInfo;
use async_trait::async_trait;
use std::fmt::Display;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

/// D-Bus 错误归一化：任何传输层错误 → [`AgentShellError::DBus`]。
fn dbus_err<E: Display>(e: E) -> AgentShellError {
    AgentShellError::DBus(e.to_string())
}

/// `org.freedesktop.login1.Manager` 的 zbus proxy。
#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login1Manager {
    // → a(susso): id, uid, name, seat, object path
    #[allow(clippy::type_complexity)] // D-Bus 签名原样，无法拆分
    fn list_sessions(&self) -> zbus::Result<Vec<(String, u32, String, String, OwnedObjectPath)>>;

    fn get_session(&self, id: &str) -> zbus::Result<OwnedObjectPath>;
    fn can_reboot(&self) -> zbus::Result<String>;
    fn can_power_off(&self) -> zbus::Result<String>;
    fn reboot(&self, interactive: bool) -> zbus::Result<()>;
    fn power_off(&self, interactive: bool) -> zbus::Result<()>;
}

/// `org.freedesktop.login1.Session` 的 zbus proxy（按会话对象路径构造）。
#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    assume_defaults = true
)]
trait Login1Session {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;

    /// 用户 (uid, user object path)
    #[zbus(property)]
    fn user(&self) -> zbus::Result<(u32, ObjectPath<'_>)>;

    /// 用户名
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    /// seat (名称, seat object path)
    #[zbus(property)]
    fn seat(&self) -> zbus::Result<(String, ObjectPath<'_>)>;

    /// X11 display（":0"）；Wayland/纯 TTY 会话为 ""。
    #[zbus(property)]
    fn display(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn remote(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn remote_host(&self) -> zbus::Result<String>;

    /// "active" / "online" / "closing"。
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    /// 关联 TTY（如 "tty2"）；无则 ""。D-Bus 属性名为全大写 `TTY`。
    #[zbus(property, name = "TTY")]
    fn tty(&self) -> zbus::Result<String>;

    fn lock(&self) -> zbus::Result<()>;
    fn unlock(&self) -> zbus::Result<()>;
}

/// logind 会话管理组件实例。
pub struct LogindComponent {
    conn: zbus::Connection,
}

impl LogindComponent {
    /// D-Bus 方法调用超时：全局默认 5s（design/11 §19.4）。
    const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// 连接 system bus（elogind 等替代实现的服务名兼容留待实测）。
    ///
    /// 方法调用超时遵循全局默认 5s（design/11 §19.4）。
    pub async fn connect() -> Result<Self> {
        let conn = zbus::connection::Builder::system()
            .map_err(dbus_err)?
            .method_timeout(Self::CALL_TIMEOUT)
            .build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("connect system bus: {e}")))?;
        Ok(Self { conn })
    }

    async fn manager(&self) -> zbus::Result<Login1ManagerProxy<'_>> {
        Login1ManagerProxy::new(&self.conn).await
    }

    /// 按会话对象路径构造 Session proxy（assume_defaults 提供
    /// service/interface 默认值，路径必须显式指定）。
    async fn session<'a>(&'a self, path: ObjectPath<'a>) -> zbus::Result<Login1SessionProxy<'a>> {
        Login1SessionProxy::builder(&self.conn)
            .path(path)?
            .build()
            .await
    }

    /// `GetSession(id)` → 会话对象路径。
    async fn session_path(&self, id: &str) -> Result<ObjectPath<'static>> {
        let owned: OwnedObjectPath = self
            .manager()
            .await
            .map_err(dbus_err)?
            .get_session(id)
            .await
            .map_err(|e| AgentShellError::DBus(format!("GetSession({id}): {e}")))?;
        Ok(owned.into_inner())
    }

    /// Session 对象属性 → core [`SessionInfo`]。
    ///
    /// `list_sessions` 与 `get_session` 共用此路径：ListSessions 元组只保证
    /// id/uid/name/seat，display/state/tty 等一律从 Session 接口属性读取，
    /// 避免两处填充逻辑分叉。
    async fn read_session(&self, path: ObjectPath<'_>) -> Result<SessionInfo> {
        let session = self.session(path).await.map_err(dbus_err)?;
        let (uid, _) = session.user().await.map_err(dbus_err)?;
        let empty_to_none = |s: String| if s.is_empty() { None } else { Some(s) };
        Ok(SessionInfo {
            id: session.id().await.map_err(dbus_err)?,
            uid,
            user_name: session.name().await.map_err(dbus_err)?,
            seat: session.seat().await.map_err(dbus_err)?.0,
            display: session.display().await.map_err(dbus_err)?,
            remote: session.remote().await.map_err(dbus_err)?,
            remote_host: empty_to_none(session.remote_host().await.map_err(dbus_err)?),
            state: session.state().await.map_err(dbus_err)?,
            tty: empty_to_none(session.tty().await.map_err(dbus_err)?),
        })
    }
}

/// `CanReboot()` 等返回的 "yes"/"no"/"challenge" → 布尔。
/// 仅 "yes" 视为允许；"challenge" 需要 polkit 交互，按不可用处理。
fn can_bool(answer: &str) -> bool {
    answer == "yes"
}

#[async_trait]
impl DesktopComponent for LogindComponent {
    fn name(&self) -> &'static str {
        "logind"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::SessionManager
    }

    fn is_available(&self) -> bool {
        true
    }

    /// doctor 健康检查：以 Manager 接口实际可调为准。
    async fn health(&self) -> ComponentHealth {
        let Ok(manager) = self.manager().await else {
            return ComponentHealth::Degraded("org.freedesktop.login1 unreachable".to_string());
        };
        match manager.list_sessions().await {
            Ok(_) => ComponentHealth::Healthy,
            Err(e) => ComponentHealth::Degraded(format!("org.freedesktop.login1 unreachable: {e}")),
        }
    }
}

#[async_trait]
impl SessionManagerComponent for LogindComponent {
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let sessions = self
            .manager()
            .await
            .map_err(dbus_err)?
            .list_sessions()
            .await
            .map_err(|e| AgentShellError::DBus(format!("ListSessions: {e}")))?;
        let mut infos = Vec::with_capacity(sessions.len());
        for (_, _, _, _, path) in sessions {
            infos.push(self.read_session(path.into_inner()).await?);
        }
        Ok(infos)
    }

    async fn get_session(&self, id: &str) -> Result<SessionInfo> {
        self.read_session(self.session_path(id).await?).await
    }

    async fn lock_session(&self, id: &str) -> Result<()> {
        let path = self.session_path(id).await?;
        self.session(path)
            .await
            .map_err(dbus_err)?
            .lock()
            .await
            .map_err(|e| AgentShellError::DBus(format!("Session.Lock({id}): {e}")))
    }

    async fn unlock_session(&self, id: &str) -> Result<()> {
        let path = self.session_path(id).await?;
        self.session(path)
            .await
            .map_err(dbus_err)?
            .unlock()
            .await
            .map_err(|e| AgentShellError::DBus(format!("Session.Unlock({id}): {e}")))
    }

    async fn can_reboot(&self) -> Result<bool> {
        let answer = self
            .manager()
            .await
            .map_err(dbus_err)?
            .can_reboot()
            .await
            .map_err(|e| AgentShellError::DBus(format!("CanReboot: {e}")))?;
        Ok(can_bool(&answer))
    }

    async fn reboot(&self) -> Result<()> {
        // interactive=false：非幂等操作，不弹 polkit 交互、不重试（design/11 §19.4）。
        self.manager()
            .await
            .map_err(dbus_err)?
            .reboot(false)
            .await
            .map_err(|e| AgentShellError::DBus(format!("Reboot: {e}")))
    }

    async fn can_poweroff(&self) -> Result<bool> {
        let answer = self
            .manager()
            .await
            .map_err(dbus_err)?
            .can_power_off()
            .await
            .map_err(|e| AgentShellError::DBus(format!("CanPowerOff: {e}")))?;
        Ok(can_bool(&answer))
    }

    async fn poweroff(&self) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .power_off(false)
            .await
            .map_err(|e| AgentShellError::DBus(format!("PowerOff: {e}")))
    }
}
