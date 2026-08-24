//! 电源组件（设计文档 §21.3 电源与会话、§21.4 PowerComponent）。
//!
//! 公共封装：`org.freedesktop.login1`（system bus，挂起/休眠/关机）
//! 与 `org.freedesktop.UPower`（system bus，电池状态）。
//!
//! DE 专有封装（KdePowerDevil / DdePower / GnomePower）位于 `backends/`，
//! 通过 [`DePriorityRouter`] 按「DE 封装优先 → 公共降级」路由。
//!
//! 安全分级（§21.21）：`lock_screen`/`suspend`/`hibernate`/`power_off` 属 L4——
//! 调用方必须先取得用户确认；D-Bus 层再经 polkit 授权
//! （login1 `interactive=true` 触发 polkit 交互授权）。

pub mod router;
pub use router::{DDE_POWER_NAMES, probe_first_existing, service_exists};
use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, PowerComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::BatteryState;
use async_trait::async_trait;
use zbus::proxy;

/// login1 Manager 代理（system bus）。
#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login1Manager {
    /// `interactive=true` 时触发 polkit 交互授权（L4 要求）。
    fn suspend(&self, interactive: bool) -> zbus::Result<()>;
    fn hibernate(&self, interactive: bool) -> zbus::Result<()>;
    fn power_off(&self, interactive: bool) -> zbus::Result<()>;
    fn can_suspend(&self) -> zbus::Result<String>;
    fn can_hibernate(&self) -> zbus::Result<String>;
}

/// UPower DisplayDevice 代理（聚合电池；台式机 `is_present=false`）。
#[proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait UpowerDevice {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<f64>;

    /// 0 unknown, 1 charging, 2 discharging, 3 empty, 4 fully charged,
    /// 5 pending charge, 6 pending discharge
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn is_present(&self) -> zbus::Result<bool>;

    /// 秒；无估计时为 0。
    #[zbus(property)]
    fn time_to_empty(&self) -> zbus::Result<i64>;

    #[zbus(property)]
    fn time_to_full(&self) -> zbus::Result<i64>;
}

/// 公共电源组件：login1 + UPower（跨 DE 保底路径）。
pub struct UPowerComponent {
    conn: zbus::Connection,
    available: bool,
}

impl UPowerComponent {
    /// 连接 system bus。bus 不可达时返回可用性为 false 的实例
    /// （health() 报 Degraded，由上层路由决定是否致命）。
    pub async fn new() -> Result<Self> {
        let conn = zbus::Connection::system()
            .await
            .map_err(|e| AgentShellError::DBus(format!("system bus: {e}")))?;
        Ok(Self {
            conn,
            available: true,
        })
    }

    async fn login1(&self) -> zbus::Result<Login1ManagerProxy<'static>> {
        Login1ManagerProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
    }

    async fn upower(&self) -> zbus::Result<UpowerDeviceProxy<'static>> {
        UpowerDeviceProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
    }
}

#[async_trait]
impl DesktopComponent for UPowerComponent {
    fn name(&self) -> &'static str {
        "upower-power"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Power
    }

    fn is_available(&self) -> bool {
        self.available
    }

    async fn health(&self) -> ComponentHealth {
        let up = match self.upower().await {
            Ok(u) => u,
            Err(e) => return ComponentHealth::Degraded(format!("UPower proxy: {e}")),
        };
        if up.percentage().await.is_ok() {
            return ComponentHealth::Healthy;
        }
        // 无电池设备是正常状态（台式机），login1 可达即 Healthy。
        match self.login1().await {
            Ok(l1) => match l1.can_suspend().await {
                Ok(_) => ComponentHealth::Degraded("UPower unreachable; login1 OK".into()),
                Err(e) => ComponentHealth::Degraded(format!("login1/UPower unreachable: {e}")),
            },
            Err(e) => ComponentHealth::Degraded(format!("login1/UPower unreachable: {e}")),
        }
    }
}

#[async_trait]
impl PowerComponent for UPowerComponent {
    async fn lock_screen(&self) -> Result<()> {
        Err(AgentShellError::BackendUnavailable(
            "lock_screen requires a DE wrapper (KDE/DDE/GNOME) or portal path; \
             generic UPower/login1 has no lock interface"
                .into(),
        ))
    }

    async fn logout(&self) -> Result<()> {
        Err(AgentShellError::BackendUnavailable(
            "logout requires a session manager (logind KillSession via DE wrapper)".into(),
        ))
    }

    async fn suspend(&self) -> Result<()> {
        self.login1()
            .await
            .map_err(|e| AgentShellError::DBus(format!("login1 proxy: {e}")))?
            .suspend(true)
            .await
            .map_err(|e| AgentShellError::DBus(format!("login1 Suspend: {e}")))
    }

    async fn hibernate(&self) -> Result<()> {
        self.login1()
            .await
            .map_err(|e| AgentShellError::DBus(format!("login1 proxy: {e}")))?
            .hibernate(true)
            .await
            .map_err(|e| AgentShellError::DBus(format!("login1 Hibernate: {e}")))
    }

    async fn power_off(&self) -> Result<()> {
        self.login1()
            .await
            .map_err(|e| AgentShellError::DBus(format!("login1 proxy: {e}")))?
            .power_off(true)
            .await
            .map_err(|e| AgentShellError::DBus(format!("login1 PowerOff: {e}")))
    }

    async fn get_battery_status(&self) -> Result<BatteryState> {
        let dev = self.upower().await.map_err(|e| AgentShellError::DBus(format!("UPower proxy: {e}")))?;
        let is_present = dev.is_present().await.map_errdbus("IsPresent")?;
        if !is_present {
            return Ok(BatteryState {
                percentage: 0.0,
                charging: false,
                time_to_empty: None,
                time_to_full: None,
                is_present: false,
            });
        }
        let state = dev.state().await.map_errdbus("State")?;
        let tte = dev.time_to_empty().await.map_errdbus("TimeToEmpty")?;
        let ttf = dev.time_to_full().await.map_errdbus("TimeToFull")?;
        Ok(BatteryState {
            percentage: dev.percentage().await.map_errdbus("Percentage")?,
            charging: state == 1 || state == 5,
            time_to_empty: (tte > 0).then_some(tte as u32),
            time_to_full: (ttf > 0).then_some(ttf as u32),
            is_present: true,
        })
    }
}

/// 小工具：把 zbus 属性错误统一映射为 DBus 变体。
trait MapErrDbus<T> {
    fn map_errdbus(self, what: &'static str) -> Result<T>;
}

impl<T> MapErrDbus<T> for zbus::Result<T> {
    fn map_errdbus(self, what: &'static str) -> Result<T> {
        self.map_err(|e| AgentShellError::DBus(format!("UPower {what}: {e}")))
    }
}
