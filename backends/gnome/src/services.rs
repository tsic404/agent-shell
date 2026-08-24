//! GNOME 服务封装实例与 DePriorityRouter 装配（§21.6 内部路由）。

use agent_shell_core::component::{
    AppearanceComponent, ComponentHealth, ComponentType, DesktopComponent, PowerComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{AppInfo, AppTarget, BatteryState, ColorScheme};
use agent_shell_power::{service_exists, UPowerComponent};
use async_trait::async_trait;
use zbus::proxy;

pub const GNOME_POWER_SERVICE: &str = "org.gnome.SettingsDaemon.Power";
pub const GNOME_SCREENSAVER: &str = "org.gnome.ScreenSaver";

/// org.gnome.SettingsDaemon.Power（bus name = 接口名，path /org/gnome/SettingsDaemon）。
#[proxy(
    interface = "org.gnome.SettingsDaemon.Power",
    default_service = "org.gnome.SettingsDaemon.Power",
    default_path = "/org/gnome/SettingsDaemon"
)]
trait GsdPower {
    fn set_power_save_mode(&self, enabled: bool) -> zbus::Result<()>;
}

/// org.gnome.ScreenSaver。
#[proxy(
    interface = "org.gnome.ScreenSaver",
    default_service = "org.gnome.ScreenSaver",
    default_path = "/org/gnome/ScreenSaver"
)]
trait ScreenSaver {
    fn lock(&self) -> zbus::Result<()>;
    fn get_active(&self) -> zbus::Result<bool>;
}

/// GNOME 电源封装：gsd.Power 命中标记 + login1 承载 L4 动作
/// （suspend/power_off interactive=true → polkit）+ UPower 电池。
pub struct GnomePower {
    conn: zbus::Connection,
    gsd_available: bool,
    login1: UPowerComponent,
}

impl GnomePower {
    /// DePriorityRouter 入口：探测 `org.gnome.SettingsDaemon.Power`；
    /// 未命中返回 None → 上层装配 `UPowerComponent`。
    pub async fn try_new() -> Result<Option<Self>> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
        let gsd_available = service_exists(&conn, GNOME_POWER_SERVICE).await?;
        if !gsd_available {
            return Ok(None);
        }
        let login1 = UPowerComponent::new().await?;
        Ok(Some(Self {
            conn,
            gsd_available,
            login1,
        }))
    }

    async fn screensaver(&self) -> Result<ScreenSaverProxy<'static>> {
        ScreenSaverProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("ScreenSaver proxy: {e}")))
    }
}

#[async_trait]
impl DesktopComponent for GnomePower {
    fn name(&self) -> &'static str {
        "gnome-gsd-power"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Power
    }

    fn is_available(&self) -> bool {
        self.gsd_available
    }

    async fn health(&self) -> ComponentHealth {
        match service_exists(&self.conn, GNOME_POWER_SERVICE).await {
            Ok(true) => ComponentHealth::Healthy,
            Ok(false) => ComponentHealth::Degraded("gsd.Power left the bus".into()),
            Err(e) => ComponentHealth::Degraded(format!("{e}")),
        }
    }
}

#[async_trait]
impl PowerComponent for GnomePower {
    /// L4：调用方必须先确认。org.gnome.ScreenSaver.Lock()。
    async fn lock_screen(&self) -> Result<()> {
        self.screensaver()
            .await?
            .lock()
            .await
            .map_err(|e| AgentShellError::DBus(format!("ScreenSaver Lock: {e}")))
    }

    async fn logout(&self) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "GNOME logout via org.gnome.SessionManager Logout; pending session-manager wiring"
                .into(),
        ))
    }

    /// L4：确认后经 login1 interactive=true 触发 polkit。
    async fn suspend(&self) -> Result<()> {
        self.login1.suspend().await
    }

    async fn hibernate(&self) -> Result<()> {
        self.login1.hibernate().await
    }

    async fn power_off(&self) -> Result<()> {
        self.login1.power_off().await
    }

    async fn get_battery_status(&self) -> Result<BatteryState> {
        self.login1.get_battery_status().await
    }
}

/// GNOME 外观封装：gsettings 优先，portal 降级由上层路由。
pub struct GnomeAppearance;

fn scheme_to_gsettings(scheme: ColorScheme) -> &'static str {
    match scheme {
        ColorScheme::Dark => "prefer-dark",
        ColorScheme::Light | ColorScheme::NoPreference => "default",
    }
}

#[async_trait]
impl DesktopComponent for GnomeAppearance {
    fn name(&self) -> &'static str {
        "gnome-appearance"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Appearance
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        let out = tokio::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "color-scheme"])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => ComponentHealth::Healthy,
            _ => ComponentHealth::Degraded("gsettings schema unavailable".into()),
        }
    }
}

#[async_trait]
impl AppearanceComponent for GnomeAppearance {
    /// gsettings picture-uri；Wayland GNOME 需同时写 dark 变体
    /// （picture-uri-dark），否则深色模式下壁纸不生效。
    async fn set_wallpaper(&self, path: &str) -> Result<()> {
        let uri = if path.contains("://") {
            path.to_string()
        } else {
            let abs = std::fs::canonicalize(path)
                .map_err(|e| AgentShellError::Other(format!("{path}: {e}").into()))?;
            format!("file://{}", abs.display())
        };
        for key in ["picture-uri", "picture-uri-dark"] {
            let out = tokio::process::Command::new("gsettings")
                .args([
                    "set",
                    "org.gnome.desktop.background",
                    key,
                    uri.as_str(),
                ])
                .output()
                .await
                .map_err(|e| AgentShellError::Other(format!("gsettings: {e}").into()))?;
            if !out.status.success() {
                return Err(AgentShellError::BackendUnavailable(format!(
                    "gsettings set {key}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        Ok(())
    }

    async fn get_color_scheme(&self) -> Result<ColorScheme> {
        let out = tokio::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "color-scheme"])
            .output()
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("gsettings: {e}")))?;
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() {
            return Err(AgentShellError::BackendUnavailable(format!(
                "gsettings get color-scheme: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(match value.trim_matches('\'') {
            "prefer-dark" => ColorScheme::Dark,
            "prefer-light" => ColorScheme::Light,
            _ => ColorScheme::NoPreference,
        })
    }

    async fn set_color_scheme(&self, scheme: ColorScheme) -> Result<()> {
        let out = tokio::process::Command::new("gsettings")
            .args([
                "set",
                "org.gnome.desktop.interface",
                "color-scheme",
                scheme_to_gsettings(scheme),
            ])
            .output()
            .await
            .map_err(|e| AgentShellError::Other(format!("gsettings: {e}").into()))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(AgentShellError::BackendUnavailable(format!(
                "gsettings set color-scheme: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }
}

/// GNOME 启动器：gio launch + portal（公共路径已覆盖），显式命名装配点。
pub struct GnomeLauncher(pub agent_shell_launcher::DesktopFileLauncher);

impl Default for GnomeLauncher {
    fn default() -> Self {
        Self(agent_shell_launcher::DesktopFileLauncher::new())
    }
}

#[async_trait]
impl DesktopComponent for GnomeLauncher {
    fn name(&self) -> &'static str {
        "gnome-launcher"
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
impl agent_shell_core::component::LauncherComponent for GnomeLauncher {
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
