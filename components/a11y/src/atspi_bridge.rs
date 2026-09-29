//! AT-SPI D-Bus 桥接（设计文档 §14.2 `a11y::atspi_bridge`）。
//!
//! 连接目标是 **a11y bus**（非 session bus）：先试已存在的 socket 候选
//! （`AT_SPI_BUS_ADDRESS` → `$XDG_RUNTIME_DIR/at-spi/bus_0` → `bus`），全部
//! 不可达才在 session bus 上调 `org.a11y.Bus.GetAddress()`——只有这一步会
//! 懒激活 a11y bus（激活有会话级副作用，见 [`AtspiBridge`] 类型文档）。
//! Registry 服务名 `org.a11y.atspi.Registry`，桌面根对象路径
//! `/org/a11y/atspi/accessible/root`（at-spi-2.0 atspi-constants.h）。
//!
//! 协议要点（对 at-spi2-registryd / Qt atspi 实现实测）：
//! - Accessible 接口方法为 PascalCase：`GetChildAtIndex`/`GetChildren`/
//!   `GetRole`/`GetRoleName`/`GetState`；属性 `Name`/`ChildCount`。
//! - 子节点引用为 `(bus_name, object_path)` 二元组；应用节点经
//!   DBus daemon `GetConnectionUnixProcessID` 解析 PID。
//! - 状态集是 `(u32, u32)` 位图，位索引即 `AtspiStateType` 枚举值。

use std::future::Future;

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
/// `org.a11y.Status` 接口（session bus 上 AT-SPI 启用开关所在处）。
pub const STATUS_IFACE: &str = "org.a11y.Status";
/// `org.a11y.Status` 所在对象路径（at-spi2-core bus launcher 注册处）。
pub const STATUS_PATH: &str = "/org/a11y/bus";
/// a11y bus 地址环境变量（at-spi2 与工具包共用；未设置时回落 well-known socket）。
pub const AT_SPI_BUS_ADDRESS_ENV: &str = "AT_SPI_BUS_ADDRESS";
/// 坐标类型：屏幕坐标（AT_SPI_COORD_TYPE_SCREEN = 0）。
const COORD_TYPE_SCREEN: u32 = 0;
/// 语义遍历最大深度保护（深层 Web 树可达数十层）。
pub const MAX_TRAVERSE_DEPTH: u8 = 24;
/// session bus 普通方法调用超时（`NameHasOwner`/`ListActivatableNames`
/// 预检），对齐 §19 D-Bus 5s。
pub const SESSION_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// `GetAddress` 懒激活超时例外：首次调用会启动私有 a11y bus（dbus-daemon +
/// registryd），真实会话里需数秒、加载中的会话更久，普通 5s 会在激活完成前
/// 中止调用、留下 stale socket（Connection refused）。放宽到 30s，仍远低于
/// dbus-daemon 激活超时 ~120s。
pub const GET_ADDRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// a11y bus socket 连接超时：stale socket 立即 `ECONNREFUSED`，超时只用于
/// 防对端挂死（socket 存在但无人 accept/认证）。
pub const BUS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 会话 AT-SPI 启用态（`org.a11y.Status`）。
///
/// 工具包（Qt/GTK）据此判定「是否有人消费无障碍」，决定是否向 a11y bus 注册
/// 应用树——默认桌面会话两属性均为 false 时，Registry 可达但**应用树为空**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionA11yStatus {
    /// `IsEnabled`（at-spi2-core 2011+ 的标准启用位）。
    pub is_enabled: bool,
    /// `ScreenReaderEnabled`（读屏软件在跑）。
    pub screen_reader_enabled: bool,
}

impl SessionA11yStatus {
    /// 工具包是否已按「有人消费无障碍」注册。
    ///
    /// 任一属性为真即视为已启用：Qt 同时读 `IsEnabled` 与 `ScreenReaderEnabled`
    /// （`libQt6Gui` 实测引用两者），读屏软件通常置位其一。
    pub fn is_active(self) -> bool {
        self.is_enabled || self.screen_reader_enabled
    }
}

/// AT-SPI D-Bus 桥接。
///
/// 持 session bus 与 a11y bus 两条连接：启用开关（[`STATUS_IFACE`]）只在
/// session bus 上，树查询走 a11y bus。两条连接**惰性建立**——装配期探测
/// 不触碰 D-Bus：激活 a11y bus 是会话级副作用（`$XDG_RUNTIME_DIR/at-spi/bus_0`
/// 按 runtime dir 共享，嵌套 session bus 里激活会把正在服务的 a11y bus
/// 换掉，会话内其它客户端的新连接随即被拒），只有真正使用无障碍时才解析
/// 地址并连接。缓存连接失效（a11y bus 重启）后下次使用自动重连。
/// `Clone` 语义为共享同一连接槽位。
#[derive(Clone)]
pub struct AtspiBridge {
    inner: std::sync::Arc<BridgeInner>,
}

/// 连接槽位：惰性建立 + 失效重连的共享状态。
struct BridgeInner {
    conn: tokio::sync::RwLock<Option<zbus::Connection>>,
    session: tokio::sync::RwLock<Option<zbus::Connection>>,
}

impl BridgeInner {
    fn new() -> Self {
        Self {
            conn: tokio::sync::RwLock::new(None),
            session: tokio::sync::RwLock::new(None),
        }
    }
}

impl AtspiBridge {
    /// 延迟连接句柄：不触碰 D-Bus；首次 [`Self::ensure_connected`] 才建立连接。
    pub fn deferred() -> Self {
        Self {
            inner: std::sync::Arc::new(BridgeInner::new()),
        }
    }

    /// 连接 a11y bus（等价 `deferred()` 后立即 [`Self::ensure_connected`]）。
    ///
    /// 失败（session bus 不通、a11y bus 不可达、`org.a11y.Bus` 缺失）
    /// 统一归一为 [`AgentShellError::BackendUnavailable`]——装配层据此
    /// 判定组件 Unavailable 而非崩溃。
    pub async fn connect() -> Result<Self> {
        let bridge = Self::deferred();
        bridge.ensure_connected().await?;
        Ok(bridge)
    }

    /// 确保两条连接就绪：已连接且未失效时复用，否则（重）连接。
    pub async fn ensure_connected(&self) -> Result<()> {
        let _ = self.session().await?;
        self.conn().await.map(|_| ())
    }

    /// 共享连接槽位的克隆构造（component.rs 的 Arc 包装用）。
    pub fn clone_bridge(&self) -> Self {
        self.clone()
    }

    /// session bus 连接（惰性建立 + 失效重连）。
    async fn session(&self) -> Result<zbus::Connection> {
        let mut slot = self.inner.session.write().await;
        if let Some(conn) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(conn.clone());
        }
        let conn = zbus::connection::Builder::session()
            .map_err(|e| AgentShellError::BackendUnavailable(format!("session bus: {e}")))?
            .build()
            .await
            .map_err(|e| AgentShellError::BackendUnavailable(format!("session bus: {e}")))?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    /// a11y bus 连接（惰性建立 + 失效重连；地址解析见 [`dial_a11y`]）。
    async fn conn(&self) -> Result<zbus::Connection> {
        let mut slot = self.inner.conn.write().await;
        if let Some(conn) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(conn.clone());
        }
        let session = self.session().await?;
        let conn = dial_a11y(&session).await?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    /// 读会话 AT-SPI 启用态（session bus [`STATUS_IFACE`]）。
    ///
    /// 属性缺失（老 at-spi2 无 `IsEnabled`）或读取失败按 false 处理——调用方
    /// [`Self::ensure_enabled`] 据此尝试置位，失败也不阻断树查询。
    pub async fn session_status(&self) -> Result<SessionA11yStatus> {
        let session = self.session().await?;
        let proxy = zbus::Proxy::new(&session, BUS_SERVICE, STATUS_PATH, STATUS_IFACE)
            .await
            .map_err(|e| AgentShellError::DBus(format!("org.a11y.Status proxy: {e}")))?;
        Ok(SessionA11yStatus {
            is_enabled: proxy
                .get_property::<bool>("IsEnabled")
                .await
                .unwrap_or(false),
            screen_reader_enabled: proxy
                .get_property::<bool>("ScreenReaderEnabled")
                .await
                .unwrap_or(false),
        })
    }

    /// 确保会话 AT-SPI 已启用，返回本次是否发生变更（`true` = 刚置位）。
    ///
    /// 默认桌面会话（实测 KDE Plasma 6 Wayland）两属性均为 false，工具包因此
    /// **不向 a11y bus 注册**，Registry 可达却应用树为空——`a11y query --all`
    /// 恒返回 0 元素。该属性正是读屏软件启动时置位的开关；本工具读屏/语义定位
    /// 与读屏同源，故在枚举前置位（调用方据此决定是否等待注册完成）。
    ///
    /// 已置位时不写总线（幂等且不扰动既有状态）；写入失败（属性不存在 /
    /// 权限拒绝）返回错误，由调用方决定是否继续枚举。
    pub async fn ensure_enabled(&self) -> Result<bool> {
        if self.session_status().await?.is_active() {
            return Ok(false);
        }
        let session = self.session().await?;
        let props = zbus::Proxy::new(
            &session,
            BUS_SERVICE,
            STATUS_PATH,
            "org.freedesktop.DBus.Properties",
        )
        .await
        .map_err(|e| AgentShellError::DBus(format!("org.a11y.Status props proxy: {e}")))?;
        let mut wrote = false;
        let mut last_err = None;
        for prop in ["IsEnabled", "ScreenReaderEnabled"] {
            match props
                .call::<_, _, ()>(
                    "Set",
                    &(STATUS_IFACE, prop, zbus::zvariant::Value::Bool(true)),
                )
                .await
            {
                Ok(()) => wrote = true,
                Err(e) => {
                    // 单个属性缺失不致命（不同 at-spi2 版本属性集不同），
                    // 但两个都写不进来说明无法启用——如实上抛。
                    tracing::debug!(prop, "org.a11y.Status set failed: {e}");
                    last_err = Some(e);
                }
            }
        }
        match (wrote, last_err) {
            (true, _) => Ok(true),
            (false, Some(e)) => Err(AgentShellError::DBus(format!(
                "org.a11y.Status enable failed: {e}"
            ))),
            (false, None) => Ok(false),
        }
    }

    fn err<T>(r: zbus::Result<T>, what: &'static str) -> Result<T> {
        r.map_err(|e| map_call_error(e, what))
    }

    /// 读路径重试：连接层失败时清空连接槽位并整体重试一次。
    ///
    /// a11y bus 重启后 zbus 异步置 `closed`：在该窗口内复用缓存连接会一直
    /// 失败（实测 `Broken pipe`）。这里不依赖 zbus 的时序，见错误即清槽重连。
    /// **只用于幂等读操作**——元素动作（click/set_text）重试可能重复投递。
    async fn retry_on_connection_loss<T, F, Fut>(&self, mut op: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        match op().await {
            Err(e) if self.should_reconnect(&e).await => {
                tracing::debug!("atspi connection lost; reconnecting and retrying once");
                *self.inner.conn.write().await = None;
                op().await
            }
            other => other,
        }
    }

    /// 是否清槽重连：已建立过连接，且（连接已关闭 或 错误来自连接层——
    /// [`map_call_error`] 把连接层失败归一为 `BackendUnavailable`）。
    async fn should_reconnect(&self, e: &AgentShellError) -> bool {
        let cached_closed = self
            .inner
            .conn
            .read()
            .await
            .as_ref()
            .map(|conn| conn.is_closed());
        should_clear_connection(
            cached_closed,
            matches!(e, AgentShellError::BackendUnavailable(_)),
        )
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
        let conn = self.conn().await?;
        let b = zbus::proxy::Builder::<zbus::Proxy<'a>>::new(&conn)
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
            .map_err(|e| map_call_error(e, what))?;
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
        let Ok(conn) = self.conn().await else {
            return false;
        };
        let proxy = zbus::fdo::IntrospectableProxy::builder(&conn)
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
        self.retry_on_connection_loss(|| self.list_applications_once())
            .await
    }

    /// [`Self::list_applications`] 本体（无重试，避免与包装层互相递归）。
    async fn list_applications_once(&self) -> Result<Vec<ApplicationNode>> {
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
        let conn = self.conn().await.ok()?;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.ok()?;
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
    ///
    /// 单个子节点的属性探测失败（节点已销毁，或实现不完整——AT-SPI 不要求
    /// 每个节点实现全部接口，实测有节点不实现 `GetState`/`GetRole`）时跳过该
    /// 节点，而非让整棵树枚举失败：与 [`Self::children`] 同口径，语义定位的
    /// 搜索空间只损失不可读节点。
    pub async fn app_windows(&self, app: &ApplicationNode) -> Result<Vec<WindowNode>> {
        let count = self.child_count(&app.bus_name, &app.path).await?;
        let mut windows = Vec::new();
        for i in 0..count {
            let (bus, path) = self.child_at(&app.bus_name, &app.path, i).await?;
            let states = match self.get_state(&bus, &path).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(%bus, %path, "atspi node state unavailable, skipping: {e}");
                    continue;
                }
            };
            if states.contains(AtspiState::DEFUNCT) {
                continue;
            }
            let role = match self.get_role(&bus, &path).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(%bus, %path, "atspi node role unavailable, skipping: {e}");
                    continue;
                }
            };
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
        self.retry_on_connection_loss(|| self.all_windows_once())
            .await
    }

    /// [`Self::all_windows`] 本体（无重试——内层一律走 `*_once`，避免重试叠加）。
    async fn all_windows_once(&self) -> Result<Vec<WindowNode>> {
        let mut out = Vec::new();
        for app in self.list_applications_once().await? {
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
        self.retry_on_connection_loss(|| self.children_once(node))
            .await
    }

    /// [`Self::children`] 本体（无重试）。
    async fn children_once(&self, node: &ElementNode) -> Result<Vec<ElementNode>> {
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

    /// Registry 是否**实际可达**（**被动**：不激活 a11y bus，fail-closed）。
    ///
    /// 有存活连接时用该连接上的注册结果；否则被动连接 a11y bus（socket 候选 →
    /// 已注册 launcher 的 GetAddress，均不激活）后查注册。两者皆无（a11y bus
    /// 仅「可激活」未启动 / 候选与地址都不可达）→ `false`：契约
    /// [`A11yComponent::registry_available`] 问的是「Registry 是否可达」，不能用
    /// 「支持存在」作答——与 daemon 探测同状态报 `a11y bus not started` 一致。
    ///
    /// [`A11yComponent::registry_available`]: agent_shell_core::component::A11yComponent::registry_available
    pub async fn registry_reachable(&self) -> bool {
        let cached = self.cached_registry_owned().await;
        let probed = match cached {
            Some(_) => None,
            None => match self.passive_bus().await {
                Some(bus) => Self::registry_owned(&bus).await,
                None => None,
            },
        };
        registry_reachability(cached, probed)
    }

    /// 缓存连接上的 Registry 注册查询（`None` = 无存活连接）。
    async fn cached_registry_owned(&self) -> Option<bool> {
        let conn = self
            .inner
            .conn
            .read()
            .await
            .clone()
            .filter(|c| !c.is_closed())?;
        Self::registry_owned(&conn).await
    }

    /// 被动连接 a11y bus（不激活）：socket 候选 → 已注册 launcher 的地址。
    async fn passive_bus(&self) -> Option<zbus::Connection> {
        let session = self.session().await.ok()?;
        Self::dial_a11y_bus(&session, LauncherActivation::Forbidden)
            .await
            .connected()
    }

    /// a11y bus 上 Registry 名称是否已注册（`None` = 查询失败）。
    async fn registry_owned(conn: &zbus::Connection) -> Option<bool> {
        let dbus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
        let name = REGISTRY_SERVICE.try_into().ok()?;
        dbus.name_has_owner(name).await.ok()
    }

    /// 读 `org.a11y.Bus` 注册状态（被动：`NameHasOwner` +
    /// `ListActivatableNames`；`None` = session bus 不可达）。
    ///
    /// 装配期只依赖本判定（[`crate::component::AtSpiComponent::probe`]），
    /// 不激活 a11y bus。
    pub async fn launcher_state() -> Option<LauncherState> {
        let session = zbus::Connection::session().await.ok()?;
        Self::launcher_state_on(&session).await
    }

    /// 同 [`Self::launcher_state`]，复用调用方已有的 session bus 连接
    /// （探测路径避免再开一条连接）。
    pub async fn launcher_state_on(session: &zbus::Connection) -> Option<LauncherState> {
        let dbus = zbus::fdo::DBusProxy::new(session).await.ok()?;
        // BUS_SERVICE 是编译期常量且为合法总线名；解析失败时保守返回 None
        let name = BUS_SERVICE.try_into().ok()?;
        let has_owner = dbus.name_has_owner(name).await.ok()?;
        let activatable = if has_owner {
            false
        } else {
            dbus.list_activatable_names()
                .await
                .ok()?
                .iter()
                .any(|n| n.as_str() == BUS_SERVICE)
        };
        Some(LauncherState {
            has_owner,
            activatable,
        })
    }

    /// 解析并连接 a11y bus（被动优先）：socket 候选（`AT_SPI_BUS_ADDRESS` /
    /// `$XDG_RUNTIME_DIR/at-spi/{bus_0,bus}`）→ 已注册 launcher 的 GetAddress → dial。
    ///
    /// doctor 与桥接共用本函数，只有文案各自差异化。`activation` 决定 launcher
    /// 仅「可激活」时是否允许 `GetAddress` 把它拉起：装配/探测传
    /// [`LauncherActivation::Forbidden`]（不产生副作用），使用路径传
    /// [`LauncherActivation::Allowed`]。
    pub async fn dial_a11y_bus(
        session: &zbus::Connection,
        activation: LauncherActivation,
    ) -> A11yBusOutcome {
        let mut candidate_err = None;
        for addr in bus_address_candidates() {
            match dial(&addr).await {
                Ok(conn) => return A11yBusOutcome::Connected(conn),
                Err(e) => candidate_err = Some(e),
            }
        }
        match launcher_address(session, activation).await {
            Ok(address) => match dial(&address).await {
                Ok(conn) => A11yBusOutcome::Connected(conn),
                Err(reason) => A11yBusOutcome::Unreachable { address, reason },
            },
            Err(LauncherAddressError::NotStarted) => A11yBusOutcome::NotStarted,
            Err(LauncherAddressError::Unsupported) => A11yBusOutcome::Unsupported,
            Err(LauncherAddressError::Failed(reason)) => A11yBusOutcome::AddressUnavailable {
                reason: match candidate_err {
                    Some(candidate_err) => {
                        format!("{reason}; candidates also unreachable ({candidate_err})")
                    }
                    None => reason,
                },
            },
        }
    }
}

/// Registry 可达性判定（被动路径；纯函数，单测锚定 fail-closed 契约）：
/// 优先用存活连接的查询结果，其次用被动探测到的连接，两者皆无 → 不可达。
fn registry_reachability(cached: Option<bool>, probed: Option<bool>) -> bool {
    cached.or(probed).unwrap_or(false)
}

/// 是否清槽重连（纯函数，单测锚定重试边界）：
/// - 从未建立过连接（`None`）→ 不重连（连接失败交给调用方原样上抛）；
/// - 连接已关闭 或 错误来自连接层 → 清槽重连重试一次。
fn should_clear_connection(cached_closed: Option<bool>, error_is_connection: bool) -> bool {
    match cached_closed {
        Some(closed) => closed || error_is_connection,
        None => false,
    }
}

/// session bus 上 `org.a11y.Bus` 的注册状态（`None` = session bus 不可达）。
#[derive(Clone, Copy)]
pub struct LauncherState {
    /// `org.a11y.Bus` 已注册（a11y bus 正在运行或已启动过）。
    pub has_owner: bool,
    /// `org.a11y.Bus` 可被 D-Bus 激活（首次使用时按需启动 a11y bus）。
    pub activatable: bool,
}

impl LauncherState {
    /// AT-SPI 支持是否存在（已注册或可激活，二者之一即可按需提供 a11y bus）。
    pub fn is_supported(self) -> bool {
        self.has_owner || self.activatable
    }
}

/// a11y bus 地址候选（被动解析，不触发激活）：`AT_SPI_BUS_ADDRESS` →
/// `$XDG_RUNTIME_DIR/at-spi/bus_0`（现代 at-spi2 socket 名）→
/// `$XDG_RUNTIME_DIR/at-spi/bus`（旧版兜底）。
///
/// 工具包同样优先读 `AT_SPI_BUS_ADDRESS`；命中存活的 socket 即可直接连接，
/// 无需向 session bus 上的 `org.a11y.Bus` 申请地址（那会懒激活 a11y bus）。
pub fn bus_address_candidates() -> Vec<String> {
    candidates_from(
        std::env::var(AT_SPI_BUS_ADDRESS_ENV).ok(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

/// 候选构造（纯函数：单测不触碰进程环境，`lib.rs` 禁止 unsafe）。
fn candidates_from(env_addr: Option<String>, runtime_dir: Option<&str>) -> Vec<String> {
    let mut addrs = Vec::new();
    if let Some(addr) = env_addr {
        addrs.push(addr);
    }
    if let Some(runtime) = runtime_dir {
        addrs.push(format!("unix:path={runtime}/at-spi/bus_0"));
        addrs.push(format!("unix:path={runtime}/at-spi/bus"));
    }
    addrs
}

/// 未启动的 a11y bus 可否由本次调用按需拉起（`GetAddress` 懒激活）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LauncherActivation {
    /// 使用路径（首次真正用无障碍）：允许——按需启动 a11y bus 是既有语义。
    Allowed,
    /// 装配/探测路径：禁止——激活是会话级副作用，见 [`AtspiBridge`] 类型文档。
    Forbidden,
}

/// a11y bus 连接尝试的结果（分级）。
///
/// doctor 与桥接共用同一条解析链（[`dial_a11y_bus`]），只有文案各自差异化——
/// 两条链各自演进会让「doctor 报告的总线」与「a11y 查询用的总线」漂移。
pub enum A11yBusOutcome {
    /// 已连接（socket 候选命中，或 launcher 给出地址后连上）。
    Connected(zbus::Connection),
    /// launcher 地址解析失败（预检或 `GetAddress` 出错/超时）：阶段 = 地址解析。
    AddressUnavailable { reason: String },
    /// 地址已解析但该地址连不上：阶段 = dial（socket 已被替换或遗留）。
    Unreachable { address: String, reason: String },
    /// launcher 未注册但可激活：a11y bus 未启动（调用方可按需启动）。
    NotStarted,
    /// launcher 未注册且不可激活：无 at-spi2-core（或其 a11y 被禁用）。
    Unsupported,
}

impl A11yBusOutcome {
    /// 已连接时返回连接（zbus 连接是 Arc 语义的浅拷贝）。
    pub fn connected(&self) -> Option<zbus::Connection> {
        match self {
            Self::Connected(conn) => Some(conn.clone()),
            _ => None,
        }
    }

    /// 失败归因（分级文案；`Connected` 为 `None`）。调用方按需追加处置提示。
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Connected(_) => None,
            Self::AddressUnavailable { reason } => {
                Some(format!("a11y bus address resolution failed ({reason})"))
            }
            Self::Unreachable { address, reason } => {
                Some(format!("a11y bus unreachable at {address} ({reason})"))
            }
            Self::NotStarted => Some(format!(
                "a11y bus not started ({BUS_SERVICE} starts on demand)"
            )),
            Self::Unsupported => Some(format!(
                "{BUS_SERVICE} not registered and not activatable (at-spi2-core missing?)"
            )),
        }
    }
}

/// [`dial_a11y_bus`] 的 Result 版本（桥接内部使用）：失败归一为
/// [`AgentShellError::BackendUnavailable`]（带分级归因）。
async fn dial_a11y(session: &zbus::Connection) -> Result<zbus::Connection> {
    let outcome = AtspiBridge::dial_a11y_bus(session, LauncherActivation::Allowed).await;
    outcome.connected().ok_or_else(|| {
        AgentShellError::BackendUnavailable(
            outcome
                .reason()
                .unwrap_or_else(|| "a11y bus unavailable".into()),
        )
    })
}

/// 按地址建立 a11y bus 连接（有界超时：stale socket 立即 ECONNREFUSED，
/// 但对端挂死时不能无限等待）。
async fn dial(address: &str) -> std::result::Result<zbus::Connection, String> {
    let builder = zbus::connection::Builder::address(address)
        .map_err(|e| format!("a11y bus addr {address}: {e}"))?;
    tokio::time::timeout(BUS_CONNECT_TIMEOUT, builder.build())
        .await
        .map_err(|_| format!("a11y bus dial timed out: {address}"))?
        .map_err(|e| format!("a11y bus dial {address}: {e}"))
}

/// `GetAddress` 上限：launcher 未注册时可能懒激活（[`GET_ADDRESS_TIMEOUT`]
/// 的 30s 例外）；已注册时不可能发生激活，健康 launcher 毫秒级应答、挂死的
/// launcher 也不该拖满 30s，用普通调用上限（纯函数，单测覆盖）。
fn get_address_timeout(has_owner: bool) -> std::time::Duration {
    if has_owner {
        SESSION_CALL_TIMEOUT
    } else {
        GET_ADDRESS_TIMEOUT
    }
}

/// launcher 地址解析失败的分级原因。
enum LauncherAddressError {
    /// 未注册但可激活：a11y bus 未启动（激活被本次调用禁止或调用方不激活）。
    NotStarted,
    /// 未注册且不可激活：无 at-spi2-core。
    Unsupported,
    /// 调用失败（预检或 `GetAddress` 出错/超时）。
    Failed(String),
}

/// session bus `org.a11y.Bus.GetAddress()`（返回 a11y bus 地址）。
async fn launcher_address(
    session: &zbus::Connection,
    activation: LauncherActivation,
) -> std::result::Result<String, LauncherAddressError> {
    let dbus = zbus::fdo::DBusProxy::new(session)
        .await
        .map_err(|e| LauncherAddressError::Failed(format!("DBusProxy: {e}")))?;
    let bus_name = BUS_SERVICE
        .try_into()
        .map_err(|e| LauncherAddressError::Failed(format!("bad bus name: {e}")))?;
    let has_owner = call_bounded(
        dbus.name_has_owner(bus_name),
        SESSION_CALL_TIMEOUT,
        "NameHasOwner",
    )
    .await
    .map_err(|e| LauncherAddressError::Failed(e.to_string()))?;

    if !has_owner {
        let activatable: Vec<zbus::names::OwnedBusName> = call_bounded(
            dbus.list_activatable_names(),
            SESSION_CALL_TIMEOUT,
            "ListActivatableNames",
        )
        .await
        .map_err(|e| LauncherAddressError::Failed(e.to_string()))?;
        if !should_probe_address(has_owner, &activatable) {
            return Err(LauncherAddressError::Unsupported);
        }
        if activation == LauncherActivation::Forbidden {
            // `GetAddress` 会把 launcher 拉起来——探测/装配不做这种事。
            return Err(LauncherAddressError::NotStarted);
        }
    }

    let bus = zbus::Proxy::new(session, BUS_SERVICE, "/org/a11y/bus", "org.a11y.Bus")
        .await
        .map_err(|e| LauncherAddressError::Failed(format!("{BUS_SERVICE} proxy: {e}")))?;
    // 返回签名是 s（实测 busctl），不是 v
    call_bounded(
        bus.call("GetAddress", &()),
        get_address_timeout(has_owner),
        "org.a11y.Bus.GetAddress",
    )
    .await
    .map_err(|e| LauncherAddressError::Failed(e.to_string()))
}

/// zbus 调用错误 → [`AgentShellError`]：
/// 连接层失败归一为 [`AgentShellError::BackendUnavailable`]（可区分、可重连），
/// 其余保留 `DBus` 错误。
///
/// 连接层失败必须可区分——读路径据此清空连接槽位并重连重试
/// （见 `AtspiBridge::retry_on_connection_loss`）。
fn map_call_error(e: zbus::Error, what: &'static str) -> AgentShellError {
    if is_connection_loss(&e) {
        AgentShellError::BackendUnavailable(format!("atspi {what}: {e}"))
    } else {
        AgentShellError::DBus(format!("atspi {what}: {e}"))
    }
}

/// 连接层失败判定：socket I/O 错误、连接建立失败、握手失败都说明当前连接
/// 不可再用（zbus 的 `is_closed` 随后置位，但调用方不应依赖该时序）。
fn is_connection_loss(e: &zbus::Error) -> bool {
    matches!(
        e,
        zbus::Error::InputOutput(_) | zbus::Error::Connection(..) | zbus::Error::Handshake(_)
    )
}

/// 有界 D-Bus 方法调用：超时或总线错误统一归一为 [`AgentShellError::BackendUnavailable`]。
///
/// zbus 5 只在连接级提供 `method_timeout`，无法对单个调用设独立超时；而
/// `GetAddress` 懒激活需 30s、预检只需 5s，故逐调用用 tokio 超时包裹。
async fn call_bounded<F, T, E>(
    fut: F,
    timeout: std::time::Duration,
    what: &'static str,
) -> Result<T>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| AgentShellError::BackendUnavailable(format!("{what} timed out")))?
        .map_err(|e| AgentShellError::BackendUnavailable(format!("{what}: {e}")))
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
    fn timeout_constants_anchor_atspi_activation_contract() {
        // 预检沿用 §19 D-Bus 5s；GetAddress 懒激活例外放宽到 30s，须严格大于
        // 预检且远低于 dbus-daemon 激活超时 ~120s——两者同取 5s 会过早中止
        // 激活（回归）。
        assert_eq!(SESSION_CALL_TIMEOUT, std::time::Duration::from_secs(5));
        assert_eq!(GET_ADDRESS_TIMEOUT, std::time::Duration::from_secs(30));
        assert!(GET_ADDRESS_TIMEOUT > SESSION_CALL_TIMEOUT);
    }

    #[test]
    fn candidates_prefer_env_then_well_known_sockets() {
        assert_eq!(
            candidates_from(
                Some("unix:path=/tmp/custom-bus".to_string()),
                Some("/run/user/1000")
            ),
            vec![
                "unix:path=/tmp/custom-bus".to_string(),
                "unix:path=/run/user/1000/at-spi/bus_0".to_string(),
                "unix:path=/run/user/1000/at-spi/bus".to_string(),
            ]
        );
        // 无环境变量 / 无 runtime dir（TTY、容器）时不产出候选
        assert_eq!(candidates_from(None, Some("/run/user/1000")).len(), 2);
        assert!(candidates_from(None, None).is_empty());
    }

    /// 支持判定：已注册或可激活即支持（装配期据此决定是否构造组件，
    /// 不激活服务）。
    #[test]
    fn launcher_state_support_requires_owner_or_activatable() {
        let state = |has_owner, activatable| LauncherState {
            has_owner,
            activatable,
        };
        assert!(state(true, false).is_supported());
        assert!(state(false, true).is_supported());
        assert!(!state(false, false).is_supported());
    }

    /// `GetAddress` 上限按「是否可能懒激活」选择：已注册的 launcher 不会触发
    /// 激活（健康时毫秒级应答，挂死也不该拖满 30s），未注册才用 30s 例外。
    #[test]
    fn get_address_timeout_follows_activation_possibility() {
        assert_eq!(get_address_timeout(true), SESSION_CALL_TIMEOUT);
        assert_eq!(get_address_timeout(false), GET_ADDRESS_TIMEOUT);
        assert!(get_address_timeout(true) < get_address_timeout(false));
    }

    /// 重试边界：从未连上（`None`）不重试——连接失败交由调用方上抛；已建立过
    /// 连接后，连接已关闭或连接层错误才清槽重连；语义错误不重试。
    #[test]
    fn reconnect_only_after_established_connection_is_lost() {
        assert!(!should_clear_connection(None, true));
        assert!(!should_clear_connection(None, false));
        assert!(should_clear_connection(Some(true), false));
        assert!(should_clear_connection(Some(false), true));
        assert!(!should_clear_connection(Some(false), false));
    }

    /// Registry 可达性 fail-closed：无存活连接、被动探测也无 bus（a11y bus 仅
    /// 可激活、未启动）→ 不可达；有证据时按证据作答。
    #[test]
    fn registry_reachability_fails_closed_without_reachable_bus() {
        assert!(!registry_reachability(None, None));
        assert!(registry_reachability(Some(true), None));
        assert!(!registry_reachability(Some(false), None));
        assert!(registry_reachability(None, Some(true)));
        assert!(!registry_reachability(None, Some(false)));
    }

    /// 失败归因分级：三种失败各自可辨，doctor 的状态行据此归因。
    #[test]
    fn bus_outcome_reasons_are_stage_specific() {
        let unreachable = A11yBusOutcome::Unreachable {
            address: "unix:path=/run/user/1000/at-spi/bus_0".to_string(),
            reason: "a11y bus dial: Connection refused".to_string(),
        };
        assert_eq!(
            unreachable.reason().as_deref(),
            Some(
                "a11y bus unreachable at unix:path=/run/user/1000/at-spi/bus_0 \
                 (a11y bus dial: Connection refused)"
            )
        );
        assert_eq!(
            A11yBusOutcome::AddressUnavailable {
                reason: "NameHasOwner(org.a11y.Bus) timed out".to_string(),
            }
            .reason()
            .as_deref(),
            Some(
                "a11y bus address resolution failed \
                 (NameHasOwner(org.a11y.Bus) timed out)"
            )
        );
        assert_eq!(
            A11yBusOutcome::NotStarted.reason().as_deref(),
            Some("a11y bus not started (org.a11y.Bus starts on demand)")
        );
        assert_eq!(
            A11yBusOutcome::Unsupported.reason().as_deref(),
            Some("org.a11y.Bus not registered and not activatable (at-spi2-core missing?)")
        );
    }

    /// 私有 session bus：用例内自建（`dbus-daemon --print-address`），使断言
    /// 不依赖宿主 session bus——CI（无 session bus）下同样执行。
    struct PrivateBus {
        child: std::process::Child,
        address: String,
    }

    impl PrivateBus {
        /// 启动私有 session bus；`address` 显式给定时改用该地址（用例借此构造
        /// 启动失败：不可绑定的路径 → daemon 立即退出、stdout 无地址行）。
        ///
        /// `child` 先移入 `Self`（带 `Drop`）再读取地址：地址行读不到而提前
        /// 返回时同样走 Drop 的 kill + wait + unlink，不留存活进程、zombie 或
        /// stale socket。启动失败（缺 `dbus-daemon`）返回 `None`，由调用方输出
        /// 可见 SKIP 行。
        fn start_with(address: Option<&str>) -> Option<Self> {
            let mut command = std::process::Command::new("dbus-daemon");
            command
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            if let Some(address) = address {
                command.arg(format!("--address={address}"));
            }
            let mut bus = Self {
                child: command.spawn().ok()?,
                address: String::new(),
            };
            let mut stdout = bus.child.stdout.take()?;
            bus.address = read_address_line(&mut stdout)?;
            (!bus.address.is_empty()).then_some(bus)
        }

        fn start() -> Option<Self> {
            Self::start_with(None)
        }

        async fn connect(&self) -> zbus::Connection {
            zbus::connection::Builder::address(self.address.as_str())
                .expect("private bus address must parse")
                .build()
                .await
                .expect("connect to private bus")
        }

        /// 地址中的 socket 路径（`unix:path=<p>[,guid=…]`）。
        fn socket_path(&self) -> Option<&str> {
            let rest = self.address.strip_prefix("unix:path=")?;
            Some(rest.split(',').next().unwrap_or(rest))
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            // `lib.rs` 禁止 unsafe（不能用 libc 发 SIGTERM），而 SIGKILL 跳过
            // dbus-daemon 的清理路径——显式 unlink 其 socket，避免在 /tmp 累积
            // stale /tmp/dbus-* 文件。
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(path) = self.socket_path() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// 逐字节读地址行（`--print-address=1` 恰好一行；缓存读会吞掉后续输出）。
    fn read_address_line(stdout: &mut std::process::ChildStdout) -> Option<String> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(_) if buf[0] == b'\n' => break,
                Ok(_) => bytes.push(buf[0]),
                Err(_) => return None,
            }
        }
        String::from_utf8(bytes).ok()
    }

    /// 本进程当前处于 zombie 态的子进程 PID（`/proc` 扫描；不可读项跳过）。
    fn zombie_children() -> Vec<u32> {
        let me = std::process::id();
        let mut zombies = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return zombies;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // 字段：pid (comm) state ppid …；comm 可含空格/括号，故从其最后一个
            // ')' 之后再切分。
            let Some((_, rest)) = stat.rsplit_once(')') else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next().unwrap_or("");
            let ppid = fields.next().and_then(|p| p.parse::<u32>().ok());
            if state == "Z" && ppid == Some(me) {
                zombies.push(pid);
            }
        }
        zombies
    }

    /// 启动失败路径必须收尾干净：地址行读不到（daemon 启动即失败退出）时
    /// `child` 已在带 `Drop` 的守卫里——kill + wait 之后不得留下 zombie
    /// 子进程（`std::process::Child` 自身没有 Drop，未 wait 的子进程会变
    /// zombie 直到本进程退出）。
    #[test]
    fn private_bus_start_failure_is_reaped() {
        if std::process::Command::new("dbus-daemon")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: dbus-daemon unavailable; private_bus_start_failure_is_reaped");
            return;
        }
        // 先留出窗口让并行用例各自 spawn 的 bus 完成 kill + wait，再取基线。
        std::thread::sleep(std::time::Duration::from_millis(300));
        let baseline = zombie_children();

        // 目录不存在的地址无法 bind：daemon 打印到 stderr 后立即退出，stdout 无
        // 地址行 → start_with 返回 None（并走 Drop 收尾）。
        let failed = PrivateBus::start_with(Some("unix:path=/nonexistent-dir-agent-shell/bus"));
        assert!(failed.is_none(), "不可绑定的地址必须启动失败（无地址行）");

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            zombie_children(),
            baseline,
            "启动失败路径不得留下 zombie 子进程"
        );
    }

    /// 读路径重试的观测行为：连接层失败时清空连接槽位并整体重试一次。
    ///
    /// 缓存槽里放一条**已关闭**的连接（`close()`），op 恒失败于连接层错误——
    /// 断言调用两次且槽位被清空。连接取自用例内自建的私有总线，任何环境
    /// （含无 session bus 的 CI 门禁）都执行断言；缺 `dbus-daemon` 时输出可见
    /// SKIP 行而非静默通过。
    #[tokio::test]
    async fn retry_on_connection_loss_clears_slot_and_retries_once() {
        let Some(bus) = PrivateBus::start() else {
            eprintln!(
                "SKIP: dbus-daemon unavailable; \
                 retry_on_connection_loss_clears_slot_and_retries_once"
            );
            return;
        };
        let conn = bus.connect().await;
        let bridge = AtspiBridge::deferred();
        let probe = conn.clone();
        conn.close().await.expect("close private bus connection");
        assert!(probe.is_closed(), "closed connection must report closed");
        *bridge.inner.conn.write().await = Some(probe);

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out: Result<u32> = bridge
            .retry_on_connection_loss(|| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async {
                    Err(AgentShellError::BackendUnavailable(
                        "atspi ChildCount: I/O error: Broken pipe".into(),
                    ))
                }
            })
            .await;
        assert!(out.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "连接层失败应清槽重连后整体重试一次"
        );
        assert!(
            bridge.inner.conn.read().await.is_none(),
            "失效连接必须被清槽，后续调用才不复用 stale 连接"
        );
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

    /// 会话启用态判定：任一属性为真即视为「工具包会注册」——Qt 同时读
    /// `IsEnabled` 与 `ScreenReaderEnabled`（libQt6Gui 实测引用两者），
    /// 只认其中一个会把另一种置位方式的会话误判为未启用（进而重复写总线）。
    #[test]
    fn session_status_is_active_on_either_flag() {
        let s = |is_enabled, screen_reader_enabled| SessionA11yStatus {
            is_enabled,
            screen_reader_enabled,
        };
        assert!(!s(false, false).is_active());
        assert!(s(true, false).is_active());
        assert!(s(false, true).is_active());
        assert!(s(true, true).is_active());
    }
}
