//! rootd D-Bus 服务接口（§23.4.3 `org.agentshell.Rootd`）。
//!
//! zbus `#[interface]` trait 将 lib 层 `dispatch` 包装为 D-Bus 方法：
//! - 每个方法调用前经 polkit 校验（`check_polkit`）——使用入站消息头
//!   中的调用者 unique name（非 rootd 自己的总线名）
//! - 非白名单方法由 lib `dispatch` 拒绝（默认拒绝）
//! - `JobProgress`/`JobDone` 信号经 `job_snapshot`/`job_drain_done` 驱动
use crate::{dispatch, job_drain_done, job_snapshot, polkit_action_for, JobState};
use serde_json::Value;
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::Interface;
use zbus::object_server::SignalEmitter;
use zbus_macros::interface;

/// polkit 校验：检查调用者是否被授权执行给定 action。
///
/// `caller` 是入站消息头中的 sender（调用者 unique name），**非**
/// `connection.unique_name()`（那是 rootd 自己的总线名）。
/// 返回 `Ok(())` 表示授权通过，`Err` 表示拒绝或 polkit 不可用。
/// 无 action 的方法（Hello/SetToken）不需 polkit。
async fn check_polkit(
    connection: &zbus::Connection,
    caller: &str,
    action_id: &str,
) -> fdo::Result<()> {
    if action_id.is_empty() {
        return Ok(());
    }
    let proxy = zbus::proxy::Proxy::new(
        connection,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await
    .map_err(|e| fdo::Error::AuthFailed(format!("polkit proxy: {e}")))?;

    // 入站 sender unique name 不能直接拼 `system-bus-connection` subject——
    // polkitd 127 不支持该 kind（运行时 `Unknown subject of kind`），仅
    // `unix-process`/`unix-session` 可用。此处经 org.freedesktop.DBus
    // GetConnectionCredentials 把 sender 解析为真实进程凭证，再构造
    // `unix-process` subject。
    let subject = resolve_unix_process_subject(connection, caller).await?;

    // CheckAuthorization(subject, action_id, details, flags, cancellation_id)
    // 返回 (is_authorized: bool, is_challenge: bool, details: dict)
    let reply: (bool, bool, std::collections::HashMap<String, String>) = proxy
        .call(
            "CheckAuthorization",
            &(
                subject,
                action_id,
                std::collections::HashMap::<String, String>::new(),
                1u32, // AllowUserInteraction
                "",
            ),
        )
        .await
        .map_err(|e| fdo::Error::AuthFailed(format!("polkit check: {e}")))?;

    if reply.0 {
        Ok(())
    } else {
        Err(fdo::Error::AuthFailed(format!(
            "polkit denied action: {action_id}"
        )))
    }
}

/// 将入站 D-Bus sender unique name 解析为 polkit `unix-process` subject。
///
/// polkitd 127 支持 `unix-process`（详情键 `pid` + `start-time` + `uid`），
/// 不支持 `system-bus-connection`/`unix-user`/`unix-group`。经
/// `org.freedesktop.DBus.GetConnectionCredentials` 取得 sender 的
/// `ProcessID`/`UnixUserID`，再读 `/proc/<pid>/stat` 第 22 字段
/// （starttime，时钟节拍）——与 polkit 自身 `get_start_time_for_pid`
/// 的实现一致，避免 PID 复用导致的授权劫持。
async fn resolve_unix_process_subject(
    connection: &zbus::Connection,
    caller: &str,
) -> fdo::Result<(
    &'static str,
    std::collections::HashMap<String, zbus::zvariant::Value<'static>>,
)> {
    let bus_name: zbus::names::BusName<'_> = caller
        .try_into()
        .map_err(|_| fdo::Error::AuthFailed(format!("invalid caller name: {caller}")))?;
    let dbus = zbus::fdo::DBusProxy::new(connection)
        .await
        .map_err(|e| fdo::Error::AuthFailed(format!("dbus proxy: {e}")))?;
    let creds = dbus
        .get_connection_credentials(bus_name)
        .await
        .map_err(|e| fdo::Error::AuthFailed(format!("caller credentials: {e}")))?;

    let pid = creds
        .process_id()
        .ok_or_else(|| fdo::Error::AuthFailed("caller has no ProcessID credential".into()))?;
    let uid = creds
        .unix_user_id()
        .ok_or_else(|| fdo::Error::AuthFailed("caller has no UnixUserID credential".into()))?;
    let start_time = process_start_time(pid).ok_or_else(|| {
        fdo::Error::AuthFailed(format!("cannot determine start time for pid {pid}"))
    })?;

    let mut details = std::collections::HashMap::new();
    details.insert("pid".to_string(), zbus::zvariant::Value::from(pid));
    details.insert(
        "start-time".to_string(),
        zbus::zvariant::Value::from(start_time),
    );
    details.insert("uid".to_string(), zbus::zvariant::Value::from(uid as i32));
    Ok(("unix-process", details))
}

/// 读 `/proc/<pid>/stat` 第 22 字段（starttime，时钟节拍），返回 u64。
///
/// 与 polkit 的 `get_start_time_for_pid`（polkitunixprocess.c）一致：
/// 从右向左搜索最后的 `)` 再跳过 `" "` 后按空格切分，取索引 19 的 token
/// （即字段 22）。进程名可含 `)`，故不能从左侧切分。
fn process_start_time(pid: u32) -> Option<u64> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = contents.rfind(')')?;
    let rest = contents.get(close.checked_add(2)?..)?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    // 跳过 `(comm)` 后剩余字段：state(3) 起，starttime 为字段 22 → 索引 19。
    tokens.get(19)?.parse::<u64>().ok()
}

/// D-Bus 接口 `org.agentshell.Rootd`（§23.4.3 白名单）。
pub struct RootdInterface;

impl RootdInterface {
    pub fn new() -> Self {
        Self
    }

    /// 通用方法分派：polkit 校验（使用调用者 bus name）→ lib dispatch。
    ///
    /// `caller` 来自入站消息头的 `sender()`——调用者的 unique name，
    /// 非 rootd 自己的总线名。polkit 据此校验调用者凭证。
    ///
    /// 返回的 `Value` 包含 lib 层全部字段（如 `cmd`/`pm` 等）；
    /// 各 D-Bus 方法只取需要的字段（如 `job_id`/`value`），
    /// 丢弃的诊断字段对 D-Bus 调用者不可见——无功能影响。
    async fn call_method(
        &self,
        connection: &zbus::Connection,
        caller: &str,
        method: &str,
        args: Vec<Value>,
    ) -> fdo::Result<Value> {
        if let Some(action_id) = polkit_action_for(method) {
            check_polkit(connection, caller, action_id).await?;
        }
        dispatch(method, &args).map_err(fdo::Error::Failed)
    }
}

impl Default for RootdInterface {
    fn default() -> Self {
        Self::new()
    }
}

#[interface(name = "org.agentshell.Rootd")]
impl RootdInterface {
    /// 版本对账（§23.4.3）。
    ///
    /// Hello 无 polkit（设计意图：版本对账无副作用）。但返回值仅含
    /// 安全模型版本号——不暴露系统配置或敏感信息。system bus 上任意
    /// 客户端可探测 rootd 安装状态，这是 intentional：daemon 需此
    /// 信息决定降级路径（§23.2）。
    async fn hello(&self) -> fdo::Result<String> {
        let r = dispatch("Hello", &[]).map_err(fdo::Error::Failed)?;
        Ok(r.to_string())
    }

    // ── 软件包 ──

    async fn package_install(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        packages: Vec<String>,
    ) -> fdo::Result<String> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        let r = self
            .call_method(conn, caller, "PackageInstall", vec![Value::from(packages)])
            .await?;
        r.get("job_id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| fdo::Error::Failed("missing job_id".into()))
    }

    async fn package_remove(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        packages: Vec<String>,
    ) -> fdo::Result<String> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        let r = self
            .call_method(conn, caller, "PackageRemove", vec![Value::from(packages)])
            .await?;
        r.get("job_id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| fdo::Error::Failed("missing job_id".into()))
    }

    async fn package_update(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        packages: Vec<String>,
    ) -> fdo::Result<String> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        let r = self
            .call_method(conn, caller, "PackageUpdate", vec![Value::from(packages)])
            .await?;
        r.get("job_id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| fdo::Error::Failed("missing job_id".into()))
    }

    async fn package_refresh(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<String> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        let r = self
            .call_method(conn, caller, "PackageRefresh", vec![])
            .await?;
        r.get("job_id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| fdo::Error::Failed("missing job_id".into()))
    }

    // ── systemd system 单元 ──

    async fn service_start(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        unit: String,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "ServiceStart", vec![Value::from(unit)])
            .await
            .map(|_| ())
    }

    async fn service_stop(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        unit: String,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "ServiceStop", vec![Value::from(unit)])
            .await
            .map(|_| ())
    }

    async fn service_restart(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        unit: String,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "ServiceRestart", vec![Value::from(unit)])
            .await
            .map(|_| ())
    }

    async fn service_enable(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        unit: String,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "ServiceEnable", vec![Value::from(unit)])
            .await
            .map(|_| ())
    }

    async fn service_disable(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        unit: String,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "ServiceDisable", vec![Value::from(unit)])
            .await
            .map(|_| ())
    }

    async fn service_reload(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        unit: String,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "ServiceReload", vec![Value::from(unit)])
            .await
            .map(|_| ())
    }

    async fn daemon_reload(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "DaemonReload", vec![])
            .await
            .map(|_| ())
    }

    // ── 系统日志 ──

    async fn journal_query(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        filter: String,
    ) -> fdo::Result<String> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        let r = self
            .call_method(conn, caller, "JournalQuery", vec![Value::from(filter)])
            .await?;
        // §23.4：JournalQuery 出参是真实 journalctl JSON 行流（设计文档
        // 签名 `s → s`，out 注释「JSON 行流」）——lib 层产出 `output`，
        // `output_format` 只是元数据，不是调用者要的数据。
        r.get("output")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| fdo::Error::Failed("missing output".into()))
    }

    // ── 系统配置 ──

    async fn sysctl_get(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        key: String,
    ) -> fdo::Result<String> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        let r = self
            .call_method(conn, caller, "SysctlGet", vec![Value::from(key)])
            .await?;
        r.get("value")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| fdo::Error::Failed("missing value".into()))
    }

    async fn sysctl_set(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        key: String,
        value: zbus::zvariant::Value<'_>,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        // 显式转换 zvariant::Value → serde_json::Value，保留类型语义。
        // 不用 unwrap_or(Null)——转换失败应报错而非传 Null 给 sysctl_set。
        let v = zvariant_to_json(&value);
        self.call_method(conn, caller, "SysctlSet", vec![Value::from(key), v])
            .await
            .map(|_| ())
    }

    async fn hostname_set(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        hostname: String,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "HostnameSet", vec![Value::from(hostname)])
            .await
            .map(|_| ())
    }

    // ── 进程管理 ──

    async fn process_kill(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        pid: i32,
        signal: i32,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(
            conn,
            caller,
            "ProcessKill",
            vec![Value::from(pid as i64), Value::from(signal as i64)],
        )
        .await
        .map(|_| ())
    }

    // ── 挂载 ──

    async fn mount(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        device: String,
        target: String,
        fstype: String,
        options: Vec<String>,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(
            conn,
            caller,
            "Mount",
            vec![
                Value::from(device),
                Value::from(target),
                Value::from(fstype),
                Value::from(options),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn unmount(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        target: String,
    ) -> fdo::Result<()> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "Unmount", vec![Value::from(target)])
            .await
            .map(|_| ())
    }

    // ── 会话 Token ──

    async fn set_token(&self, token: String) -> fdo::Result<()> {
        dispatch("SetToken", &[Value::from(token)]).map_err(fdo::Error::Failed)?;
        Ok(())
    }

    // ── 信号（§23.4.3）──

    /// Job 进度信号。
    #[zbus(signal)]
    async fn job_progress(
        &self,
        _ctxt: SignalEmitter<'_>,
        job_id: String,
        progress: f64,
    ) -> zbus::Result<()>;

    /// Job 完成信号。
    #[zbus(signal)]
    async fn job_done(
        &self,
        _ctxt: SignalEmitter<'_>,
        job_id: String,
        success: bool,
    ) -> zbus::Result<()>;
}

/// 显式转换 `zvariant::Value` → `serde_json::Value`，保留类型语义。
/// 不用 `serde_json::to_value` + `unwrap_or(Null)`——转换失败应报错。
fn zvariant_to_json(value: &zbus::zvariant::Value<'_>) -> Value {
    match value {
        zbus::zvariant::Value::Str(s) => Value::from(s.as_str()),
        zbus::zvariant::Value::Bool(b) => Value::from(*b),
        zbus::zvariant::Value::U8(n) => Value::from(*n),
        zbus::zvariant::Value::I16(n) => Value::from(*n),
        zbus::zvariant::Value::U16(n) => Value::from(*n),
        zbus::zvariant::Value::I32(n) => Value::from(*n),
        zbus::zvariant::Value::U32(n) => Value::from(*n),
        zbus::zvariant::Value::I64(n) => Value::from(*n),
        zbus::zvariant::Value::U64(n) => Value::from(*n),
        zbus::zvariant::Value::F64(n) => Value::from(*n),
        _ => Value::String(value.to_string()),
    }
}

/// 信号驱动循环：周期性轮询 job 状态，发射 D-Bus 信号。
///
/// 注意：`emit()` 的第三个参数是 `&impl Serialize`——Rust 元组
/// `(String, f64)` 序列化为 D-Bus 结构体 `(sd)`，与 §23.4.3 信号的
/// 两个独立 `<arg>` 签名一致（zbus 将元组序列化为多参数 body）。
pub async fn drive_signals(
    _connection: &zbus::Connection,
    iface_ref: zbus::object_server::InterfaceRef<RootdInterface>,
) {
    use std::time::Duration;
    let mut last_snapshot: Vec<JobState> = Vec::new();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;

        let emitter = iface_ref.signal_emitter();

        // 进度信号——只在已有前次快照且进度变化时发射
        // （新 job 的 0.0 初始进度不发信号——job_id 由方法返回值传递）
        let snapshot = job_snapshot();
        for job in &snapshot {
            let prev = last_snapshot.iter().find(|j| j.id == job.id);
            let changed = prev.is_some_and(|p| (p.progress - job.progress).abs() > f64::EPSILON);
            if changed && !job.done {
                let _ = emitter
                    .emit(
                        RootdInterface::name(),
                        "JobProgress",
                        &(job.id.clone(), job.progress),
                    )
                    .await;
            }
        }
        last_snapshot = snapshot;

        // 完成信号
        let done = job_drain_done();
        for job in done {
            let _ = emitter
                .emit(RootdInterface::name(), "JobDone", &(job.id, job.success))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::process_start_time;

    #[test]
    fn start_time_of_current_process_is_positive() {
        let pid = std::process::id();
        let t = process_start_time(pid);
        assert!(t.is_some_and(|v| v > 0));
    }

    #[test]
    fn start_time_missing_pid_is_none() {
        // 远大于任何实际 PID 的值——不存在对应 /proc/<pid>/stat
        assert!(process_start_time(u32::MAX).is_none());
    }
}
