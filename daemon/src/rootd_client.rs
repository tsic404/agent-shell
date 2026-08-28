//! rootd D-Bus 客户端（§23.4 特权链路：CLI → daemon → rootd）。
//!
//! daemon 经 system bus 调用 rootd 的白名单方法。rootd 未安装时
//! 返回 None——上层据此降级为仅用户态操作（§23.2）。

use zbus::proxy;

/// rootd system bus 代理（org.agentshell.Rootd）。
///
/// 方法经 polkit 授权（§23.4.2）；daemon 不直连系统资源——
/// 全部经 rootd 白名单接口中转。
///
/// **注意**：当前仅接线 CLI 需要的 5 个方法（hello/service_start/
/// service_stop/service_restart/journal_query）。rootd D-Bus 接口暴露
/// 20 个白名单方法，其余（service_enable/disable/reload/daemon_reload/
/// sysctl_get/set/hostname_set/process_kill/mount/unmount/set_token/
/// package_*）待 CLI 后续命令扩展时添加。
#[proxy(
    interface = "org.agentshell.Rootd",
    default_service = "org.agentshell.Rootd",
    default_path = "/org/agentshell/Rootd"
)]
pub trait Rootd {
    /// 版本对账（§23.4.3）。
    fn hello(&self) -> zbus::Result<String>;

    /// 启动系统服务。
    fn service_start(&self, unit: &str) -> zbus::Result<()>;

    /// 停止系统服务。
    fn service_stop(&self, unit: &str) -> zbus::Result<()>;

    /// 重启系统服务。
    fn service_restart(&self, unit: &str) -> zbus::Result<()>;

    /// 查询系统日志。
    fn journal_query(&self, filter: &str) -> zbus::Result<String>;
}

/// rootd 连接结果。None = rootd 未安装（降级路径，§23.2）。
///
/// 通过 NameHasOwner 探测 org.agentshell.Rootd 是否在线——
/// zbus Proxy::builder 不校验 name ownership，即使 rootd 缺席
/// 也能 build 成功，导致降级分支不可达。
pub async fn connect() -> Option<RootdProxy<'static>> {
    let conn = zbus::Connection::system().await.ok()?;
    // 探测 rootd 是否在 system bus 上注册
    let dbus_proxy = zbus::proxy::Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .ok()?;
    let has_owner: bool = dbus_proxy
        .call::<_, _, bool>("NameHasOwner", &("org.agentshell.Rootd",))
        .await
        .ok()?;
    if !has_owner {
        return None;
    }
    RootdProxy::builder(&conn)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()
}
