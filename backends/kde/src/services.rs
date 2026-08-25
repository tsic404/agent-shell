//! KDE 服务封装实例与 DePriorityRouter 装配（§21.6 内部路由）。

use agent_shell_core::component::{
    AppearanceComponent, ComponentHealth, ComponentType, DesktopComponent, PowerComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{AppInfo, AppTarget, BatteryState, ColorScheme};
use agent_shell_power::{service_exists, UPowerComponent};
use async_trait::async_trait;
use zbus::proxy;

pub const KDE_POWER_SERVICE: &str = "org.kde.Solid.PowerManagement";
pub const KDE_SCREENSAVER_BUS: &str = "org.kde.screensaver";

/// org.kde.Solid.PowerManagement 代理（session bus，powerdevil）。
#[proxy(
    interface = "org.kde.Solid.PowerManagement",
    default_service = "org.kde.Solid.PowerManagement",
    default_path = "/org/kde/Solid/PowerManagement"
)]
trait SolidPowerManagement {
    fn suspend(&self) -> zbus::Result<()>;
    fn hibernate(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn battery_charge_percent(&self) -> zbus::Result<i32>;
}

/// Actions.SuspendSession 子接口（suspendToRam / suspendToDisk）。
#[proxy(
    interface = "org.kde.Solid.PowerManagement.Actions.SuspendSession",
    default_service = "org.kde.Solid.PowerManagement",
    default_path = "/org/kde/Solid/PowerManagement/Actions/SuspendSession"
)]
trait SuspendSessionActions {
    fn suspend_to_ram(&self) -> zbus::Result<()>;
    fn suspend_to_disk(&self) -> zbus::Result<()>;
}

/// kscreenlocker 锁屏代理。
#[proxy(
    interface = "org.kde.screensaver",
    default_service = "org.kde.screensaver",
    default_path = "/ScreenSaver"
)]
trait KScreenLocker {
    fn lock(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn is_locked(&self) -> zbus::Result<bool>;
}

/// KDE 电源封装：powerdevil 优先，锁屏走 kscreenlocker，
/// 挂起/休眠优先 powerdevil，关机回退 login1（polkit L4）。
pub struct KdePowerDevil {
    conn: zbus::Connection,
    login1: UPowerComponent,
    available: bool,
}

impl KdePowerDevil {
    /// DePriorityRouter 入口：探测 `org.kde.Solid.PowerManagement`；
    /// 未命中返回 None → 上层装配 `UPowerComponent` 公共降级。
    pub async fn try_new() -> Result<Option<Self>> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
        if !service_exists(&conn, KDE_POWER_SERVICE).await? {
            return Ok(None);
        }
        let login1 = UPowerComponent::new().await?;
        Ok(Some(Self {
            conn,
            login1,
            available: true,
        }))
    }

    async fn solid(&self) -> Result<SolidPowerManagementProxy<'static>> {
        SolidPowerManagementProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("powerdevil proxy: {e}")))
    }

    async fn suspend_actions(&self) -> Result<SuspendSessionActionsProxy<'static>> {
        SuspendSessionActionsProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("SuspendSession proxy: {e}")))
    }

    async fn locker(&self) -> Result<KScreenLockerProxy<'static>> {
        KScreenLockerProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("kscreenlocker proxy: {e}")))
    }
}

#[async_trait]
impl DesktopComponent for KdePowerDevil {
    fn name(&self) -> &'static str {
        "kde-powerdevil"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Power
    }

    fn is_available(&self) -> bool {
        self.available
    }

    async fn health(&self) -> ComponentHealth {
        let solid = match self.solid().await {
            Ok(s_) => s_,
            Err(e) => return ComponentHealth::Degraded(format!("powerdevil proxy: {e}")),
        };
        match solid.battery_charge_percent().await {
            Ok(_) => ComponentHealth::Healthy,
            Err(e) => ComponentHealth::Degraded(format!("powerdevil: {e}")),
        }
    }
}

#[async_trait]
impl PowerComponent for KdePowerDevil {
    /// L4：调用方必须先确认；kscreenlocker 锁屏 + polkit。
    async fn lock_screen(&self) -> Result<()> {
        self.locker()
            .await?
            .lock()
            .await
            .map_err(|e| AgentShellError::DBus(format!("kscreenlocker Lock: {e}")))
    }

    async fn logout(&self) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "KDE logout via ksmserver requestShutDown; pending session-manager wiring".into(),
        ))
    }

    async fn suspend(&self) -> Result<()> {
        // powerdevil SuspendSession.suspendToRam 优先；失败回退 login1。
        match self.suspend_actions().await?.suspend_to_ram().await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::debug!(error = %e, "powerdevil suspendToRam failed; falling back to login1");
                self.login1.suspend().await
            }
        }
    }

    async fn hibernate(&self) -> Result<()> {
        match self.suspend_actions().await?.suspend_to_disk().await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::debug!(error = %e, "powerdevil suspendToDisk failed; falling back to login1");
                self.login1.hibernate().await
            }
        }
    }

    async fn power_off(&self) -> Result<()> {
        // 关机统一 login1（interactive=true 触发 polkit 授权，L4）。
        self.login1.power_off().await
    }

    async fn get_battery_status(&self) -> Result<BatteryState> {
        // powerdevil 只给百分比整数；完整状态回退 UPower DisplayDevice。
        let solid = match self.solid().await {
            Ok(s_) => s_,
            Err(_) => return self.login1.get_battery_status().await,
        };
        match solid.battery_charge_percent().await {
            Ok(pct) => {
                // 固化假设：powerdevil 已报出电量百分比 ⇒ 本机存在电池，
                // UPower 补充读数失败时兜底 is_present=true、其余字段未知。
                let base = self
                    .login1
                    .get_battery_status()
                    .await
                    .unwrap_or(BatteryState {
                        percentage: pct as f64,
                        charging: false,
                        time_to_empty: None,
                        time_to_full: None,
                        is_present: true,
                    });
                Ok(BatteryState {
                    percentage: pct as f64,
                    ..base
                })
            }
            Err(_) => self.login1.get_battery_status().await,
        }
    }
}

/// KDE 外观封装：plasma-apply-* CLI 优先，portal 降级由上层路由负责。
pub struct KdeAppearance;

impl KdeAppearance {
    async fn run_cli(bin: &str, args: &[&str]) -> Result<()> {
        let out = tokio::process::Command::new(bin)
            .args(args)
            .output()
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("{bin}: {e}")))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(AgentShellError::BackendUnavailable(format!(
                "{bin} {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }
}

#[async_trait]
impl DesktopComponent for KdeAppearance {
    fn name(&self) -> &'static str {
        "kde-appearance"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Appearance
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        match tokio::process::Command::new("plasma-apply-colorscheme")
            .arg("--list-schemes")
            .output()
            .await
        {
            Ok(o) if o.status.success() => ComponentHealth::Healthy,
            _ => ComponentHealth::Degraded("plasma-apply-colorscheme unavailable".into()),
        }
    }
}

#[async_trait]
impl AppearanceComponent for KdeAppearance {
    async fn set_wallpaper(&self, path: &str) -> Result<()> {
        Self::run_cli("plasma-apply-wallpaperimage", &[path]).await
    }

    async fn get_color_scheme(&self) -> Result<ColorScheme> {
        let out = tokio::process::Command::new("plasma-apply-colorscheme")
            .arg("--list-schemes")
            .output()
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("{e}")))?;
        let text = String::from_utf8_lossy(&out.stdout);
        // 当前生效方案以 "* " 标记输出。
        for line in text.lines() {
            let name = line.trim_start_matches(['*', ' ']).trim();
            if line.trim_start().starts_with('*') {
                if name.to_lowercase().contains("dark") {
                    return Ok(ColorScheme::Dark);
                }
                if name.to_lowercase().contains("light") {
                    return Ok(ColorScheme::Light);
                }
            }
        }
        Err(AgentShellError::Other(
            format!("cannot infer current scheme from plasma-apply-colorscheme output:\n{text}")
                .into(),
        ))
    }

    async fn set_color_scheme(&self, scheme: ColorScheme) -> Result<()> {
        let name = match scheme {
            ColorScheme::Dark | ColorScheme::NoPreference => "BreezeDark",
            ColorScheme::Light => "BreezeLight",
        };
        Self::run_cli("plasma-apply-colorscheme", &[name]).await
    }
}

/// KDE 启动器：公共 `.desktop` + gio 路径已覆盖（§21.3 跨 DE 行），
/// 此类型作为 backend 装配点的显式命名实现，能力快照据此记录 app_launch/app_list=true。
pub struct KdeLauncher(pub agent_shell_launcher::DesktopFileLauncher);

impl Default for KdeLauncher {
    fn default() -> Self {
        Self(agent_shell_launcher::DesktopFileLauncher::new())
    }
}

#[async_trait]
impl DesktopComponent for KdeLauncher {
    fn name(&self) -> &'static str {
        "kde-launcher"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Launcher
    }

    fn is_available(&self) -> bool {
        self.0.is_available()
    }

    async fn health(&self) -> ComponentHealth {
        self.0.health().await
    }
}

#[async_trait]
impl agent_shell_core::component::LauncherComponent for KdeLauncher {
    async fn list_installed_apps(&self) -> Result<Vec<AppInfo>> {
        self.0.list_installed_apps().await
    }

    async fn launch_app(&self, app: &AppTarget) -> Result<()> {
        self.0.launch_app(app).await
    }

    async fn launch_uri(&self, uri: &str) -> Result<()> {
        self.0.launch_uri(uri).await
    }
}
