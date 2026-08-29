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
/// 完整接线 §23.4.3 的全部 21 个白名单方法 + JobProgress/JobDone 信号。
#[proxy(
    interface = "org.agentshell.Rootd",
    default_service = "org.agentshell.Rootd",
    default_path = "/org/agentshell/Rootd"
)]
pub trait Rootd {
    /// 版本对账（§23.4.3）。
    fn hello(&self) -> zbus::Result<String>;

    // ── 软件包（返回 job id，进度经 JobProgress/JobDone 信号）──
    fn package_install(&self, packages: Vec<String>) -> zbus::Result<String>;
    fn package_remove(&self, packages: Vec<String>) -> zbus::Result<String>;
    fn package_update(&self, packages: Vec<String>) -> zbus::Result<String>;
    fn package_refresh(&self) -> zbus::Result<String>;

    // ── systemd system 单元 ──
    fn service_start(&self, unit: &str) -> zbus::Result<()>;
    fn service_stop(&self, unit: &str) -> zbus::Result<()>;
    fn service_restart(&self, unit: &str) -> zbus::Result<()>;
    fn service_reload(&self, unit: &str) -> zbus::Result<()>;
    fn service_enable(&self, unit: &str) -> zbus::Result<()>;
    fn service_disable(&self, unit: &str) -> zbus::Result<()>;
    fn daemon_reload(&self) -> zbus::Result<()>;

    // ── 系统日志 ──
    fn journal_query(&self, filter: &str) -> zbus::Result<String>;

    // ── 系统配置 ──
    fn sysctl_get(&self, key: &str) -> zbus::Result<String>;
    fn sysctl_set(&self, key: &str, value: zbus::zvariant::Value<'_>) -> zbus::Result<()>;
    fn hostname_set(&self, hostname: &str) -> zbus::Result<()>;

    // ── 进程管理（跨用户）──
    fn process_kill(&self, pid: i32, signal: i32) -> zbus::Result<()>;

    // ── 挂载 ──
    fn mount(
        &self,
        device: &str,
        target: &str,
        fstype: &str,
        options: Vec<String>,
    ) -> zbus::Result<()>;
    fn unmount(&self, target: &str) -> zbus::Result<()>;

    // ── 会话 Token ──
    fn set_token(&self, token: &str) -> zbus::Result<()>;

    // ── Job 状态查询 ──
    fn job_status(&self, job_id: &str) -> zbus::Result<String>;

    // ── 信号（§23.4.3）──
    #[zbus(signal)]
    fn job_progress(&self, job_id: String, progress: f64) -> zbus::Result<()>;
    #[zbus(signal)]
    fn job_done(&self, job_id: String, success: bool) -> zbus::Result<()>;
}

/// rootd 连接结果。None = rootd 未安装（降级路径，§23.2）。
///
/// 通过 NameHasOwner 探测 org.agentshell.Rootd 是否在线——
/// zbus Proxy::builder 不校验 name ownership，即使 rootd 缺席
/// 也能 build 成功，导致降级分支不可达。
pub async fn connect() -> Option<RootdProxy<'static>> {
    // 方法超时必须大于 rootd 内部 JournalQuery 超时（60s），让 rootd 的
    // 超时错误（而非 daemon 侧 D-Bus 超时）传播回 CLI——daemon 侧先行
    // 超时会掩盖真实失败原因（TSI-2493）。
    let conn = zbus::connection::Builder::system()
        .ok()?
        .method_timeout(std::time::Duration::from_secs(90))
        .build()
        .await
        .ok()?;
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
