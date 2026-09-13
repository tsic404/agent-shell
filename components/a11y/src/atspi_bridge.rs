//! AT-SPI D-Bus 桥接（设计文档 §14.2 `a11y::atspi_bridge`）。
//!
//! 连接目标是 **a11y bus**（非 session bus）：先在 session bus 上调
//! `org.a11y.Bus.GetAddress()` 拿到 a11y 总线地址（如
//! `unix:path=$XDG_RUNTIME_DIR/at-spi/bus_0`），再直连该地址。Registry
//! 服务名 `org.a11y.atspi.Registry`，桌面根对象路径
//! `/org/a11y/atspi/accessible/root`（at-spi-2.0 atspi-constants.h）。
//!
//! 协议要点（对 at-spi2-registryd / Qt atspi 实现实测）：
//! - Accessible 接口方法为 PascalCase：`GetChildAtIndex`/`GetChildren`/
//!   `GetRole`/`GetRoleName`/`GetState`；属性 `Name`/`ChildCount`。
//! - 子节点引用为 `(bus_name, object_path)` 二元组；应用节点经
//!   DBus daemon `GetConnectionUnixProcessID` 解析 PID。
//! - 状态集是 `(u32, u32)` 位图，位索引即 `AtspiStateType` 枚举值。

use crate::tree::{ApplicationNode, AtspiRole, AtspiState, ElementNode, WindowNode};
use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_core::types::Rect;
use zbus::zvariant::OwnedObjectPath;

/// a11y 总线上的 Registry 服务名。
pub const REGISTRY_SERVICE: &str = "org.a11y.atspi.Registry";
/// 桌面根对象路径（所有应用的父节点）。
pub const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
/// session bus 上负责公布 a11y 总线地址的服务。
pub const BUS_SERVICE: &str = "org.a11y.Bus";
/// 坐标类型：屏幕坐标（AT_SPI_COORD_TYPE_SCREEN = 0）。
const COORD_TYPE_SCREEN: u32 = 0;
/// 语义遍历最大深度保护（深层 Web 树可达数十层）。
pub const MAX_TRAVERSE_DEPTH: u8 = 24;
/// session bus 方法调用超时：GetAddress 触发懒激活时，在无 AT-SPI 的隔离
/// 会话（dbus-run-session）里避免挂满 dbus-daemon 默认激活超时 ~120s
/// （对齐 §19 超时表 D-Bus 5s）。
const SESSION_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// AT-SPI D-Bus 桥接。
///
/// 持有到 a11y bus 的独立连接；所有树查询都经由本桥接的引用寻址
/// `(bus_name, path)` 完成。`Clone` 语义为共享同一连接（zbus::Connection
/// 内部是 Arc）。
#[derive(Clone)]
pub struct AtspiBridge {
    conn: zbus::Connection,
}

impl AtspiBridge {
    /// 连接 a11y bus。
    ///
    /// 失败（session bus 不通、a11y bus 未启动、Registry 无 owner）
    /// 统一归一为 [`AgentShellError::BackendUnavailable`]——装配层据此
    /// 判定组件 Unavailable 而非崩溃。
    pub async fn connect() -> Result<Self> {
        let session = zbus::connection::Builder::session()
            .map_err(|e| AgentShellError::BackendUnavailable(format!("session bus: {e}")))?
            .method_timeout(SESSION_CALL_TIMEOUT)
            .build()
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("session bus: {e}")))?;

        let dbus = zbus::fdo::DBusProxy::new(&session)
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("DBusProxy: {e}")))?;
        let has_owner = dbus
            .name_has_owner(
                BUS_SERVICE.try_into().map_err(|e| {
                    AgentShellError::BackendUnavailable(format!("bad bus name: {e}"))
                })?,
            )
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("NameHasOwner: {e}")))?;

        // 无 owner 时查询可激活列表：可激活 → GetAddress 按需启动 a11y bus；
        // 不可激活 → 提前返回，不触发激活（避免 ~120s 停顿，TSI-3086）。
        let activatable: Vec<zbus::names::OwnedBusName> = if has_owner {
            Vec::new()
        } else {
            dbus.list_activatable_names().await.map_err(|e| {
                AgentShellError::BackendUnavailable(format!("ListActivatableNames: {e}"))
            })?
        };
        if !should_probe_address(has_owner, &activatable) {
            return Err(AgentShellError::BackendUnavailable(
                "org.a11y.Bus not owned and not activatable; AT-SPI support disabled".into(),
            ));
        }

        let bus = zbus::Proxy::new(&session, BUS_SERVICE, "/org/a11y/bus", "org.a11y.Bus")
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("org.a11y.Bus: {e}")))?;
        // 返回签名是 s（实测 busctl），不是 v
        let address: String =
            Self::err(bus.call("GetAddress", &()).await, "org.a11y.Bus.GetAddress")
                .map_err(|e| AgentShellError::BackendUnavailable(e.to_string()))?;

        let conn = zbus::connection::Builder::address(address.as_str())
            .map_err(|e| AgentShellError::BackendUnavailable(format!("a11y bus addr: {e}")))?
            .build()
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("a11y bus dial: {e}")))?;

        Ok(Self { conn })
    }

    /// 共享连接的克隆构造（component.rs 的 Arc 包装用）。
    pub fn clone_bridge(&self) -> Self {
        Self {
            conn: self.conn.clone(),
        }
    }

    fn err<T>(r: zbus::Result<T>, what: &'static str) -> Result<T> {
        r.map_err(|e| AgentShellError::DBus(format!("atspi {what}: {e}")))
    }

    /// 构造指向指定 `(bus_name, path)` 的通用 Proxy。
    ///
    /// AT-SPI 的属性访问不走标准 `org.freedesktop.DBus.Properties.GetAll`
    /// 缓存语义（部分实现返回空 a{sv}），必须禁用 zbus 属性缓存、
    /// 逐个 Get。
    pub async fn proxy_for<'a>(
        &'a self,
        bus_name: &'a str,
        path: &'a str,
        interface: &'a str,
    ) -> Result<zbus::Proxy<'a>> {
        let b = zbus::proxy::Builder::<zbus::Proxy<'a>>::new(&self.conn)
            .destination(bus_name.to_owned())
            .and_then(|b| b.path(path))
            .map(|b| b.interface(interface))
            .map_err(|e| AgentShellError::DBus(format!("proxy path: {e}")))?;
        let b = b
            .map_err(|e| AgentShellError::DBus(format!("proxy build: {e}")))?
            .cache_properties(zbus::proxy::CacheProperties::No);
        b.build()
            .await
            .map_err(|e| AgentShellError::DBus(format!("proxy {interface}: {e}")))
    }

    /// 方法调用 + 返回值解包（单返回值）。
    pub async fn call_checked<'b, B, R>(
        proxy: &zbus::Proxy<'_>,
        method: &str,
        body: &B,
        what: &'static str,
    ) -> Result<R>
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
        R: for<'de> serde::de::Deserialize<'de> + zbus::zvariant::Type,
    {
        let r: R = proxy
            .call(method, body)
            .await
            .map_err(|e| AgentShellError::DBus(format!("atspi {what}: {e}")))?;
        Ok(r)
    }

    // ───────────────────── 属性读取 ─────────────────────

    /// 读 Accessible.Name 属性。
    pub async fn get_name(&self, bus_name: &str, path: &str) -> Result<String> {
        let p = self
            .proxy_for(bus_name, path, "org.a11y.atspi.Accessible")
            .await?;
        let v: zbus::zvariant::Value = Self::err(p.get_property("Name").await, "Accessible.Name")?;
        Ok(v.try_into().unwrap_or_default())
    }

    /// 读 ChildCount 属性。
    pub async fn child_count(&self, bus_name: &str, path: &str) -> Result<i32> {
        let p = self
            .proxy_for(bus_name, path, "org.a11y.atspi.Accessible")
            .await?;
        let count: i32 = Self::err(p.get_property("ChildCount").await, "ChildCount")?;
        Ok(count)
    }

    /// GetState → 解码位图。
    pub async fn get_state(&self, bus_name: &str, path: &str) -> Result<AtspiState> {
        let p = self
            .proxy_for(bus_name, path, "org.a11y.atspi.Accessible")
            .await?;
        // 实测 GTK/Qt atspi 返回 `au`（gdbus: ([uint32 x, 0],)），
        // 而非规范文档的 (uu) 双字。
        let words: Vec<u32> = Self::err(p.call("GetState", &()).await, "GetState")?;
        let low = words.first().copied().unwrap_or(0);
        let high = words.get(1).copied().unwrap_or(0);
        Ok(AtspiState::from_pair(low, high))
    }

    /// GetRole + GetRoleName。
    pub async fn get_role(&self, bus_name: &str, path: &str) -> Result<AtspiRole> {
        let p = self
            .proxy_for(bus_name, path, "org.a11y.atspi.Accessible")
            .await?;
        let code: u32 = Self::err(p.call("GetRole", &()).await, "GetRole")?;
        let name: String = Self::err(p.call("GetRoleName", &()).await, "GetRoleName")?;
        Ok(AtspiRole { code, name })
    }

    /// Component.GetExtents → 屏幕坐标矩形。
    pub async fn get_extents(&self, bus_name: &str, path: &str) -> Result<Rect> {
        let p = self
            .proxy_for(bus_name, path, "org.a11y.atspi.Component")
            .await?;
        let (x, y, width, height): (i32, i32, i32, i32) = Self::err(
            p.call("GetExtents", &(COORD_TYPE_SCREEN,)).await,
            "Component.GetExtents",
        )?;
        Ok(Rect {
            x,
            y,
            width,
            height,
        })
    }

    // ───────────────────── 树导航 ─────────────────────

    /// GetChildAtIndex(i) → 子引用。
    async fn child_at(&self, bus_name: &str, path: &str, index: i32) -> Result<(String, String)> {
        let p = self
            .proxy_for(bus_name, path, "org.a11y.atspi.Accessible")
            .await?;
        let (bus, opath): (String, OwnedObjectPath) = Self::err(
            p.call("GetChildAtIndex", &(index,)).await,
            "GetChildAtIndex",
        )?;
        Ok((bus, opath.to_string()))
    }

    /// 应用是否实现了某接口（Introspect 探测接口列表；D-Bus 不可达
    /// 时返回 false——能力探测不产生硬错误）。
    pub async fn has_interface_public(&self, bus_name: &str, path: &str, interface: &str) -> bool {
        let Ok(path_obj) = zbus::zvariant::ObjectPath::try_from(path) else {
            return false;
        };
        let proxy = zbus::fdo::IntrospectableProxy::builder(&self.conn)
            .destination(bus_name)
            .and_then(|b| b.path(path_obj));
        let Ok(proxy) = proxy else { return false };
        let Ok(proxy) = proxy.build().await else {
            return false;
        };
        match proxy.introspect().await {
            // 实测 XML 形如 <interface name=\"org.a11y.atspi.Application\">
            // （busctl 返回值内引号被转义；gdbus 解析后为普通引号），
            // 两种形态都匹配。
            //
            // 已知风险（审查记录）：这是对 Introspect XML 文本的字符串
            // 匹配，非严格 XML 解析——理论上注解/文档字符串里若出现
            // `interface name="<名>"` 字样会误报。可接受原因：接口名是
            // 本模块内的编译期字面量、目标 XML 由 at-spi 实现（Qt/GTK）
            // 生成且不含此类文本；引入完整 XML 解析器的复杂度不成比例。
            Ok(xml) => {
                xml.contains(&format!("interface name=\\\"{interface}\\\""))
                    || xml.contains(&format!("interface name=\"{interface}\""))
            }
            Err(_) => false,
        }
    }

    // ───────────────────── 设计 §14.2 三入口 ─────────────────────

    /// 获取桌面根下的全部应用节点（等价设计的 `get_desktop()`；
    /// at-spi2 的 Registry 单桌面对象，根的直接子节点即应用列表）。
    pub async fn list_applications(&self) -> Result<Vec<ApplicationNode>> {
        let count = self.child_count(REGISTRY_SERVICE, ROOT_PATH).await?;
        let mut apps = Vec::with_capacity(count.max(0) as usize);
        for i in 0..count {
            let (bus, path) = self.child_at(REGISTRY_SERVICE, ROOT_PATH, i).await?;
            // 首个子引用可能是 Registry 自身（bus == registry 服务总线），
            // 其 Name 属性为空且无 Application 接口，跳过。
            if !self
                .has_interface_public(&bus, &path, "org.a11y.atspi.Application")
                .await
            {
                continue;
            }
            let name = self.get_name(&bus, &path).await.unwrap_or_default();
            let pid = self.pid_of(&bus).await;
            apps.push(ApplicationNode {
                bus_name: bus,
                path,
                name,
                pid,
            });
        }
        Ok(apps)
    }

    /// 经 DBus daemon 解析总线名的进程 PID。
    async fn pid_of(&self, bus_name: &str) -> Option<u32> {
        let bus: zbus::names::BusName = bus_name.try_into().ok()?;
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await.ok()?;
        dbus.get_connection_unix_process_id(bus).await.ok()
    }

    /// 获取指定 PID 的无障碍树（应用根节点）。
    ///
    /// 找不到时返回 [`AgentShellError::WindowNotFound`]（设计 §14.2）。
    pub async fn get_app_tree(&self, pid: u32) -> Result<ApplicationNode> {
        self.list_applications()
            .await?
            .into_iter()
            .find(|app| app.pid == Some(pid))
            .ok_or_else(|| AgentShellError::WindowNotFound(format!("PID {pid}")))
    }

    /// 枚举一个应用下的全部顶层窗口（FRAME/WINDOW 角色）。
    pub async fn app_windows(&self, app: &ApplicationNode) -> Result<Vec<WindowNode>> {
        let count = self.child_count(&app.bus_name, &app.path).await?;
        let mut windows = Vec::new();
        for i in 0..count {
            let (bus, path) = self.child_at(&app.bus_name, &app.path, i).await?;
            let states = self.get_state(&bus, &path).await?;
            if states.contains(AtspiState::DEFUNCT) {
                continue;
            }
            let role = self.get_role(&bus, &path).await?;
            if !matches!(role.code, 23 | 69) {
                // 23 = FRAME, 69 = WINDOW（AtspiRoleType）
                continue;
            }
            let name = self.get_name(&bus, &path).await.unwrap_or_default();
            windows.push(WindowNode {
                bus_name: bus,
                path,
                name,
                states,
            });
        }
        Ok(windows)
    }

    /// 全桌面窗口枚举（语义定位的搜索空间）。
    pub async fn all_windows(&self) -> Result<Vec<WindowNode>> {
        let mut out = Vec::new();
        for app in self.list_applications().await? {
            out.extend(self.app_windows(&app).await?);
        }
        Ok(out)
    }

    /// 获取当前活动窗口（状态集含 ACTIVE 或 FOCUSED；设计 §14.2）。
    ///
    /// 已知限制（实测 GTK3 atk-bridge on KWin Wayland）：部分工具包
    /// 不向 AT-SPI 传播 ACTIVE/FOCUSED 窗口状态。此时降级为第一个
    /// SHOWING 的顶层窗口并记录 debug 日志——语义定位的搜索空间仍
    /// 正确，只是"活动性"判定精度下降。
    pub async fn get_active_window(&self) -> Result<WindowNode> {
        let windows = self.all_windows().await?;
        for window in &windows {
            if window.states.contains(AtspiState::ACTIVE)
                || window.states.contains(AtspiState::FOCUSED)
            {
                return Ok(window.clone());
            }
        }
        if let Some(window) = windows
            .iter()
            .find(|w| w.states.contains(AtspiState::SHOWING))
        {
            tracing::debug!(
                path = %window.path,
                "no ACTIVE/FOCUSED state exposed by toolkit; falling back to first showing window"
            );
            return Ok(window.clone());
        }
        Err(AgentShellError::WindowNotFound("no active window".into()))
    }

    /// 把 WindowNode 提升为可搜索的 ElementNode 视图。
    pub async fn window_as_element(&self, window: &WindowNode) -> Result<ElementNode> {
        let role = self.get_role(&window.bus_name, &window.path).await?;
        let name = self
            .get_name(&window.bus_name, &window.path)
            .await
            .unwrap_or_default();
        Ok(ElementNode {
            bus_name: window.bus_name.clone(),
            path: window.path.clone(),
            name,
            role,
            states: window.states,
        })
    }

    /// 枚举直接子元素（完整 ElementNode 视图）。
    pub async fn children(&self, node: &ElementNode) -> Result<Vec<ElementNode>> {
        let count = self.child_count(&node.bus_name, &node.path).await?;
        let mut out = Vec::new();
        for i in 0..count {
            let (bus, path) = self.child_at(&node.bus_name, &node.path, i).await?;
            let states = match self.get_state(&bus, &path).await {
                Ok(s) => s,
                // 子节点可能已销毁（DEFUNCT 前 race），跳过而非整体失败
                Err(_) => continue,
            };
            if states.contains(AtspiState::DEFUNCT) {
                continue;
            }
            let (role, name) =
                match tokio::try_join!(self.get_role(&bus, &path), self.get_name(&bus, &path)) {
                    Ok((r, n)) => (r, n),
                    Err(_) => continue,
                };
            out.push(ElementNode {
                bus_name: bus,
                path,
                name,
                role,
                states,
            });
        }
        Ok(out)
    }

    /// a11y 总线是否可用（探测装配用；不触发服务激活）。
    ///
    /// 注意：实际检查的是 session bus 上 `org.a11y.Bus`（[`BUS_SERVICE`])
    /// 是否有 owner——它是 a11y 总线的启动入口，存在即代表 AT-SPI 支持
    /// 已启用；不直接探测 `org.a11y.atspi.Registry`（Registry 在 a11y
    /// 总线上而非 session 总线，直接 name_has_owner 会误判）。
    pub async fn bus_available() -> bool {
        let Ok(session) = zbus::Connection::session().await else {
            return false;
        };
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&session).await else {
            return false;
        };
        // BUS_SERVICE 是编译期常量且为合法总线名；解析失败时保守返回 false
        let Ok(name) = BUS_SERVICE.try_into() else {
            return false;
        };
        matches!(dbus.name_has_owner(name).await, Ok(true))
    }
}

/// 判定是否应调用 `GetAddress` 继续装配（纯函数，无 I/O）。
///
/// 三态（单测覆盖）：
/// - `has_owner` → 继续（owner 已在位，正常路径）；
/// - `!has_owner` 且 [`BUS_SERVICE`] 在 `activatable` 列表 → 继续
///   （可激活，GetAddress 按需启动 accessibility bus）；
/// - `!has_owner` 且不在列表 → 跳过（无 at-spi2-core，避免激活挂满
///   dbus-daemon 默认激活超时 ~120s）。
fn should_probe_address(has_owner: bool, activatable: &[zbus::names::OwnedBusName]) -> bool {
    has_owner || activatable.iter().any(|n| n.as_str() == BUS_SERVICE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_bits_match_atspi_enum() {
        // ACTIVE=1, FOCUSED=12（at-spi-2.0 atspi-constants.h 实测序数）
        assert_eq!(AtspiState::ACTIVE, AtspiState(1 << 1));
        assert_eq!(AtspiState::FOCUSED, AtspiState(1 << 12));
        let s = AtspiState::from_pair(1 << 1 | 1 << 25, 0);
        assert!(s.contains(AtspiState::ACTIVE));
        assert!(s.contains(AtspiState::SHOWING));
        assert!(!s.contains(AtspiState::FOCUSED));
        // 高字位图
        let hi = AtspiState::from_pair(0, 1 << 2); // 位 34
        assert_eq!(hi.0 >> 32, 4);
    }

    #[test]
    fn constants_match_atspi_headers() {
        assert_eq!(REGISTRY_SERVICE, "org.a11y.atspi.Registry");
        assert_eq!(ROOT_PATH, "/org/a11y/atspi/accessible/root");
    }

    #[test]
    fn should_probe_address_three_states() {
        let owned = |name: &str| zbus::names::OwnedBusName::try_from(name).expect("valid bus name");
        // owner 已在位 → 继续（无需查可激活列表）
        assert!(should_probe_address(true, &[]));
        // 无 owner 但可激活 → 继续（GetAddress 懒启动 accessibility bus）
        assert!(should_probe_address(false, &[owned(BUS_SERVICE)]));
        // 无 owner 且不可激活 → 提前返回（无 at-spi2-core，避免 ~120s 停顿）
        assert!(!should_probe_address(
            false,
            &[owned("org.freedesktop.DBus")]
        ));
        assert!(!should_probe_address(false, &[]));
    }
}
