//! rootd D-Bus 服务接口（§23.4.3 `org.agentshell.Rootd`）。
//!
//! zbus `#[interface]` trait 将 lib 层 `dispatch` 包装为 D-Bus 方法：
//! - 每个方法调用前经 polkit 校验（`check_polkit`）——使用入站消息头
//!   中的调用者 unique name（非 rootd 自己的总线名）
//! - 非白名单方法由 lib `dispatch` 拒绝（默认拒绝）
//! - `JobProgress`/`JobDone` 信号经 `job_snapshot`/`job_drain_done` 驱动
use crate::{
    dispatch, dispatch_with_pid, job_drain_done, job_snapshot, kill_via_pidfd, polkit_action_for,
    JobState, COMMAND_TIMEOUT, JOURNAL_QUERY_TIMEOUT, MAX_CONCURRENT_BLOCKING_COMMANDS,
};
use serde_json::Value;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
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

/// 阻塞命令并发准入闸（TSI-2504）：`call_method` 分派前必须取得许可。
/// 上限 [`MAX_CONCURRENT_BLOCKING_COMMANDS`] 远低于 tokio blocking 线程池
/// 上限，防止调用潮耗尽线程与 spawn 的短生命周期子进程。
///
/// 范围仅覆盖经 `spawn_blocking` 执行的同步阻塞命令；Package* 后台 job
/// 在 `spawn_package_job` 的独立线程中异步执行，不经此闸。
static BLOCKING_SEMAPHORE: Semaphore = Semaphore::const_new(MAX_CONCURRENT_BLOCKING_COMMANDS);

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
        // polkit 通过后，分派与超时/事件循环隔离全部委托给
        // `dispatch_with_timeout`——同步 dispatch 可能执行阻塞性系统命令
        // （journalctl/systemctl/sysctl/hostnamectl/mount/umount），必须从
        // tokio worker 移出，否则单次长查询会占住 rootd 事件循环，后续调用
        // 全部排队超时（TSI-2493：全量 journalctl 200s，本应 0.4s 的
        // --lines=50 排队）。TSI-2504：分派前经 Semaphore 限制并发阻塞命令数。
        dispatch_with_semaphore(method, args, &BLOCKING_SEMAPHORE)
            .await
            .map_err(fdo::Error::Failed)
    }
}

/// 带超时与事件循环隔离地执行一次 dispatch（§23.4.3 白名单方法）。
///
/// 同步 `dispatch` 可能执行阻塞性系统命令；`spawn_blocking` 将阻塞工作移出
/// tokio worker，`tokio::time::timeout` 在超时后放弃等待（TSI-2504：随后
/// kill 已 spawn 的子进程，杜绝超时后的孤儿进程）。
/// 独立成 async 纯函数，使「超时 + spawn_blocking 包装」可被 `#[tokio::test]`
/// 直接断言：把包装改回同步 `dispatch` 会使超时测试失败。
async fn dispatch_with_timeout(method: &str, args: Vec<Value>) -> Result<Value, String> {
    dispatch_with_timeout_tracked(method, args).await.0
}

/// `dispatch_with_timeout` 的实现，额外返回超时分支中被 kill 子进程的
/// pidfd（`Option<OwnedFd>`）供测试断言 kill 确实发生；生产路径忽略该值。
async fn dispatch_with_timeout_tracked(
    method: &str,
    args: Vec<Value>,
) -> (Result<Value, String>, Option<OwnedFd>) {
    let timeout = command_timeout_for(method);
    let method_name = method.to_string();
    let pidfd_slot: Arc<Mutex<Option<OwnedFd>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&pidfd_slot);
    let result = tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || dispatch_with_pid(&method_name, &args, &slot)),
    )
    .await;
    match result {
        // spawn_blocking 成功完成，内层为 dispatch 结果。
        Ok(Ok(dispatch_result)) => (dispatch_result, None),
        // spawn_blocking join 失败（blocking 线程 panic/中止）。
        Ok(Err(join_err)) => (Err(format!("{method} task join failed: {join_err}")), None),
        // 超时：短暂等待阻塞线程把子进程 pidfd 写入 slot 后 kill（TSI-2504）。
        Err(_) => {
            let killed = wait_for_pidfd(&pidfd_slot, Duration::from_millis(200))
                .await
                .inspect(kill_via_pidfd);
            (Err(format!("{method} timed out after {timeout:?}")), killed)
        }
    }
}

/// 并发准入 + 超时隔离的分派（TSI-2504）。
///
/// 先取得 Semaphore 许可再进入 `dispatch_with_timeout`：许可只限并发
/// 准入，不消耗命令自身的超时预算——排队等待不因本函数超时而失败。
async fn dispatch_with_semaphore(
    method: &str,
    args: Vec<Value>,
    sem: &Semaphore,
) -> Result<Value, String> {
    let _permit = sem
        .acquire()
        .await
        .map_err(|_| "blocking semaphore closed".to_string())?;
    dispatch_with_timeout(method, args).await
}

/// 轮询等待 pidfd 写入（阻塞线程已 spawn 子进程），最多 `grace` 时长。
///
/// Semaphore 许可先于 `spawn_blocking` 取得，16 上限远小于 blocking 线程池
/// 512 容量，spawn 应在毫秒内发生；200ms 宽限覆盖调度抖动，避免 kill 落空。
async fn wait_for_pidfd(slot: &Arc<Mutex<Option<OwnedFd>>>, grace: Duration) -> Option<OwnedFd> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if let Some(fd) = slot.lock().expect("pidfd slot poisoned").take() {
            return Some(fd);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 方法对应的命令超时：`JournalQuery` 走专项超时（60s），其余走默认（30s）。
///
/// 纯函数便于单测分支；阻塞性命令实际执行仍经 `dispatch_with_timeout` 的
/// `spawn_blocking` + `timeout` 包装。`TestSlowMethod`/`TestSpawnSleep` 为
/// 测试专用，映射亚秒超时保证回归测试在 CI 快速完成，而非真等 30s。
fn command_timeout_for(method: &str) -> std::time::Duration {
    if method == "JournalQuery" {
        JOURNAL_QUERY_TIMEOUT
    } else if method == "TestSlowMethod" || method == "TestSpawnSleep" {
        std::time::Duration::from_millis(100)
    } else {
        COMMAND_TIMEOUT
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

    // ── Job 状态查询 ──

    async fn job_status(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
        job_id: String,
    ) -> fdo::Result<String> {
        let caller = hdr.sender().map(|n| n.as_str()).unwrap_or("");
        self.call_method(conn, caller, "JobStatus", vec![Value::from(job_id)])
            .await
            .map(|r| serde_json::to_string(&r).unwrap_or_else(|_| "{}".to_string()))
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
    use super::{
        command_timeout_for, dispatch_with_semaphore, dispatch_with_timeout,
        dispatch_with_timeout_tracked, process_start_time,
    };
    use crate::{COMMAND_TIMEOUT, JOURNAL_QUERY_TIMEOUT};
    use std::os::fd::AsRawFd;

    // ── drive_signals 信号循环 ──

    use super::{drive_signals, RootdInterface};
    use crate::{job_create, job_done_with, job_progress, job_status, JOB_TEST_MUTEX};
    use futures_util::StreamExt;
    use std::io::Read as _;
    use std::process::{ChildStdout, Stdio};
    use std::time::{Duration, Instant};

    /// 独立私有 session bus（避免 `Connection::session()` 环境变量在并行
    /// 测试间竞争）。对象服务与信号订阅方共享同一地址。
    struct TestBus {
        addr: String,
        _child: std::process::Child,
    }

    impl TestBus {
        async fn start() -> Self {
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("dbus-daemon must be installed for rootd dbus tests");
            let stdout = child.stdout.take().expect("piped stdout");
            let addr = read_address_line(stdout);
            assert!(
                addr.starts_with("unix:"),
                "dbus-daemon printed unexpected address: {addr:?}"
            );
            Self {
                addr,
                _child: child,
            }
        }

        async fn connect(&self) -> zbus::Connection {
            zbus::connection::Builder::address(self.addr.as_str())
                .expect("dbus-daemon address must parse")
                .build()
                .await
                .expect("connect to private session bus")
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self._child.kill();
            let _ = self._child.wait();
        }
    }

    /// 逐字节读地址行：`dbus-daemon --print-address=1` 恰好一行。
    fn read_address_line(stdout: ChildStdout) -> String {
        let mut reader = std::io::BufReader::new(stdout);
        let mut bytes = Vec::new();
        loop {
            let mut buf = [0u8; 1];
            match reader.read_exact(&mut buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("read dbus-daemon address: {e}"),
            }
            bytes.push(buf[0]);
            if buf[0] == b'\n' {
                break;
            }
        }
        let line = String::from_utf8(bytes).expect("dbus-daemon address must be UTF-8");
        assert!(!line.is_empty(), "dbus-daemon printed no address line");
        line.trim_end_matches('\n').to_string()
    }

    /// 从匹配流里读下一条属于 `member` 的信号；超时或流结束返回 None。
    async fn next_signal(
        stream: &mut zbus::MessageStream,
        member: &str,
        deadline: Instant,
    ) -> Option<zbus::Message> {
        while Instant::now() < deadline {
            let next =
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), stream.next())
                    .await;
            let msg = match next {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(_))) | Ok(None) => continue,
                Err(_) => return None,
            };
            let header = msg.header();
            if header.member().map(|m| m.as_str()) == Some(member) {
                return Some(msg);
            }
        }
        None
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn drive_signals_emits_progress_and_done_and_drains_registry() {
        // 全程持有进程级测试闸：drive_signals 每 200ms `job_drain_done`，
        // 会抽走其他 job 测试尚未断言完成的 job——必须串行。parking_lot
        // 守护跨 `.await` 持有（await_holding_lock 由此豁免）。
        let _guard = JOB_TEST_MUTEX.lock();
        let _ = crate::job_drain_done();

        let bus = TestBus::start().await;
        let server = bus.connect().await;
        let _ = server
            .object_server()
            .at("/org/agentshell/Rootd", RootdInterface::new())
            .await
            .expect("register RootdInterface");
        let iface_ref = server
            .object_server()
            .interface::<_, RootdInterface>("/org/agentshell/Rootd")
            .await
            .expect("fetch InterfaceRef");

        let subscriber = bus.connect().await;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.agentshell.Rootd")
            .expect("interface name must parse")
            .build();
        let mut stream = zbus::MessageStream::for_match_rule(rule, &subscriber, Some(16))
            .await
            .expect("subscribe to Rootd signals");

        let server_conn = server.clone();
        let handle = tokio::spawn(async move { drive_signals(&server_conn, iface_ref).await });

        let id = job_create("PackageInstall");
        job_progress(&id, 0.5);
        tokio::time::sleep(Duration::from_millis(400)).await;
        job_progress(&id, 0.7);

        let progress_deadline = Instant::now() + Duration::from_secs(5);
        let progress_msg = next_signal(&mut stream, "JobProgress", progress_deadline)
            .await
            .expect("JobProgress signal must be received");
        let (job_id, progress): (String, f64) = progress_msg
            .body()
            .deserialize()
            .expect("JobProgress body must deserialize");
        assert_eq!(job_id, id);
        assert!(
            (progress - 0.7).abs() < f64::EPSILON,
            "progress was {progress}"
        );

        job_done_with(&id, true, Some(0), String::new());
        let done_deadline = Instant::now() + Duration::from_secs(5);
        let done_msg = next_signal(&mut stream, "JobDone", done_deadline)
            .await
            .expect("JobDone signal must be received");
        let (job_id, success): (String, bool) = done_msg
            .body()
            .deserialize()
            .expect("JobDone body must deserialize");
        assert_eq!(job_id, id);
        assert!(success, "job must report success");

        // drain 在 JobDone 发射后同步发生——该 job 必须已从注册表淘汰。
        assert!(
            job_status(&id).is_none(),
            "done job must be drained from registry"
        );
        handle.abort();
        let _ = crate::job_drain_done();
    }

    #[test]
    fn journal_query_uses_dedicated_timeout() {
        assert_eq!(command_timeout_for("JournalQuery"), JOURNAL_QUERY_TIMEOUT);
    }

    #[test]
    fn other_methods_use_default_timeout() {
        assert_eq!(command_timeout_for("ServiceStart"), COMMAND_TIMEOUT);
        assert_eq!(command_timeout_for("PackageInstall"), COMMAND_TIMEOUT);
    }

    #[test]
    fn slow_test_method_uses_short_timeout() {
        // 测试专用慢方法映射到亚秒超时，保证超时测试在 CI 快速完成。
        assert!(command_timeout_for("TestSlowMethod") < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn slow_dispatch_times_out_in_wrapper() {
        // 核心回归保护：移除 spawn_blocking+timeout 包装后，本测试会因
        // 阻塞线程占用测试线程而死锁或 panic，不再绿。
        let r = dispatch_with_timeout("TestSlowMethod", vec![]).await;
        let err = r.expect_err("slow command must be timed out");
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn fast_dispatch_returns_through_wrapper() {
        let r = dispatch_with_timeout("Hello", vec![]).await;
        assert!(r.is_ok(), "fast command must succeed: {r:?}");
    }

    #[tokio::test]
    async fn semaphore_limits_concurrency() {
        // 许可为 1：先占满，再分派必须排队等待，而非立即通过。
        let sem = tokio::sync::Semaphore::new(1);
        let permit = sem.try_acquire().expect("initial permit");
        let fut = dispatch_with_semaphore("Hello", vec![], &sem);
        tokio::pin!(fut);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut fut)
                .await
                .is_err(),
            "dispatch must wait for a permit while semaphore is exhausted",
        );
        drop(permit);
        assert!(fut.await.is_ok());
    }

    #[tokio::test]
    async fn timeout_kills_spawned_child() {
        // TestSpawnSleep spawn `sleep 30` 后阻塞等待；100ms 超时后必须
        // kill 子进程，不留孤儿。被 kill 的 pidfd 由包装函数返回，避免
        // 全局状态在多测试并行下互相覆盖；pidfd 探测与 PID 无关，不惧复用。
        let (r, killed) = dispatch_with_timeout_tracked("TestSpawnSleep", vec![]).await;
        let err = r.expect_err("spawning slow command must time out");
        assert!(err.contains("timed out"), "{err}");
        let pidfd = killed.expect("timed-out child pidfd must be captured");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    0,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                )
            };
            if rc != 0 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH),
                    "child should be gone; probe returned unexpected error",
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child not killed within 2s",
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

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
