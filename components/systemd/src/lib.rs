//! systemd init 系统组件（设计文档 §21.13「systemd 服务管理」）。
//!
//! [`SystemdComponent`] 封装 system bus 上的 `org.freedesktop.systemd1.Manager`
//! 接口，实现 core 的 [`SystemComponent`] 与 [`DesktopComponent`]。
//! 不依赖 DE——TTY 后端装配时的必选组件之一。
//!
//! zbus system-bus 连接在构造时建立并持有；D-Bus 错误经共享 helper
//! [`agent_shell_core::error::dbus_error`] 归一化（design/11 §19）：权限类
//! 错误名 → [`AgentShellError::Permission`]，其余 → [`AgentShellError::DBus`]。

use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, SystemComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::SystemdUnit;
use agent_shell_core::types::UnitStatus;
use async_trait::async_trait;
use std::fmt::Display;
use tracing::debug;
use zbus::zvariant::OwnedObjectPath;

/// D-Bus 错误归一化：委托共享 helper [`agent_shell_core::error::dbus_error`]，
/// 权限类错误名 → [`AgentShellError::Permission`]，其余 → [`AgentShellError::DBus`]。
fn dbus_err<E: Display>(e: E) -> AgentShellError {
    agent_shell_core::error::dbus_error(e)
}

/// `org.freedesktop.systemd1.Manager` 的 zbus proxy。
///
/// 方法签名与 D-Bus 接口逐字对应（§21.13 接口表）：
/// https://www.freedesktop.org/wiki/Software/systemd/dbus/
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait Systemd1Manager {
    // → a(ssssssouso)：name, description, load_state, active_state,
    //   sub_state, follow_unit, object_path, job_id, job_type, job_object_path
    #[allow(clippy::type_complexity)] // D-Bus 签名原样，无法拆分
    fn list_units(
        &self,
    ) -> zbus::Result<
        Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            OwnedObjectPath,
            u32,
            String,
            OwnedObjectPath,
        )>,
    >;

    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn reload_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    // (asbb) → (ba(sss))
    #[allow(clippy::type_complexity)]
    fn enable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> zbus::Result<(bool, Vec<(String, String, String)>)>;
    // (asb) → a(sss)
    fn disable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
    ) -> zbus::Result<Vec<(String, String, String)>>;

    fn reset_failed_unit(&self, name: &str) -> zbus::Result<()>;
    fn reload(&self) -> zbus::Result<()>;
}

/// systemd init 系统组件实例。
pub struct SystemdComponent {
    conn: zbus::Connection,
}

impl SystemdComponent {
    /// D-Bus 方法调用超时：全局默认 5s（design/11 §19.4）。
    const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// 连接 system bus（systemd 是 Linux 标配；连接失败由装配层降级）。
    pub async fn connect() -> Result<Self> {
        let conn = zbus::connection::Builder::system()
            .map_err(dbus_err)?
            .method_timeout(Self::CALL_TIMEOUT)
            .build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("connect system bus: {e}")))?;
        Ok(Self { conn })
    }

    async fn manager(&self) -> zbus::Result<Systemd1ManagerProxy<'_>> {
        Systemd1ManagerProxy::new(&self.conn).await
    }

    /// 供集成测试验证状态映射域。
    #[doc(hidden)]
    pub fn map_active_state(state: &str) -> UnitStatus {
        match state {
            "active" => UnitStatus::Active,
            "reloading" => UnitStatus::Reloading,
            "inactive" => UnitStatus::Inactive,
            "failed" => UnitStatus::Failed,
            "activating" => UnitStatus::Activating,
            "deactivating" => UnitStatus::Deactivating,
            _ => UnitStatus::Unknown,
        }
    }
    /// ListUnits 元组 `(name, description, load_state, active_state, sub_state,
    /// follow_unit, object_path, job_id, job_type, job_object_path)`
    /// → core [`SystemdUnit`]。
    ///
    /// `enabled`/`main_pid` 不在 ListUnits 元组内：`enabled` 需
    /// GetUnitFileState/ListUnitFiles，保守置 false；`main_pid` 同理为 None。
    fn parse_unit(
        unit: (
            String,
            String,
            String,
            String,
            String,
            String,
            OwnedObjectPath,
            u32,
            String,
            OwnedObjectPath,
        ),
    ) -> SystemdUnit {
        let (
            name,
            description,
            load_state,
            active_state,
            _sub_state,
            _follow_unit,
            _obj_path,
            _job_id,
            _job_type,
            _job_object_path,
        ) = unit;
        debug!(unit = %name, active = %active_state, "parsed unit");
        SystemdUnit {
            name,
            description,
            status: Self::map_active_state(&active_state),
            enabled: false,
            main_pid: None,
            load_state,
        }
    }
}

#[async_trait]
impl DesktopComponent for SystemdComponent {
    fn name(&self) -> &'static str {
        "systemd"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::InitSystem
    }

    /// 同步可用性：system bus 连接已在 [`Self::connect`] 建立即视为可用。
    fn is_available(&self) -> bool {
        true
    }

    /// doctor 健康检查：以 Manager 接口实际可调为准。
    async fn health(&self) -> ComponentHealth {
        let Ok(manager) = self.manager().await else {
            return ComponentHealth::Degraded("org.freedesktop.systemd1 unreachable".to_string());
        };
        match manager.list_units().await {
            Ok(_) => ComponentHealth::Healthy,
            Err(e) => {
                ComponentHealth::Degraded(format!("org.freedesktop.systemd1 unreachable: {e}"))
            }
        }
    }
}

#[async_trait]
impl SystemComponent for SystemdComponent {
    async fn daemon_reload(&self) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .reload()
            .await
            .map_err(|e| dbus_err(format!("Reload: {e}")))
    }

    async fn list_units(&self) -> Result<Vec<SystemdUnit>> {
        let units = self
            .manager()
            .await
            .map_err(dbus_err)?
            .list_units()
            .await
            .map_err(|e| dbus_err(format!("ListUnits: {e}")))?;
        Ok(units.into_iter().map(Self::parse_unit).collect())
    }

    async fn start_unit(&self, name: &str) -> Result<()> {
        // mode "replace"：排队替换既有 job（§21.13 默认模式）。
        self.manager()
            .await
            .map_err(dbus_err)?
            .start_unit(name, "replace")
            .await
            .map(|_| ())
            .map_err(|e| dbus_err(format!("StartUnit({name}): {e}")))
    }

    async fn stop_unit(&self, name: &str) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .stop_unit(name, "replace")
            .await
            .map(|_| ())
            .map_err(|e| dbus_err(format!("StopUnit({name}): {e}")))
    }

    async fn enable_unit(&self, name: &str) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .enable_unit_files(&[name], false, false)
            .await
            .map(|_| ())
            .map_err(|e| dbus_err(format!("EnableUnitFiles({name}): {e}")))
    }

    async fn disable_unit(&self, name: &str) -> Result<()> {
        self.manager()
            .await
            .map_err(dbus_err)?
            .disable_unit_files(&[name], false)
            .await
            .map(|_| ())
            .map_err(|e| dbus_err(format!("DisableUnitFiles({name}): {e}")))
    }

    async fn unit_status(&self, name: &str) -> Result<UnitStatus> {
        let units = self.list_units().await?;
        Ok(units
            .into_iter()
            .find(|u| u.name == name)
            .map(|u| u.status)
            .unwrap_or(UnitStatus::Unknown))
    }
}

/// 装配系统服务组件族（systemd + logind），返回 `(init_system, session_manager)`。
///
/// 所有 backend 共享的公共装配逻辑（§3.3 装配矩阵「系统服务」行）：
/// systemd 与 logind 是唯一在所有 Linux 环境（含 TTY）均「必选」的组件族。
/// 探测失败不 panic——返回 `None` 对应 slot，由 doctor 报告 Degraded/Unavailable。
pub async fn assemble_system_services() -> (
    Option<Box<dyn agent_shell_core::component::SystemComponent>>,
    Option<Box<dyn agent_shell_core::component::SessionManagerComponent>>,
) {
    let init_system = match SystemdComponent::connect().await {
        Ok(s) => Some(Box::new(s) as Box<dyn agent_shell_core::component::SystemComponent>),
        Err(e) => {
            tracing::warn!(error = %e, "systemd assembly failed");
            None
        }
    };
    let session_manager = match agent_shell_logind::LogindComponent::connect().await {
        Ok(s) => Some(Box::new(s) as Box<dyn agent_shell_core::component::SessionManagerComponent>),
        Err(e) => {
            tracing::warn!(error = %e, "logind assembly failed");
            None
        }
    };
    (init_system, session_manager)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbus_err_maps_permission_error_names() {
        for msg in [
            "Reload: org.freedesktop.DBus.Error.AccessDenied: Permission denied",
            "StartUnit(x): org.freedesktop.DBus.Error.InteractiveAuthorizationRequired: interactive auth required",
            "org.freedesktop.DBus.Error.AuthenticationRequisite: Authentication is required",
            "org.freedesktop.DBus.Error.UnixFD.AccessDenied: fd passing denied",
        ] {
            assert!(
                matches!(dbus_err(msg), AgentShellError::Permission(_)),
                "expected Permission for {msg:?}"
            );
        }
    }

    #[test]
    fn dbus_err_keeps_other_errors_as_dbus() {
        for msg in [
            "Reload: failed",
            "StartUnit(x): Access denied as the requested operation requires interactive authentication",
        ] {
            assert!(
                matches!(dbus_err(msg), AgentShellError::DBus(_)),
                "expected DBus for {msg:?}"
            );
        }
    }
}
