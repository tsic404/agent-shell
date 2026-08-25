//! DePriorityRouter——DE 封装优先路由（设计文档 §21.4）。
//!
//! 统一规则：每个能力先探测 DE 专有 D-Bus 服务
//! （org.kde.* / org.deepin.dde.* / org.gnome.*），命中用 DE 封装实现，
//! 未命中回退公共组件实例（portal / freedesktop / 公共通用实例）。
//!
//! 本 crate 提供探测原语与电源域的路由；其余能力域（通知/外观/启动器）
//! 的 DE 封装在 `backends/{kde,dde,gnome}` 内按同一模式装配。

use agent_shell_core::error::{AgentShellError, Result};

/// DDE 20/25 双服务名（§21.36.1）：DDE25 主名 org.deepin.dde.*，
/// DDE20 仅 com.deepin.daemon.*；先探新名，失败退旧名。
pub const DDE_POWER_NAMES: [&str; 2] = ["org.deepin.dde.Power1", "com.deepin.daemon.Power"];

/// 探测 bus 上是否存在指定服务名（NameHasOwner）。
pub async fn service_exists(conn: &zbus::Connection, name: &str) -> Result<bool> {
    use zbus::fdo::DBusProxy;
    let dbus = DBusProxy::new(conn)
        .await
        .map_err(|e| AgentShellError::DBus(format!("DBusProxy: {e}")))?;
    let bus_name: zbus::names::BusName<'_> = name
        .try_into()
        .map_err(|e| AgentShellError::DBus(format!("bad bus name {name}: {e}")))?;
    dbus.name_has_owner(bus_name)
        .await
        .map_err(|e| AgentShellError::DBus(format!("NameHasOwner({name}): {e}")))
}

/// 双名探测：依次尝试候选服务名，返回第一个在 bus 上的名字。
pub async fn probe_first_existing(
    conn: &zbus::Connection,
    names: &[&str],
) -> Result<Option<String>> {
    for name in names {
        if service_exists(conn, name).await? {
            return Ok(Some((*name).to_string()));
        }
    }
    Ok(None)
}
