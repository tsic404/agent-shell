//! NetworkManager 组件（`org.freedesktop.NetworkManager`，system bus）。
//!
//! design/13 §21.3 网络表「跨 DE」行：标准网络管理接口。本实现用 zbus 动态
//! 代理（非生成宏）：NM 接口面大而稳定，动态调用点少（每能力 1–3 个方法），
//! 免去 codegen 的编译期负担；错误统一折算
//! [`AgentShellError::DBus`](agent_shell_core::error::AgentShellError::DBus)。
//!
//! 小步探测约定（§21.36.4）：服务名 → 对象路径 → 方法，任何一步缺失返回
//! 结构化错误；capability 记录实际命中的对象路径供 doctor 输出。

use async_trait::async_trait;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Signature, Value};

use agent_shell_core::component::{
    ComponentHealth, ComponentType, DesktopComponent, NetworkComponent,
};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::services::{Connectivity, NetworkState, WifiNetwork};

const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const ACTIVE_CONN_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const WIRELESS_ENABLED_PROP: &str = "WirelessEnabled";
/// NM settings 对象面（已知连接枚举）。
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const SETTINGS_CONN_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const IP4CONFIG_IFACE: &str = "org.freedesktop.NetworkManager.IP4Config";

/// `NM_SERVICE` 从 crate 根再导（lib.rs detect() 也用）。
use super::NM_SERVICE;

/// NetworkManager 连通性检查结果（D-Bus `Connectivity` 属性）。
mod nm_connectivity {
    pub const NONE: u32 = 1;
    pub const PORTAL: u32 = 2;
    pub const LIMITED: u32 = 3;
    pub const FULL: u32 = 4;
}

/// NetworkManager 设备类型（D-Bus `DeviceType`）。
mod nm_device_type {
    pub const WIFI: u32 = 2;
}

/// NetworkManager 设备状态。
mod nm_dev_state {
    /// 已激活（含 IP 配置完成）。
    pub const ACTIVATED: u32 = 100;
    /// 激活中。
    pub const ACTIVATING: u32 = 70;
}

/// NetworkManager ActiveConnection 状态（`Connection.Active.State`）。
mod nm_active_state {
    /// 已激活（IP 配置完成，成功判定）。
    pub const ACTIVATED: u32 = 2;
    /// 停用中/已停用（失败判定——含认证被拒后回退）。
    pub const DEACTIVATING: u32 = 3;
    pub const DEACTIVATED: u32 = 4;
}

/// NetworkManager 组件实例。
pub struct NetworkManagerComponent {
    conn: zbus::Connection,
}

impl std::fmt::Debug for NetworkManagerComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkManagerComponent").finish()
    }
}

fn dbus_err(e: impl std::fmt::Display) -> AgentShellError {
    AgentShellError::DBus(format!("NetworkManager: {e}"))
}

impl NetworkManagerComponent {
    /// 连接 system bus 并构造组件。
    ///
    /// 不在此处探测 NM 服务存在性——探测走 [`probe`](Self::probe)，保持构造
    /// 与探测分离（doctor 可分别报告「总线不可达」与「服务不在位」）。
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::system()
            .await
            .map_err(|e| AgentShellError::DBus(format!("system bus connect: {e}")))?;
        Ok(Self { conn })
    }

    /// NM 服务是否在位（小步第一步：服务名可达 + Version 属性可读）。
    pub async fn probe(&self) -> bool {
        match self.nm_proxy().await {
            Ok(p) => p.get_property::<String>("Version").await.is_ok(),
            Err(_) => false,
        }
    }

    async fn nm_proxy(&self) -> Result<zbus::Proxy<'_>> {
        zbus::Proxy::new(&self.conn, NM_SERVICE, NM_PATH, NM_IFACE)
            .await
            .map_err(dbus_err)
    }

    async fn devices(&self) -> Result<Vec<ObjectPath<'_>>> {
        let p = self.nm_proxy().await?;
        let v: Vec<ObjectPath> = p.get_property("Devices").await.map_err(dbus_err)?;
        Ok(v)
    }

    /// 读取设备属性（Device 基础接口）。
    async fn device_prop<T>(&self, dev: &ObjectPath<'_>, prop: &str) -> Result<T>
    where
        T: std::convert::TryFrom<zbus::zvariant::OwnedValue>,
        T::Error: Into<zbus::Error>,
    {
        zbus::Proxy::new(&self.conn, NM_SERVICE, dev, DEVICE_IFACE)
            .await
            .map_err(dbus_err)?
            .get_property::<T>(prop)
            .await
            .map_err(dbus_err)
    }

    /// 找到第一个 WiFi 设备。
    async fn wifi_device(&self) -> Result<Option<ObjectPath<'_>>> {
        for dev in self.devices().await? {
            let ty: u32 = self.device_prop(&dev, "DeviceType").await?;
            if ty == nm_device_type::WIFI {
                return Ok(Some(dev));
            }
        }
        Ok(None)
    }

    /// 当前活动连接的 IPv4 地址与 SSID（取第一个激活的 WiFi 或任意活动连接）。
    ///
    /// 返回 `(ssid, ip)`，任一未知为 None。
    async fn active_ssid_and_ip(&self) -> (Option<String>, Option<String>) {
        let Ok(p) = self.nm_proxy().await else {
            return (None, None);
        };
        let active: Vec<ObjectPath> = p
            .get_property("ActiveConnections")
            .await
            .unwrap_or_default();
        for ac in active {
            let Ok(proxy) = zbus::Proxy::new(&self.conn, NM_SERVICE, &ac, ACTIVE_CONN_IFACE).await
            else {
                continue;
            };
            // State: 0=unknown 1=activating 2=activated（Connection.Active 稳定 ABI）。
            let state: u32 = match proxy.get_property("State").await {
                Ok(s) => s,
                Err(_) => continue,
            };
            if state < 2 {
                continue;
            }
            let dev_path: Option<ObjectPath> = proxy
                .get_property::<Vec<ObjectPath>>("Devices")
                .await
                .ok()
                .and_then(|ds| ds.into_iter().next());
            let Some(dev) = dev_path else { continue };
            let ip: Option<String> = self.ipv4_of_device(&dev).await;
            let ty: u32 = self.device_prop(&dev, "DeviceType").await.unwrap_or(0);
            let ssid = if ty == nm_device_type::WIFI {
                // WiFi 设备：从 ActiveAccessPoint（o，可为 "/" 表示无）读 SSID。
                let wireless = zbus::Proxy::new(&self.conn, NM_SERVICE, &dev, WIRELESS_IFACE)
                    .await
                    .ok();
                let ap: Option<OwnedObjectPath> = match wireless {
                    Some(w) => w
                        .get_property::<OwnedObjectPath>("ActiveAccessPoint")
                        .await
                        .ok(),
                    None => None,
                };
                match ap {
                    Some(ap_path) if ap_path.as_str() != "/" => {
                        self.ssid_of_ap(&ObjectPath::from(ap_path)).await
                    }
                    _ => None,
                }
            } else {
                None
            };
            return (ssid, ip);
        }
        (None, None)
    }

    /// 读 AP 的 SSID 字节并按 UTF-8 宽松解码（隐藏/非 UTF8 SSID 截断可见段）。
    async fn ssid_of_ap(&self, ap: &ObjectPath<'_>) -> Option<String> {
        let proxy = zbus::Proxy::new(&self.conn, NM_SERVICE, ap, AP_IFACE)
            .await
            .ok()?;
        let bytes: Vec<u8> = proxy.get_property("Ssid").await.ok()?;
        decode_ssid(&bytes)
    }

    async fn ipv4_of_device(&self, dev: &ObjectPath<'_>) -> Option<String> {
        let cfg: Option<OwnedObjectPath> =
            zbus::Proxy::new(&self.conn, NM_SERVICE, dev, DEVICE_IFACE)
                .await
                .ok()?
                .get_property("Ip4Config")
                .await
                .ok();
        let cfg = cfg?;
        let p = zbus::Proxy::new(
            &self.conn,
            NM_SERVICE,
            ObjectPath::from(cfg),
            IP4CONFIG_IFACE,
        )
        .await
        .ok()?;
        let data: Vec<std::collections::HashMap<String, Value>> =
            p.get_property("AddressData").await.ok()?;
        let first = data.first()?;
        first.get("address")?.downcast_ref::<String>().ok()
    }

    /// 把 NM Connectivity 枚举映射到统一 [`Connectivity`]。
    fn map_connectivity(raw: u32, has_ip: bool) -> Connectivity {
        match raw {
            nm_connectivity::FULL => Connectivity::Full,
            nm_connectivity::LIMITED | nm_connectivity::PORTAL => Connectivity::Limited,
            nm_connectivity::NONE => {
                if has_ip {
                    Connectivity::Local
                } else {
                    Connectivity::None
                }
            }
            _ => {
                // UNKNOWN(0)：有 IP 视作 Local，否则 None。
                if has_ip {
                    Connectivity::Local
                } else {
                    Connectivity::None
                }
            }
        }
    }
}

#[async_trait]
impl DesktopComponent for NetworkManagerComponent {
    fn name(&self) -> &'static str {
        "NetworkManagerComponent"
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Network
    }

    fn is_available(&self) -> bool {
        // 同步视图无缓存——保守 false；真实可用性经 probe()/health() 异步判定。
        false
    }

    async fn health(&self) -> ComponentHealth {
        if self.probe().await {
            ComponentHealth::Healthy
        } else {
            ComponentHealth::Unavailable
        }
    }
}

#[async_trait]
impl NetworkComponent for NetworkManagerComponent {
    async fn get_network_state(&self) -> Result<NetworkState> {
        let p = self.nm_proxy().await?;
        let connectivity_raw: u32 = p.get_property("Connectivity").await.map_err(dbus_err)?;
        let wifi_enabled: bool = p
            .get_property(WIRELESS_ENABLED_PROP)
            .await
            .map_err(dbus_err)?;
        // NM Metered 枚举：UNKNOWN=0 / YES=1 / NO=2 / GUESS_YES=3 / GUESS_NO=4
        // （NM 1.x 稳定 ABI）。计费 = 明确 YES 或猜测 YES。
        let metered: bool = p
            .get_property::<u32>("Metered")
            .await
            .map(|m| matches!(m, 1 | 3))
            .unwrap_or(false); // 查询失败按否处理
        let (active_ssid, ip_address) = self.active_ssid_and_ip().await;
        Ok(NetworkState {
            connectivity: Self::map_connectivity(connectivity_raw, ip_address.is_some()),
            wifi_enabled,
            active_ssid,
            ip_address,
            metered,
        })
    }

    async fn list_wifi_networks(&self) -> Result<Vec<WifiNetwork>> {
        let Some(dev) = self.wifi_device().await? else {
            return Err(AgentShellError::BackendUnavailable(
                "no Wi-Fi device present".into(),
            ));
        };
        // 先请求扫描再读 AP 列表（RequestScan 无参变体：空 a{sv}）。
        let wireless = zbus::Proxy::new(&self.conn, NM_SERVICE, &dev, WIRELESS_IFACE)
            .await
            .map_err(dbus_err)?;
        if let Err(e) = wireless
            .call::<_, _, ()>(
                "RequestScan",
                &std::collections::HashMap::<String, Value>::new(),
            )
            .await
        {
            // RequestScan 在某些权限配置下被拒；已有扫描结果仍可列出。
            tracing::debug!(error = %e, "RequestScan rejected, listing stale APs");
        }
        let aps: Vec<ObjectPath> = wireless
            .get_property("AccessPoints")
            .await
            .map_err(dbus_err)?;

        // 已知网络集合：settings 里的 connection SSID。
        let known_ssids = self.known_wifi_ssids().await;

        let mut networks = Vec::with_capacity(aps.len());
        for ap in aps {
            let proxy = zbus::Proxy::new(&self.conn, NM_SERVICE, &ap, AP_IFACE)
                .await
                .map_err(dbus_err)?;
            let ssid_bytes: Vec<u8> = proxy.get_property("Ssid").await.unwrap_or_default();
            let ssid = String::from_utf8_lossy(&ssid_bytes)
                .trim_end_matches('\0')
                .to_string();
            if ssid.is_empty() {
                continue; // 隐藏网络占位 AP 不进列表
            }
            let strength_byte: u8 = proxy.get_property("Strength").await.unwrap_or(0);
            let frequency: u32 = proxy.get_property("Frequency").await.unwrap_or(0);
            let flags: u32 = proxy.get_property("Flags").await.unwrap_or(0);
            let wpa_flags: u32 = proxy.get_property("WpaFlags").await.unwrap_or(0);
            let rsn_flags: u32 = proxy.get_property("RsnFlags").await.unwrap_or(0);
            networks.push(WifiNetwork {
                known: known_ssids.contains(&ssid),
                secured: flags & 0x1 != 0 || wpa_flags != 0 || rsn_flags != 0,
                strength: strength_byte.min(100),
                ssid,
                frequency,
            });
        }
        // 强度降序展示。
        networks.sort_by_key(|n| std::cmp::Reverse(n.strength));
        Ok(networks)
    }
    async fn connect_wifi(&self, ssid: &str, password: Option<&str>) -> Result<()> {
        use zvariant::{Array, Dict};

        if ssid.is_empty() {
            return Err(AgentShellError::DBus("empty ssid".into()));
        }

        // 组装 a{sa{sv}} connection 设置（NM AddAndActivateConnection 第一参数，
        // NM 1.x 稳定 ABI）：connection / 802-11-wireless / 802-11-wireless-security。
        fn make_group(
            items: &[(&str, Value<'_>)],
        ) -> std::result::Result<Value<'static>, AgentShellError> {
            let mut d =
                zvariant::Dict::new(&zvariant::Signature::Str, &zvariant::Signature::Variant);
            for (k, v) in items {
                let owned = v
                    .try_to_owned()
                    .map_err(|e| AgentShellError::DBus(format!("value clone: {e}")))?;
                d.append(Value::from(k.to_string()), Value::from(owned))
                    .map_err(|e| AgentShellError::DBus(format!("dict append: {e}")))?;
            }
            Ok(Value::Dict(d))
        }

        let mut settings = Dict::new(&Signature::Str, &Signature::Variant);
        // 组：connection / 802-11-wireless / 802-11-wireless-security。
        settings
            .append(
                Value::from("connection".to_string()),
                Value::Value(Box::new(make_group(&[
                    ("type", Value::from("802-11-wireless")),
                    ("id", Value::from(ssid.to_string())),
                ])?)),
            )
            .map_err(dbus_err)?;
        // SSID 为字节数组（ay）——非 UTF8 SSID 合法，必须按字节透传。
        let mut ssid_arr = Array::new(&Signature::U8);
        for b in ssid.as_bytes() {
            ssid_arr.append(Value::from(*b)).map_err(dbus_err)?;
        }
        settings
            .append(
                Value::from("802-11-wireless".to_string()),
                Value::Value(Box::new(make_group(&[("ssid", Value::from(ssid_arr))])?)),
            )
            .map_err(dbus_err)?;
        // 空 PSK 必被 AP 拒绝且无法区分密码错——按无密码处理（开放网络语义）。
        let password = password.filter(|pw| !pw.is_empty());
        if let Some(pw) = password {
            settings
                .append(
                    Value::from("802-11-wireless-security".to_string()),
                    Value::Value(Box::new(make_group(&[
                        ("key-mgmt", Value::from("wpa-psk")),
                        ("psk", Value::from(pw.to_string())),
                    ])?)),
                )
                .map_err(dbus_err)?;
        }

        let target = self
            .wifi_device()
            .await?
            .ok_or_else(|| AgentShellError::BackendUnavailable("no Wi-Fi device".into()))?;

        // 目标 AP：从扫描列表匹配 SSID；未扫到时传 "/" 让 NM 自行选择/新建。
        let matched = match self.match_ap_by_ssid(&target, ssid).await {
            Some(ap) => ap,
            None => root_path(),
        };

        let nm = self.nm_proxy().await?;
        // Dict 不实现 Type——先序列化为 Value 树再传参（a{sa{sv}}）。
        let settings_value = Value::Dict(settings);
        // AddAndActivateConnection(settings, device, specific_object) → ActiveConnection
        // 路径。后续收敛判定必须轮询**该对象**的 Connection.Active.State——
        // 不能轮询设备级 State：设备原已连接其他 SSID、新连接认证失败回退
        // 旧连接时设备会回到 ACTIVATED，误报新 SSID 成功。
        let active_conn: OwnedObjectPath = nm
            .call(
                "AddAndActivateConnection",
                &(settings_value, &target, matched.as_str()),
            )
            .await
            .map_err(dbus_err)?;

        self.wait_activation(&active_conn, ssid).await
    }

    async fn disconnect_wifi(&self) -> Result<()> {
        let Some(dev) = self.wifi_device().await? else {
            return Err(AgentShellError::BackendUnavailable(
                "no Wi-Fi device present".into(),
            ));
        };
        let state: u32 = self.device_prop(&dev, "State").await.unwrap_or(0);
        if state != nm_dev_state::ACTIVATED && state != nm_dev_state::ACTIVATING {
            return Err(AgentShellError::BackendUnavailable(format!(
                "Wi-Fi device not connected (state {state})"
            )));
        }
        let proxy = zbus::Proxy::new(&self.conn, NM_SERVICE, &dev, DEVICE_IFACE)
            .await
            .map_err(dbus_err)?;
        let _: () = proxy.call("Disconnect", &()).await.map_err(dbus_err)?;
        Ok(())
    }
}

impl NetworkManagerComponent {
    /// settings 连接池中的已知 WiFi SSID（known 标记数据源）。
    async fn known_wifi_ssids(&self) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        let Ok(list) =
            zbus::Proxy::new(&self.conn, NM_SERVICE, SETTINGS_PATH, SETTINGS_IFACE).await
        else {
            return out;
        };
        let paths: Vec<OwnedObjectPath> = match list
            .call::<_, _, Vec<OwnedObjectPath>>("ListConnections", &())
            .await
        {
            Ok(p) => p,
            Err(_) => return out,
        };
        for c in paths {
            let Ok(proxy) = zbus::Proxy::new(&self.conn, NM_SERVICE, &c, SETTINGS_CONN_IFACE).await
            else {
                continue;
            };
            if let Ok(owned) = proxy
                .call::<_, _, zvariant::OwnedValue>("GetSettings", &())
                .await
            {
                // settings: a{sa{sv}} —— 反序列化为 Value 树后遍历取
                // 802-11-wireless.ssid（GetSettings 含密钥的字段会被 NM 脱敏，
                // ssid 字节不受影响）。
                let value: Value = owned.into();
                if let Some(ssid_bytes) = find_wifi_ssid(&value) {
                    if let Some(s) = decode_ssid(&ssid_bytes) {
                        out.insert(s);
                    }
                }
            }
        }
        out
    }

    /// 在目标设备的 AP 列表中找指定 SSID 的对象路径。
    async fn match_ap_by_ssid(
        &self,
        dev: &ObjectPath<'_>,
        ssid: &str,
    ) -> Option<ObjectPath<'static>> {
        let wireless = zbus::Proxy::new(&self.conn, NM_SERVICE, dev, WIRELESS_IFACE)
            .await
            .ok()?;
        let aps: Vec<OwnedObjectPath> = wireless.get_property("AccessPoints").await.ok()?;
        for ap in aps {
            if let Ok(proxy) = zbus::Proxy::new(&self.conn, NM_SERVICE, &ap, AP_IFACE).await {
                if let Ok(bytes) = proxy.get_property::<Vec<u8>>("Ssid").await {
                    if decode_ssid(&bytes).as_deref() == Some(ssid) {
                        return Some(ap.into());
                    }
                }
            }
        }
        None
    }

    /// 轮询 AddAndActivateConnection 返回的 ActiveConnection 直到收敛。
    ///
    /// 判定走 `Connection.Active.State`（0=unknown 1=activating 2=activated
    /// 3=deactivating 4=deactivated，NM 1.x 稳定 ABI）：activated → Ok；
    /// deactivated/消失 → 认证或激活失败；超时 20s。设备级状态不作为
    /// 成功依据（回退旧连接场景见 connect_wifi 注释）。
    async fn wait_activation(&self, active_conn: &OwnedObjectPath, ssid: &str) -> Result<()> {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(20);
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
            if tokio::time::Instant::now() >= deadline {
                return Err(AgentShellError::Timeout(format!(
                    "wifi activation for {ssid:?} did not converge in 20s"
                )));
            }
            let Ok(proxy) = zbus::Proxy::new(
                &self.conn,
                NM_SERVICE,
                active_conn.as_str(),
                ACTIVE_CONN_IFACE,
            )
            .await
            else {
                return Err(AgentShellError::DBus(format!(
                    "active connection for {ssid:?} vanished during activation"
                )));
            };
            match proxy.get_property::<u32>("State").await {
                Ok(nm_active_state::ACTIVATED) => return Ok(()),
                Ok(nm_active_state::DEACTIVATED) | Ok(nm_active_state::DEACTIVATING) => {
                    return Err(AgentShellError::DBus(format!(
                        "activation failed for {ssid:?} (connection deactivated; check credentials)"
                    )));
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(AgentShellError::DBus(format!(
                        "NetworkManager: Connection.Active.State for {ssid:?}: {e}"
                    )));
                }
            }
        }
    }

    /// doctor 报告：NM 版本 + 实际使用的对象面。
    pub async fn channel_report(&self) -> String {
        match self.nm_proxy().await {
            Ok(p) => match p.get_property::<String>("Version").await {
                Ok(v) => format!("{NM_SERVICE} v{v}"),
                Err(_) => NM_SERVICE.to_string(),
            },
            Err(_) => "unavailable".into(),
        }
    }
}

/// 在 a{sa{sv}} Value 树中定位 `802-11-wireless` 组的 `ssid`（ay）字节。
fn find_wifi_ssid(value: &Value<'_>) -> Option<Vec<u8>> {
    let Value::Dict(dict) = value else {
        return None;
    };
    for (k, v) in dict.iter() {
        let group_ok = k.downcast_ref::<String>().ok().as_deref() == Some("802-11-wireless");
        if !group_ok {
            continue;
        }
        let Value::Dict(group) = v else { continue };
        for (gk, gv) in group.iter() {
            if gk.downcast_ref::<String>().ok().as_deref() == Some("ssid") {
                if let Value::Array(arr) = gv {
                    return Some(
                        arr.iter()
                            .filter_map(|b| b.downcast_ref::<u8>().ok())
                            .collect(),
                    );
                }
            }
        }
    }
    None
}

/// SSID 字节解码：UTF-8 有效按原样，否则 lossy（隐藏/非 UTF8 SSID 截断可见段）。
fn decode_ssid(bytes: &[u8]) -> Option<String> {
    let trimmed: Vec<u8> = bytes.iter().copied().take_while(|b| *b != 0).collect();
    let s = String::from_utf8(trimmed)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// NM API 的空 specific_object 路径（让 NM 自行选择 AP）。
fn root_path() -> ObjectPath<'static> {
    ObjectPath::try_from("/").expect("literal is a valid object path")
}
