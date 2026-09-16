//! KWin 原生 D-Bus 通道（设计文档 §7.7 / §21.36.2，`native.rs`）。
//!
//! KWin 5.x 的 Wayland 会话可能不注册 `/Scripting` 对象路径（DDE 20 /
//! uos-PC 实测 kwin_wayland 5.27.2），此时 Scripting 桥接的 `loadScript`
//! 整体不可用。本模块提供不依赖 Scripting 的原生 `org.kde.KWin` 通道：
//!
//! - 窗口信息：`org.kde.KWin` `/KWin` 的 `getWindowInfo(uuid) -> a{sv}`
//!   （KWin 5.27 起存在；KWin 5.0 仅 `queryWindowInfo` 交互选窗，不可用于枚举）；
//! - 工作区：`org.kde.KWin.VirtualDesktopManager` 的 `desktops`（`a(uss)`，
//!   每项 `(position, id, name)`）与 `current`（当前桌面 id）属性。
//!
//! 窗口 uuid 枚举不由本模块承担——由 `wayland::WindowManagement` 的
//! `get_stacking_order`（`org_kde_plasma_window_management` v17+）经
//! wl_registry 枚举，本模块按 uuid 回读详情，二者组合等价于 Scripting
//! `list_windows.js` 的 `workspace.windowList()`。

use std::collections::HashMap;

use agent_shell_core::types::{DesktopEnvironment, Rect, WindowId, WindowInfo, WindowState};
use zbus::zvariant::{ObjectPath, OwnedValue};
use zbus::{Connection, Proxy};

use crate::error::{KWinError, Result};

/// `org.kde.KWin` 原生接口对象路径（`/KWin`，与 Scripting 的 `/Scripting` 并列）。
pub const KWIN_PATH: &str = "/KWin";
/// `org.kde.KWin` 原生接口名。
pub const KWIN_IFACE: &str = "org.kde.KWin";
/// `org.kde.KWin.VirtualDesktopManager` 对象路径（KWin 5.27 `virtualdesktopmanageradaptor`）。
pub const VD_PATH: &str = "/VirtualDesktopManager";
/// `org.kde.KWin.VirtualDesktopManager` 接口名。
pub const VD_IFACE: &str = "org.kde.KWin.VirtualDesktopManager";

/// 单窗详情 `getWindowInfo(uuid) -> a{sv}` 的解析结果 → core [`WindowInfo`]。
///
/// 字段取自 KWin `dbusinterface.cpp` `clientToVariantMap`（5.27 实测形态）：
/// `caption` / `resourceClass` / `resourceName` / `desktopFile` / `x` / `y` /
/// `width` / `height` / `uuid` / `desktops`（桌面 id 数组）/ `minimized` /
/// `maximizeHorizontal` / `maximizeVertical` / `fullscreen` / `keepAbove` /
/// `type`（`NET::WindowType` 整数枚举）。该 map **不含 pid**（KWin 未导出），
/// `pid` 恒 0；缺失字段按默认值兜底（同 Scripting `parse_window` 口径）。
///
/// `uuid` 缺失返回 `None`——调用方按 uuid 枚举，回读结果必须可回溯到该 uuid，
/// 否则是竞态（窗口已销毁）而非可渲染的窗口条目。
pub fn parse_window(map: &HashMap<String, OwnedValue>, stacking_order: u32) -> Option<WindowInfo> {
    let id_str = map.get("uuid").and_then(as_string)?;

    let rect = |kx: &str, ky: &str, kw: &str, kh: &str| Rect {
        x: map.get(kx).and_then(as_i32).unwrap_or(0),
        y: map.get(ky).and_then(as_i32).unwrap_or(0),
        width: map.get(kw).and_then(as_i32).unwrap_or(0),
        height: map.get(kh).and_then(as_i32).unwrap_or(0),
    };

    let mut states = Vec::new();
    if map.get("minimized").and_then(as_bool).unwrap_or(false) {
        states.push(WindowState::Minimized);
    }
    if map
        .get("maximizeHorizontal")
        .and_then(as_bool)
        .unwrap_or(false)
        || map
            .get("maximizeVertical")
            .and_then(as_bool)
            .unwrap_or(false)
    {
        states.push(WindowState::Maximized);
    }
    if map.get("fullscreen").and_then(as_bool).unwrap_or(false) {
        states.push(WindowState::FullScreen);
    }
    if states.is_empty() {
        states.push(WindowState::Normal);
    }

    let workspace_id = map
        .get("desktops")
        .and_then(as_string_vec)
        .and_then(|v| v.into_iter().next())
        .map(|id| agent_shell_core::types::WorkspaceId {
            native_id: id,
            de_type: DesktopEnvironment::KDE,
        });

    Some(WindowInfo {
        id: WindowId {
            native_id: id_str,
            de_type: DesktopEnvironment::KDE,
        },
        title: map.get("caption").and_then(as_string).unwrap_or_default(),
        app_id: map
            .get("resourceClass")
            .and_then(as_string)
            .unwrap_or_default(),
        // clientToVariantMap 不导出 pid（见模块 doc）。
        pid: 0,
        geometry: rect("x", "y", "width", "height"),
        // KWin 原生通道不提供 frameGeometry；置 default（全零）表示未知，
        // 与 X11 EWMH `x11_build_window_info` 的口径一致——不得把内容几何
        // 误报为外框。
        frame_geometry: Rect::default(),
        states,
        workspace_id,
        monitor_id: None,
        stacking_order,
        desktop_file: map
            .get("desktopFile")
            .and_then(as_string)
            .filter(|s| !s.is_empty()),
        window_type: map
            .get("type")
            .and_then(as_i32)
            .map(parse_window_type_int)
            .unwrap_or(agent_shell_core::types::WindowType::Unknown),
        icon_geometry: None,
        keep_above: map.get("keepAbove").and_then(as_bool).unwrap_or(false),
    })
}

/// KWin `NET::WindowType` 整数枚举 → core [`WindowType`]。
///
/// `clientToVariantMap` 的 `type` 字段是 `NET::WindowType`（`netwm_def.h`）
/// 整数值，非字符串——枚举值归一化到 core 窗口类型（与 Scripting
/// `parse_window_type` 的字符串映射对齐）。未知/未定义值归 `Unknown`。
fn parse_window_type_int(t: i32) -> agent_shell_core::types::WindowType {
    use agent_shell_core::types::WindowType;
    match t {
        0 => WindowType::Normal,        // NET::Normal
        1 => WindowType::Desktop,       // NET::Desktop
        2 => WindowType::Dock,          // NET::Dock
        4 => WindowType::DropdownMenu,  // NET::Menu（tear-off menu）
        5 => WindowType::Dialog,        // NET::Dialog
        8 => WindowType::Utility,       // NET::Utility
        9 => WindowType::Splash,        // NET::Splash
        10 => WindowType::DropdownMenu, // NET::DropdownMenu
        12 => WindowType::Tooltip,      // NET::Tooltip
        13 => WindowType::Notification, // NET::Notification
        _ => WindowType::Unknown,
    }
}

/// `org.kde.KWin` 原生通道（非 Scripting）：两个裸 proxy 分别挂
/// `/KWin`（窗口方法）与 `/VirtualDesktopManager`（工作区属性）。
///
/// 用裸 [`Proxy`] 而非 `#[zbus::proxy]`——`getWindowInfo` 返回 `a{sv}`，裸
/// proxy 直接把回执 body 反序列化为 `HashMap<String, OwnedValue>`，比生成
/// trait 的类型别名更直观，也便于把「uuid 不存在」与「服务不可达」分开处理。
pub struct KWinNative {
    /// `/KWin` · `org.kde.KWin`（getWindowInfo 等窗口方法）。
    inner: Proxy<'static>,
    /// `/VirtualDesktopManager` · `org.kde.KWin.VirtualDesktopManager`（工作区）。
    vd: Proxy<'static>,
}

impl KWinNative {
    /// 建代理：`org.kde.KWin` 的 `/KWin` 与 `/VirtualDesktopManager` 两个对象。
    /// 各自持有一份 `Connection` clone 保活（`Connection` 内部 Arc，开销可忽略）。
    pub async fn new(conn: Connection) -> Result<Self> {
        let inner = Proxy::new_owned(
            conn.clone(),
            "org.kde.KWin",
            ObjectPath::try_from(KWIN_PATH)
                .map_err(|e| KWinError::Scripting(format!("bad kwin path: {e}")))?,
            KWIN_IFACE,
        )
        .await
        .map_err(|e| KWinError::Scripting(format!("org.kde.KWin proxy: {e}")))?;
        let vd = Proxy::new_owned(
            conn,
            "org.kde.KWin",
            ObjectPath::try_from(VD_PATH)
                .map_err(|e| KWinError::Scripting(format!("bad vd path: {e}")))?,
            VD_IFACE,
        )
        .await
        .map_err(|e| KWinError::Scripting(format!("VirtualDesktopManager proxy: {e}")))?;
        Ok(Self { inner, vd })
    }

    /// 单窗详情（`getWindowInfo(uuid)`）。uuid 不存在返回空 map（KWin 语义：
    /// `findAbstractClient` 未命中即 `{}`），调用方据此跳过。
    pub async fn window_info(&self, uuid: &str) -> Result<HashMap<String, OwnedValue>> {
        let reply = self
            .inner
            .call_method("getWindowInfo", &(uuid,))
            .await
            .map_err(|e| KWinError::Scripting(format!("getWindowInfo({uuid}): {e}")))?;
        reply
            .body()
            .deserialize::<HashMap<String, OwnedValue>>()
            .map_err(|e| KWinError::Scripting(format!("getWindowInfo reply: {e}")))
    }

    /// `org.kde.KWin.VirtualDesktopManager` 的 `desktops` 属性（`a(uss)`，
    /// 每项 `(position, id, name)`，按 position 升序）。
    ///
    /// 首位是无符号 `u32`：KWin XML 声明 `a(iss)`，但 `DBusDesktopDataStruct.position`
    /// 是 C++ `uint`，QtDBus 用 QVariant 序列化时不强绑定 XML 签名，实际 wire
    /// 为 `a(uss)`（目标机 busctl 实测 `VARIANT "a(uss)"`，UINT32 0/1/2）——
    /// zvariant 严格签名匹配，用 `i32` 读会返回 `incorrect type` 恒失败。
    /// `position` 由调用方转 1 基 `number`。
    pub async fn desktops(&self) -> Result<Vec<(u32, String, String)>> {
        self.vd
            .get_property::<Vec<(u32, String, String)>>("desktops")
            .await
            .map_err(|e| KWinError::Scripting(format!("VirtualDesktopManager.desktops: {e}")))
    }

    /// `org.kde.KWin.VirtualDesktopManager` 的 `current` 属性（当前桌面 id）。
    pub async fn current_desktop(&self) -> Result<String> {
        self.vd
            .get_property::<String>("current")
            .await
            .map_err(|e| KWinError::Scripting(format!("VirtualDesktopManager.current: {e}")))
    }

    /// 激活工作区：写 `VirtualDesktopManager.current` 属性为桌面 id。
    ///
    /// 与 `list_workspaces` 的 `native_id`（`desktops` 每项的 id）同源——切换
    /// 传入的 [`WorkspaceId`] 直接来自原生列表时按 id 写回；Scripting 通道的
    /// numeric id 与本通道的 UUID id 不互通，跨通道切换不在本方法职责内。
    pub async fn set_current_desktop(&self, id: &str) -> Result<()> {
        self.vd
            .set_property("current", id)
            .await
            .map_err(|e| KWinError::Scripting(format!("VirtualDesktopManager.set current: {e}")))
    }

    /// 原生工作区列表：`desktops`（`a(uss)` → `(position, id, name)`）+ `current`
    /// 组装 core [`WorkspaceInfo`]。`native_id` 取桌面 id（与 `getWindowInfo`
    /// 返回的 `desktops` 数组同源），`number` 取 position+1——`position` 是
    /// `x11DesktopNumber() - 1`（0 基，KWin `desktopCreated`/`desktops` 实测），
    /// 而 core `number` 与 X11/Scripting 口径一致为 1 基。
    ///
    /// `current` 读取失败直接传播——`.ok()` 吞错误会把全部工作区误报为
    /// inactive，误导调用方。
    pub async fn workspaces(&self) -> Result<Vec<agent_shell_core::types::WorkspaceInfo>> {
        use agent_shell_core::types::{DesktopEnvironment, WorkspaceId, WorkspaceInfo};
        let desktops = self.desktops().await?;
        let current = self.current_desktop().await?;
        Ok(desktops
            .into_iter()
            .map(|(position, id, name)| WorkspaceInfo {
                id: WorkspaceId {
                    native_id: id.clone(),
                    de_type: DesktopEnvironment::KDE,
                },
                name,
                number: position.saturating_add(1),
                is_active: current == id,
                monitor_ids: Vec::new(),
                window_ids: Vec::new(),
            })
            .collect())
    }
}

/// `OwnedValue` → `bool`（`minimized` / `fullscreen` 等布尔字段）。
fn as_bool(v: &OwnedValue) -> Option<bool> {
    v.downcast_ref::<bool>().ok()
}

/// `OwnedValue` → `i32`（几何/坐标字段；QVariantMap 的 int 均为 32 位）。
fn as_i32(v: &OwnedValue) -> Option<i32> {
    v.downcast_ref::<i32>().ok()
}

/// `OwnedValue` → `String`（caption / resourceClass / uuid 等）。
fn as_string(v: &OwnedValue) -> Option<String> {
    v.downcast_ref::<String>().ok()
}

/// `OwnedValue` → `Vec<String>`（`desktops` 字段，QStringList → `as`）。
///
/// zvariant 无 `TryFrom<&Value> for Vec<String>`——数组以 `Value::Array` 呈现，
/// 先匹配取 `Array`，再逐元素 `downcast_ref::<String>` 转出。
fn as_string_vec(v: &OwnedValue) -> Option<Vec<String>> {
    let inner: &zbus::zvariant::Value<'static> = v;
    match inner {
        zbus::zvariant::Value::Array(arr) => arr
            .inner()
            .iter()
            .map(|e| e.downcast_ref::<String>().ok())
            .collect(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::types::WindowState;
    use zbus::zvariant::{Array, Str, Value};

    fn s(v: &str) -> OwnedValue {
        OwnedValue::from(Str::from(v).to_owned())
    }

    fn string_array(items: &[&str]) -> OwnedValue {
        let elements: Vec<Value<'static>> = items
            .iter()
            .map(|v| Value::Str(Str::from(*v).to_owned()))
            .collect();
        let array = Array::from(elements);
        OwnedValue::try_from(array).expect("build string array OwnedValue")
    }

    /// `getWindowInfo` 的 `clientToVariantMap` 形态 → core `WindowInfo`。
    #[test]
    fn parse_window_maps_kwin_native_fields() {
        let mut map = HashMap::new();
        map.insert("uuid".to_string(), s("{f8b1-..}"));
        map.insert("caption".to_string(), s("Terminal"));
        map.insert("resourceClass".to_string(), s("konsole"));
        map.insert("x".to_string(), OwnedValue::from(10i32));
        map.insert("y".to_string(), OwnedValue::from(20i32));
        map.insert("width".to_string(), OwnedValue::from(800i32));
        map.insert("height".to_string(), OwnedValue::from(600i32));
        map.insert("minimized".to_string(), OwnedValue::from(false));
        map.insert("maximizeHorizontal".to_string(), OwnedValue::from(false));
        map.insert("maximizeVertical".to_string(), OwnedValue::from(false));
        map.insert("fullscreen".to_string(), OwnedValue::from(true));
        map.insert("keepAbove".to_string(), OwnedValue::from(true));
        map.insert("desktopFile".to_string(), s("org.kde.konsole.desktop"));
        map.insert("type".to_string(), OwnedValue::from(0i32));
        map.insert("desktops".to_string(), string_array(&["vd-uuid-1"]));

        let w = parse_window(&map, 3).expect("window must parse");
        assert_eq!(w.id.native_id, "{f8b1-..}");
        assert_eq!(w.title, "Terminal");
        assert_eq!(w.app_id, "konsole");
        // clientToVariantMap 无 pid 字段 → 恒 0。
        assert_eq!(w.pid, 0);
        assert_eq!(w.geometry.x, 10);
        assert_eq!(w.geometry.y, 20);
        assert_eq!(w.geometry.width, 800);
        assert_eq!(w.geometry.height, 600);
        assert_eq!(w.stacking_order, 3);
        assert!(w.states.contains(&WindowState::FullScreen));
        assert!(!w.states.contains(&WindowState::Minimized));
        assert!(w.keep_above);
        assert_eq!(
            w.workspace_id.as_ref().map(|w| w.native_id.as_str()),
            Some("vd-uuid-1")
        );
        assert_eq!(w.window_type, agent_shell_core::types::WindowType::Normal);
    }

    /// `NET::WindowType` 整数枚举 → core `WindowType`（type 字段是整数非字符串）。
    #[test]
    fn window_type_maps_integer_enum() {
        use agent_shell_core::types::WindowType;
        let mut map = HashMap::new();
        map.insert("uuid".to_string(), s("u-1"));
        map.insert("type".to_string(), OwnedValue::from(13i32)); // NET::Notification
        assert_eq!(
            parse_window(&map, 0).unwrap().window_type,
            WindowType::Notification
        );
        // 字符串 type（错误形态）→ 不命中 as_i32 → Unknown。
        map.insert("type".to_string(), s("normal"));
        assert_eq!(
            parse_window(&map, 0).unwrap().window_type,
            WindowType::Unknown
        );
    }

    /// uuid 缺失 → 竞态（窗口已销毁），跳过而非渲染空 id。
    #[test]
    fn parse_window_without_uuid_is_none() {
        let mut map = HashMap::new();
        map.insert("caption".to_string(), s("orphan"));
        assert!(parse_window(&map, 0).is_none());
    }

    /// 布尔几何字段缺失按默认值兜底；无状态 → Normal。
    #[test]
    fn parse_window_defaults_missing_fields() {
        let mut map = HashMap::new();
        map.insert("uuid".to_string(), s("u-1"));
        let w = parse_window(&map, 0).expect("minimal window");
        assert_eq!(w.geometry, Rect::default());
        assert_eq!(w.states, vec![WindowState::Normal]);
        assert_eq!(w.title, "");
        assert_eq!(w.pid, 0);
        assert_eq!(w.window_type, agent_shell_core::types::WindowType::Unknown);
    }

    /// `desktops` wire 签名契约：实际 wire 为 `a(uss)`（C++ uint position，
    /// busctl 实测 VARIANT "a(uss)"），XML 的 `a(iss)` 是误导声明——锁定
    /// 本实现依赖的真实类型选择（用 `i32` 读会 `incorrect type` 恒失败）。
    #[test]
    fn desktops_wire_signature_is_a_uss() {
        use zbus::zvariant::Type;
        assert_eq!(
            Vec::<(u32, String, String)>::SIGNATURE.to_string(),
            "a(uss)",
            "KWin VirtualDesktopManager.desktops actual wire is a(uss)"
        );
        assert_eq!(
            Vec::<(i32, String, String)>::SIGNATURE.to_string(),
            "a(iss)",
            "sanity: i32 position would read a(iss), which mismatches actual wire"
        );
    }

    /// 端到端反序列化：`desktops` 的 `a(uss)` wire 数据只能反序列化为
    /// `Vec<(u32,..)>`，`Vec<(i32,..)>` 因签名不匹配（`a(iss)`）失败——
    /// 上一版用 `i32` 读 `a(uss)` 正是 TC-004 目标机恒失败的根因。
    #[test]
    fn a_uss_wire_data_rejects_i32_position() {
        use zbus::zvariant::serialized::Context;
        use zbus::zvariant::{to_bytes, LE};

        let data: Vec<(u32, String, String)> =
            vec![(1, "vd-1".to_string(), "Desktop 1".to_string())];
        let ctx = Context::new_dbus(LE, 0);
        let bytes = to_bytes(ctx, &data).expect("serialize a(uss)");

        let (back, _): (Vec<(u32, String, String)>, usize) =
            bytes.deserialize().expect("a(uss) -> Vec<(u32,..)>");
        assert_eq!(back, data);

        let err = bytes.deserialize::<(Vec<(i32, String, String)>, usize)>();
        assert!(
            err.is_err(),
            "a(uss) must NOT deserialize into Vec<(i32,..)> (signature mismatch)"
        );
    }
}
