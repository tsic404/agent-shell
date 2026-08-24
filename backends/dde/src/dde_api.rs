//! DDE 服务组件：双名探测路由 + 电源/锁屏/通知/外观/启动器封装（§21.6、§21.36）。

use agent_shell_core::component::{
    AppearanceComponent, ComponentHealth, ComponentType, DesktopComponent, LauncherComponent,
    NotificationComponent, PowerComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{
    AppInfo, AppTarget, BatteryState, ColorScheme, NotificationSpec,
};
use agent_shell_power::{probe_first_existing, service_exists, UPowerComponent};
use async_trait::async_trait;
use zbus::{proxy, Connection};

/// DDE 电源双服务名单一来源：`agent_shell_power::DDE_POWER_NAMES`（re-export，§21.36.4）。
pub use agent_shell_power::DDE_POWER_NAMES;
/// 多电池百分比聚合：按电池数取平均并钳制 [0.0, 100.0]（求和会超 100%）。
fn average_battery_percentage(map: &std::collections::HashMap<String, f64>) -> f64 {
    if map.is_empty() {
        return 0.0;
    }
    let sum: f64 = map.values().copied().sum();
    (sum / map.len() as f64).clamp(0.0, 100.0)
}

pub const DDE_LOCK_NAMES: [&str; 2] =
    ["org.deepin.dde.LockService1", "com.deepin.daemon.LockService"];
pub const DDE_NOTIFICATION_NAMES: [&str; 2] = [
    "org.deepin.dde.Notification1",
    "com.deepin.daemon.Notification",
];
pub const DDE_APPEARANCE_NAMES: [&str; 2] =
    ["org.deepin.dde.Appearance1", "com.deepin.daemon.Appearance"];

/// Power1 / LockService1 在 system bus。
pub async fn connect_system() -> Result<Connection> {
    Connection::system()
        .await
        .map_err(|e| AgentShellError::DBus(format!("system bus: {e}")))
}

/// session bus 连接。
pub async fn connect_session() -> Result<Connection> {
    Connection::session()
        .await
        .map_err(|e| AgentShellError::DBus(format!("session bus: {e}")))
}

/// zbus 错误 → AgentShellError。
fn dde_err(e: zbus::Error) -> AgentShellError {
    AgentShellError::DBus(format!("DDE: {e}"))
}

// ───────────────────────── 电源 ─────────────────────────

/// org.deepin.dde.Power1（system bus）。属性按 §21.36.1 实测：
/// `BatteryPercentage(a{sd})`、`OnBattery`、`BatteryState(a{su})`、
/// `BatteryIsPresent(a{sb})`——均为 battery→value 映射；
/// 方法 `SetPrepareSuspend(i)`。旧文档「根对象 SetVolume/SetSuspend」已过时。
#[proxy(
    interface = "org.deepin.dde.Power1",
    default_service = "org.deepin.dde.Power1",
    default_path = "/org/deepin/dde/Power1"
)]
trait DdePower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn battery_percentage(&self) -> zbus::Result<std::collections::HashMap<String, f64>>;

    #[zbus(property)]
    fn battery_is_present(&self) -> zbus::Result<std::collections::HashMap<String, bool>>;
}

/// com.deepin.daemon.Power（DDE20 名，同方法集）。
#[proxy(
    interface = "com.deepin.daemon.Power",
    default_service = "com.deepin.daemon.Power",
    default_path = "/com/deepin/daemon/Power"
)]
trait ComDeepinPower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn battery_percentage(&self) -> zbus::Result<std::collections::HashMap<String, f64>>;

    #[zbus(property)]
    fn battery_is_present(&self) -> zbus::Result<std::collections::HashMap<String, bool>>;
}

enum PowerProxy {
    New(DdePowerProxy<'static>),
    Old(ComDeepinPowerProxy<'static>),
}

impl PowerProxy {
    async fn probe(conn: &Connection) -> Result<Option<Self>> {
        match probe_first_existing(conn, &DDE_POWER_NAMES).await? {
            Some(name) if name == DDE_POWER_NAMES[0] => Ok(Some(PowerProxy::New(
                DdePowerProxy::builder(conn)
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .map_err(dde_err)?,
            ))),
            Some(_) => Ok(Some(PowerProxy::Old(
                ComDeepinPowerProxy::builder(conn)
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .map_err(dde_err)?,
            ))),
            None => Ok(None),
        }
    }

    async fn aggregate_present(&self) -> zbus::Result<bool> {
        let map = match self {
            PowerProxy::New(p) => p.battery_is_present().await?,
            PowerProxy::Old(p) => p.battery_is_present().await?,
        };
        Ok(map.values().any(|v| *v))
    }

    async fn aggregate_percentage(&self) -> zbus::Result<f64> {
        let map = match self {
            PowerProxy::New(p) => p.battery_percentage().await?,
            PowerProxy::Old(p) => p.battery_percentage().await?,
        };
        Ok(average_battery_percentage(&map))
    }

    async fn on_battery(&self) -> zbus::Result<bool> {
        match self {
            PowerProxy::New(p) => p.on_battery().await,
            PowerProxy::Old(p) => p.on_battery().await,
        }
    }
}

/// DDE 电源封装：Power1 属性读电池；挂起/休眠/关机走 login1（polkit L4，
/// power policy 属系统级授权，见 §3.5 DDE polkit 策略记录）。
pub struct DdePower {
    system: Connection,
    power_proxy: Option<PowerProxy>,
    login1: UPowerComponent,
}

impl DdePower {
    /// DePriorityRouter 入口：探 Power1 双名；未命中仍构造实例但降级
    /// （电池读数回退 UPower），由 health()/capability 快照反映实际通道。
    pub async fn try_new() -> Result<Self> {
        let system = connect_system().await?;
        let power_proxy = PowerProxy::probe(&system).await?;
        let login1 = UPowerComponent::new().await?;
        Ok(Self {
            system,
            power_proxy,
            login1,
        })
    }

    /// DE 封装是否命中（供能力快照）。
    pub fn de_native_battery(&self) -> bool {
        self.power_proxy.is_some()
    }

    async fn lock_proxy(&self) -> Result<LockServiceProxy<'static>> {
        LockServiceProxy::builder(&self.system)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(dde_err)
    }
}

#[async_trait]
impl DesktopComponent for DdePower {
    fn name(&self) -> &'static str {
        "dde-power"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Power
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        if !service_exists(&self.system, DDE_POWER_NAMES[0])
            .await
            .unwrap_or(false)
            && !service_exists(&self.system, DDE_POWER_NAMES[1])
                .await
                .unwrap_or(false)
        {
            return ComponentHealth::Degraded(format!(
                "neither {} nor {} present; battery via UPower fallback",
                DDE_POWER_NAMES[0],
                DDE_POWER_NAMES[1]
            ));
        }
        ComponentHealth::Healthy
    }
}

#[async_trait]
impl PowerComponent for DdePower {
    /// L4：调用方必须先确认。LockNow() 走 LockService1（system bus + polkit）。
    async fn lock_screen(&self) -> Result<()> {
        // 双名探测锁屏服务后调用 LockNow()。
        let lock_result = match probe_first_existing(&self.system, &DDE_LOCK_NAMES).await? {
            Some(name) if name == DDE_LOCK_NAMES[0] => self.lock_proxy().await?.lock_now().await.map_err(dde_err),
            Some(_) => {
                OldLockServiceProxy::builder(&self.system)
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .map_err(dde_err)?
                    .lock_now()
                    .await
                    .map_err(dde_err)
            }
            None => Err(AgentShellError::BackendUnavailable(
                "no DDE LockService (org.deepin.dde.LockService1 / com.deepin.daemon.LockService); \
                 degraded path: logind LockSession via SessionManagerComponent"
                    .into(),
            )),
        };
        lock_result
        .map_err(|e| AgentShellError::DBus(format!("DDE LockNow: {e}")))
    }

    async fn logout(&self) -> Result<()> {
        Err(AgentShellError::NotImplemented(
            "DDE logout via SessionShell/session manager; pending wiring".into(),
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
        if let Some(p) = &self.power_proxy {
            let is_present = p.aggregate_present().await.map_err(|e| {
                AgentShellError::DBus(format!("Power1 BatteryIsPresent: {e}"))
            })?;
            if !is_present {
                return Ok(BatteryState {
                    percentage: 0.0,
                    charging: false,
                    time_to_empty: None,
                    time_to_full: None,
                    is_present: false,
                });
            }
            return Ok(BatteryState {
                percentage: p
                    .aggregate_percentage()
                    .await
                    .map_err(|e| AgentShellError::DBus(format!("Power1 BatteryPercentage: {e}")))?,
                charging: !p
                    .on_battery()
                    .await
                    .map_err(|e| AgentShellError::DBus(format!("Power1 OnBattery: {e}")))?,
                time_to_empty: None, // Power1 不提供时间估计；如需走 UPower 补齐
                time_to_full: None,
                is_present: true,
            });
        }
        self.login1.get_battery_status().await
    }
}

#[proxy(
    interface = "org.deepin.dde.LockService1",
    default_service = "org.deepin.dde.LockService1",
    default_path = "/org/deepin/dde/LockService1"
)]
trait LockService {
    fn lock_now(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "com.deepin.daemon.LockService",
    default_service = "com.deepin.daemon.LockService",
    default_path = "/com/deepin/daemon/LockService"
)]
trait OldLockService {
    fn lock_now(&self) -> zbus::Result<()>;
}

// ───────────────────────── 通知 ❓需真机确认 ─────────────────────────

/// DDE 通知封装。`org.deepin.dde.Notification1.Notify(appName, replacesId,
/// appIcon, summary, body, actions, hints, timeout)` 方法签名同 freedesktop
/// 规范（§21.36）；服务归属未核实 → 探测失败时上层装配回退公共通知组件。
pub struct DdeNotification {
    /// 成对代理：DDE25 新名 / DDE20 旧名，接口与路径随服务名成对切换
    /// （与 PowerProxy 同模式，避免「探测命中旧名、调用硬编码新名」断裂）。
    proxy: NotificationProxy,
}

impl DdeNotification {
    pub async fn try_new() -> Result<Option<Self>> {
        let conn = connect_session().await?;
        match probe_first_existing(&conn, &DDE_NOTIFICATION_NAMES).await? {
            Some(name) if name == DDE_NOTIFICATION_NAMES[0] => Ok(Some(Self {
                proxy: NotificationProxy::New(
                    DdeNotificationServiceProxy::builder(&conn)
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                        .map_err(dde_err)?,
                ),
            })),
            Some(_) => Ok(Some(Self {
                proxy: NotificationProxy::Old(
                    ComDeepinNotificationProxy::builder(&conn)
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                        .map_err(dde_err)?,
                ),
            })),
            None => Ok(None),
        }
    }

    fn notify_call(&self) -> &dyn NotifyMethods {
        match &self.proxy {
            NotificationProxy::New(p) => p,
            NotificationProxy::Old(p) => p,
        }
    }
}

/// DDE25 新名代理（org.deepin.dde.Notification1，路径 /org/deepin/dde/Notification1）。
#[proxy(
    interface = "org.deepin.dde.Notification1",
    default_service = "org.deepin.dde.Notification1",
    default_path = "/org/deepin/dde/Notification1"
)]
trait DdeNotificationService {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: std::collections::HashMap<&'static str, zbus::zvariant::Value<'static>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;
}

/// DDE20 旧名代理（com.deepin.daemon.Notification，方法签名同 freedesktop 规范）。
#[proxy(
    interface = "com.deepin.daemon.Notification",
    default_service = "com.deepin.daemon.Notification",
    default_path = "/com/deepin/daemon/Notification"
)]
trait ComDeepinNotification {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: std::collections::HashMap<&'static str, zbus::zvariant::Value<'static>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;
}

enum NotificationProxy {
    New(DdeNotificationServiceProxy<'static>),
    Old(ComDeepinNotificationProxy<'static>),
}

/// 统一两个成对代理的调用面（freedesktop 同构签名）。
trait NotifyMethods: Sync + Send {
    #[allow(clippy::too_many_arguments)]
    fn notify<'a>(
        &'a self,
        app_name: &'a str,
        replaces_id: u32,
        app_icon: &'a str,
        summary: &'a str,
        body: &'a str,
        actions: Vec<&'a str>,
        hints: std::collections::HashMap<&'static str, zbus::zvariant::Value<'static>>,
        expire_timeout: i32,
    ) -> futures_util::future::BoxFuture<'a, zbus::Result<u32>>;

    fn close_notification<'a>(
        &'a self,
        id: u32,
    ) -> futures_util::future::BoxFuture<'a, zbus::Result<()>>;
}

macro_rules! impl_notify_methods {
    ($($t:ty),*) => {$(
        impl NotifyMethods for $t {
            fn notify<'a>(
                &'a self,
                app_name: &'a str,
                replaces_id: u32,
                app_icon: &'a str,
                summary: &'a str,
                body: &'a str,
                actions: Vec<&'a str>,
                hints: std::collections::HashMap<&'static str, zbus::zvariant::Value<'static>>,
                expire_timeout: i32,
            ) -> futures_util::future::BoxFuture<'a, zbus::Result<u32>> {
                Box::pin(<$t>::notify(self, app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout))
            }

            fn close_notification<'a>(
                &'a self,
                id: u32,
            ) -> futures_util::future::BoxFuture<'a, zbus::Result<()>> {
                Box::pin(<$t>::close_notification(self, id))
            }
        }
    )*};
}

impl_notify_methods!(DdeNotificationServiceProxy<'_>, ComDeepinNotificationProxy<'_>);

#[async_trait]
impl DesktopComponent for DdeNotification {
    fn name(&self) -> &'static str {
        "dde-notification"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Notification
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        ComponentHealth::Healthy
    }
}

#[async_trait]
impl NotificationComponent for DdeNotification {
    async fn send_notification(&self, notif: &NotificationSpec) -> Result<u32> {
        let mut flat = Vec::with_capacity(notif.actions.len() * 2);
        for (key, label) in &notif.actions {
            flat.push(key.clone());
            flat.push(label.clone());
        }
        let action_refs: Vec<&str> = flat.iter().map(String::as_str).collect();
        let mut hints = std::collections::HashMap::new();
        hints.insert(
            "urgency",
            zbus::zvariant::Value::from(match notif.urgency {
                agent_shell_core::services::NotificationUrgency::Low => 0u8,
                agent_shell_core::services::NotificationUrgency::Normal => 1u8,
                agent_shell_core::services::NotificationUrgency::Critical => 2u8,
            }),
        );
        let timeout = if notif.urgency == agent_shell_core::services::NotificationUrgency::Critical
        {
            0
        } else {
            notif.timeout_ms.unwrap_or(-1)
        };
        self.notify_call()
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
            .await
            .map_err(|e| AgentShellError::DBus(format!("DDE Notify: {e}")))
    }

    async fn close_notification(&self, id: u32) -> Result<()> {
        self.notify_call()
            .close_notification(id)
            .await
            .map_err(|e| AgentShellError::DBus(format!("DDE CloseNotification: {e}")))
    }
}

// ───────────────────────── 外观 ❓需真机确认 ─────────────────────────

/// DDE 外观封装：`SetWallpaper(image_path)` / `SetTheme(type, name)`。
/// 服务文件不在 dde-daemon（可能在 dde-session-shell）❓需真机确认；
/// 探测失败返回 None，上层回退 PortalAppearance。
pub struct DdeAppearance {
    /// 成对代理：DDE25 新名（/org/deepin/dde/Appearance1）/
    /// DDE20 旧名 com.deepin.daemon.Appearance（路径 /com/deepin/daemon/Appearance）。
    proxy: AppearanceProxy,
}

impl DdeAppearance {
    pub async fn try_new() -> Result<Option<Self>> {
        let conn = connect_session().await?;
        match probe_first_existing(&conn, &DDE_APPEARANCE_NAMES).await? {
            Some(name) if name == DDE_APPEARANCE_NAMES[0] => Ok(Some(Self {
                proxy: AppearanceProxy::New(
                    DdeAppearanceServiceProxy::builder(&conn)
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                        .map_err(dde_err)?,
                ),
            })),
            Some(_) => Ok(Some(Self {
                proxy: AppearanceProxy::Old(
                    ComDeepinAppearanceProxy::builder(&conn)
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                        .map_err(dde_err)?,
                ),
            })),
            None => Ok(None),
        }
    }

    fn appearance_call(&self) -> &dyn AppearanceMethods {
        match &self.proxy {
            AppearanceProxy::New(p) => p,
            AppearanceProxy::Old(p) => p,
        }
    }
}

/// DDE25 新名外观代理。
#[proxy(
    interface = "org.deepin.dde.Appearance1",
    default_service = "org.deepin.dde.Appearance1",
    default_path = "/org/deepin/dde/Appearance1"
)]
trait DdeAppearanceService {
    fn set_wallpaper(&self, image_path: &str) -> zbus::Result<()>;

    fn set_theme(&self, theme_type: &str, name: &str) -> zbus::Result<()>;
}

/// DDE20 旧名外观代理。
#[proxy(
    interface = "com.deepin.daemon.Appearance",
    default_service = "com.deepin.daemon.Appearance",
    default_path = "/com/deepin/daemon/Appearance"
)]
trait ComDeepinAppearance {
    fn set_wallpaper(&self, image_path: &str) -> zbus::Result<()>;

    fn set_theme(&self, theme_type: &str, name: &str) -> zbus::Result<()>;
}

enum AppearanceProxy {
    New(DdeAppearanceServiceProxy<'static>),
    Old(ComDeepinAppearanceProxy<'static>),
}

/// 统一两个成对代理的调用面。
trait AppearanceMethods: Sync + Send {
    fn set_wallpaper<'a>(
        &'a self,
        image_path: &'a str,
    ) -> futures_util::future::BoxFuture<'a, zbus::Result<()>>;

    fn set_theme<'a>(
        &'a self,
        theme_type: &'a str,
        name: &'a str,
    ) -> futures_util::future::BoxFuture<'a, zbus::Result<()>>;
}

macro_rules! impl_appearance_methods {
    ($($t:ty),*) => {$(
        impl AppearanceMethods for $t {
            fn set_wallpaper<'a>(
                &'a self,
                image_path: &'a str,
            ) -> futures_util::future::BoxFuture<'a, zbus::Result<()>> {
                Box::pin(<$t>::set_wallpaper(self, image_path))
            }

            fn set_theme<'a>(
                &'a self,
                theme_type: &'a str,
                name: &'a str,
            ) -> futures_util::future::BoxFuture<'a, zbus::Result<()>> {
                Box::pin(<$t>::set_theme(self, theme_type, name))
            }
        }
    )*};
}

impl_appearance_methods!(DdeAppearanceServiceProxy<'_>, ComDeepinAppearanceProxy<'_>);

#[async_trait]
impl DesktopComponent for DdeAppearance {
    fn name(&self) -> &'static str {
        "dde-appearance"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Appearance
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        ComponentHealth::Degraded(
            "org.deepin.dde.Appearance1 ownership unverified (not in dde-daemon); \
             wallpaper/theme calls may need portal fallback"
                .into(),
        )
    }
}

#[async_trait]
impl AppearanceComponent for DdeAppearance {
    async fn set_wallpaper(&self, path: &str) -> Result<()> {
        self.appearance_call()
            .set_wallpaper(path)
            .await
            .map_err(|e| AgentShellError::DBus(format!("DDE SetWallpaper: {e}")))
    }

    async fn get_color_scheme(&self) -> Result<ColorScheme> {
        // Themes 属性列出可用主题；当前主题归属未核实 ❓——保守报错并提示降级。
        Err(AgentShellError::NotImplemented(
            "DDE current-theme read unverified on real hardware; \
             degraded path: portal Settings.ReadOne color-scheme"
                .into(),
        ))
    }

    async fn set_color_scheme(&self, scheme: ColorScheme) -> Result<()> {
        let theme = match scheme {
            ColorScheme::Dark | ColorScheme::NoPreference => "dark",
            ColorScheme::Light => "light",
        };
        // SetTheme(type, name)：type=gtk 主题类别，name=dark/light。
        self.appearance_call()
            .set_theme("gtk", theme)
            .await
            .map_err(|e| AgentShellError::DBus(format!("DDE SetTheme: {e}")))
    }
}

// ───────────────────────── 启动器 ─────────────────────────

/// DDE 启动器：DDE25 优先 `dde-am` CLI（dde-application-manager）；
/// DDE20 未发现 Application1 ❓。两者皆失败 → 上层回退公共 gio 路径。
pub struct DdeLauncher {
    dde_am_available: bool,
    fallback: agent_shell_launcher::DesktopFileLauncher,
}

impl DdeLauncher {
    pub async fn try_new() -> Result<Self> {
        let dde_am_available = tokio::process::Command::new("dde-am")
            .arg("--help")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        Ok(Self {
            dde_am_available,
            fallback: agent_shell_launcher::DesktopFileLauncher::new(),
        })
    }

    pub fn uses_dde_am(&self) -> bool {
        self.dde_am_available
    }
}

#[async_trait]
impl DesktopComponent for DdeLauncher {
    fn name(&self) -> &'static str {
        "dde-launcher"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Launcher
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn health(&self) -> ComponentHealth {
        if self.dde_am_available {
            ComponentHealth::Healthy
        } else {
            ComponentHealth::Degraded(
                "dde-am unavailable; falling back to desktop-file+gio launch".into(),
            )
        }
    }
}

#[async_trait]
impl LauncherComponent for DdeLauncher {
    async fn list_installed_apps(&self) -> Result<Vec<AppInfo>> {
        self.fallback.list_installed_apps().await
    }

    async fn launch_app(&self, app: &AppTarget) -> Result<()> {
        if self.dde_am_available {
            if let AppTarget::ByDesktopFile(id) = app {
                // dde-am 启动的应用常驻并继承 stdio——与 gio_launch 同理，
                // piped + 等 EOF 会挂起；置 null 后只取退出码。
                let status = tokio::process::Command::new("dde-am")
                    .args(["launch", "--desktop-file", id])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .map_err(|e| AgentShellError::Other(format!("dde-am: {e}").into()))?;
                if status.success() {
                    return Ok(());
                }
                tracing::debug!(status = %status, "dde-am launch failed; falling back to gio");
            }
        }
        self.fallback.launch_app(app).await
    }

    async fn launch_uri(&self, uri: &str) -> Result<()> {
        self.fallback.launch_uri(uri).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §21.36.1：DDE25 主名在前，DDE20 旧名在后——顺序即探测优先级。
    #[test]
    fn multi_battery_percentage_is_averaged_and_clamped() {
        let mut m = std::collections::HashMap::new();
        m.insert("bat0".to_string(), 80.0);
        m.insert("bat1".to_string(), 60.0);
        assert!((average_battery_percentage(&m) - 70.0).abs() < f64::EPSILON);

        // 越界值被钳制到 [0,100]。
        let mut m2 = std::collections::HashMap::new();
        m2.insert("bat0".to_string(), 150.0);
        assert_eq!(average_battery_percentage(&m2), 100.0);

        let mut m3 = std::collections::HashMap::new();
        m3.insert("bat0".to_string(), -5.0);
        assert_eq!(average_battery_percentage(&m3), 0.0);

        // 空表（无电池）返回 0。
        let empty: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        assert_eq!(average_battery_percentage(&empty), 0.0);
    }

    #[test]
    fn dde_dual_names_probe_new_first() {
        assert_eq!(DDE_POWER_NAMES[0], "org.deepin.dde.Power1");
        assert_eq!(DDE_POWER_NAMES[1], "com.deepin.daemon.Power");
        assert_eq!(DDE_LOCK_NAMES[0], "org.deepin.dde.LockService1");
        assert_eq!(DDE_NOTIFICATION_NAMES[0], "org.deepin.dde.Notification1");
        assert_eq!(DDE_APPEARANCE_NAMES[0], "org.deepin.dde.Appearance1");
    }
}
