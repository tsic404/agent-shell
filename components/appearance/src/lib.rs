//! 外观组件（设计文档 §21.3 外观与壁纸、§21.4 AppearanceComponent）。
//!
//! 公共降级路径：
//! - 壁纸：portal `org.freedesktop.portal.Wallpaper.SetWallpaperURI`
//! - 配色读取：portal `org.freedesktop.portal.Settings.ReadOne`
//!   （`org.freedesktop.appearance` → `color-scheme`: 0 no-preference / 1 dark / 2 light）
//! - 配色写入：无跨 DE portal，按 DE 用 gsettings / plasma-apply-colorscheme /
//!   org.deepin.dde.Appearance1（backends/ 内 DE 封装负责）；
//!   GNOME 环境下公共路径可直接 gsettings 写 `org.gnome.desktop.interface color-scheme`。

use agent_shell_core::component::{
    AppearanceComponent, ComponentHealth, ComponentType, DesktopComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::ColorScheme;
use async_trait::async_trait;

pub const BUS_NAME: &str = "org.freedesktop.portal.Desktop";

/// portal 外观组件（Settings 读 + Wallpaper 写）。
pub struct PortalAppearance {
    conn: zbus::Connection,
    available: bool,
}

impl PortalAppearance {
    pub async fn new() -> Result<Self> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))?;
        Ok(Self {
            conn,
            available: true,
        })
    }

    async fn portal(&self, iface: &'static str) -> Result<zbus::Proxy<'static>> {
        zbus::Proxy::new_owned(
            self.conn.clone(),
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            iface,
        )
        .await
        .map_err(|e| AgentShellError::DBus(format!("portal proxy {iface}: {e}")))
    }

    /// ReadOne 返回 Variant 包裹的 u32。
    async fn read_color_scheme(&self) -> Result<ColorScheme> {
        let p = self.portal("org.freedesktop.portal.Settings").await?;
        let reply: zbus::zvariant::OwnedValue = p
            .call(
                "ReadOne",
                &("org.freedesktop.appearance", "color-scheme"),
            )
            .await
            .map_err(|e| AgentShellError::DBus(format!("Settings.ReadOne: {e}")))?;
        let v = u32::try_from(reply)
            .map_err(|e| AgentShellError::DBus(format!("color-scheme decode: {e}")))?;
        Ok(match v {
            1 => ColorScheme::Dark,
            2 => ColorScheme::Light,
            _ => ColorScheme::NoPreference,
        })
    }
}

#[async_trait]
impl DesktopComponent for PortalAppearance {
    fn name(&self) -> &'static str {
        "portal-appearance"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Appearance
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
            .name_has_owner(BUS_NAME.try_into().expect("valid bus name"))
            .await
        {
            Ok(true) => match self.read_color_scheme().await {
                Ok(_) => ComponentHealth::Healthy,
                Err(e) => ComponentHealth::Degraded(format!("Settings.ReadOne failed: {e}")),
            },
            Ok(false) => ComponentHealth::Unavailable,
            Err(e) => ComponentHealth::Degraded(format!("{e}")),
        }
    }
}

#[async_trait]
impl AppearanceComponent for PortalAppearance {
    async fn set_wallpaper(&self, path: &str) -> Result<()> {
        let uri = if path.contains("://") {
            path.to_string()
        } else {
            let abs = std::fs::canonicalize(path)
                .map_err(|e| AgentShellError::Other(format!("{path}: {e}").into()))?;
            format!("file://{}", abs.display())
        };
        // SetWallpaperURI(parent_window "", uri, options {handle_token})
        let p = self.portal("org.freedesktop.portal.Wallpaper").await?;
        let mut options: std::collections::HashMap<String, zbus::zvariant::Value> =
            std::collections::HashMap::new();
        options.insert("handle_token".into(), zbus::zvariant::Value::from("agent-shell".to_string()));
        let arg: (&str, &str, std::collections::HashMap<String, zbus::zvariant::Value>) =
            ("", uri.as_str(), options);
        let _reply: () = p
            .call("SetWallpaperURI", &arg)
            .await
            .map_err(|e| AgentShellError::DBus(format!("SetWallpaperURI: {e}")))?;
        Ok(())
    }

    async fn get_color_scheme(&self) -> Result<ColorScheme> {
        self.read_color_scheme().await
    }

    async fn set_color_scheme(&self, scheme: ColorScheme) -> Result<()> {
        // 无跨 DE 写入 portal；GNOME 上 gsettings 可用则直接写。
        let value = match scheme {
            // GNOME color-scheme 合法值 default/prefer-dark/prefer-light；
            // 写 default 读回是 NoPreference，浅色必须写 prefer-light（读写对称）。
            ColorScheme::Dark => "prefer-dark",
            ColorScheme::Light => "prefer-light",
            ColorScheme::NoPreference => "default",
        };
        let out = tokio::process::Command::new("gsettings")
            .args([
                "set",
                "org.gnome.desktop.interface",
                "color-scheme",
                value,
            ])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(AgentShellError::BackendUnavailable(format!(
                "gsettings color-scheme write failed (non-GNOME DE should route to its \
                 backend wrapper): {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ))),
            Err(e) => Err(AgentShellError::BackendUnavailable(format!(
                "gsettings unavailable: {e}"
            ))),
        }
    }
}
