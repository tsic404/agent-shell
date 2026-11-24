//! daemon 请求服务循环（§22.2 激活策略）。
//!
//! 两种接入形态共用同一循环（逐行读请求 → dispatch → 逐行写响应）：
//! - stdio：CLI/MCP fork/exec 的瞬态子进程，stdin EOF 或空闲超时退出；
//! - Unix socket（`--socket`）：常驻显式端点，多连接并发接入、请求在 daemon
//!   状态锁上串行执行，无请求达空闲超时即退出。

use crate::dispatch;
use crate::path_guard::describe_file_type;
use crate::state::Daemon;
use agent_shell_core::security::SecurityManager;
use agent_shell_rpc::{Notification, Request, Response};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use uuid::Uuid;

/// 监听模式的空闲检查间隔：只决定退出判定精度（最多晚一个 tick），不参与协议时序。
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// accept 报错后的退避：避免 EMFILE 等持续错误把监听循环打成忙等。
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// 退出前等待在途审计追加的上限：不让一次慢写拖住进程退出。
const EXIT_AUDIT_FLUSH: Duration = Duration::from_millis(300);

/// 监听模式的活动时钟：连入与每个请求都刷新，监听循环据此判定空闲退出。
struct ActivityClock {
    last: StdMutex<Instant>,
}

impl ActivityClock {
    fn new() -> Self {
        Self {
            last: StdMutex::new(Instant::now()),
        }
    }

    fn touch(&self) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        *last = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }
}

/// 绑定成功后的显式端点：listener + 绑定路径身份（dev/ino）。
///
/// Drop 时按身份复核后回收 socket 文件：空闲超时退出与 panic 展开（含装配期 panic）
/// 都不留残留端点。被信号终止（SIGKILL/SIGTERM）不执行析构，仍会留下文件——由下次
/// `bind_socket` 的连通性探测接管，与崩溃现场同一路径。
#[derive(Debug)]
pub struct BoundSocket {
    listener: UnixListener,
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        // 先复核 inode 再删：旧端点退出与新端点启动重叠时，路径可能已被新 daemon
        // 回收重建；按路径盲删会摘掉后来者的端点，让它变成无人可达的孤儿。
        let Ok(meta) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if (meta.dev(), meta.ino()) != (self.dev, self.ino) {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::warn!(socket = %self.path.display(), "cannot unlink socket endpoint: {e}");
        }
    }
}

/// 绑定监听 socket（显式端点）。
///
/// 路径已存在时按文件类型分流：**非 socket 对象（普通文件/目录/符号链接）一律拒绝**
/// ——`--socket` 指向用户数据时不得为了绑定而删除它；socket 文件则连一下判定：能连上
/// 说明有 daemon 正在服务该端点（拒绝启动，不抢活跃端点），连不上说明是上次崩溃留下的
/// 文件，回收后重新绑定。权限收紧到 0600——该 socket 即桌面控制面，不能放同机其他用户接入。
pub fn bind_socket(path: &Path) -> Result<BoundSocket, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    return Err(format!(
                        "another daemon is already listening on {}",
                        path.display()
                    ))
                }
                Err(_) => {
                    std::fs::remove_file(path).map_err(|e| {
                        format!("cannot remove stale socket {}: {e}", path.display())
                    })?;
                }
            }
        }
        Ok(meta) => {
            return Err(format!(
                "refusing to bind {}: existing path is a {}, not a unix socket \
                 (choose a different --socket path, or remove it yourself)",
                path.display(),
                describe_file_type(&meta.file_type())
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "cannot inspect socket path {}: {e}",
                path.display()
            ))
        }
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create socket dir {}: {e}", parent.display()))?;
        }
    }
    let listener = UnixListener::bind(path)
        .map_err(|e| format!("cannot bind socket {}: {e}", path.display()))?;
    // 先取身份再收紧权限：此后任何失败路径都由守卫 Drop 回收文件，不留半成品端点。
    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        format!(
            "cannot read identity of bound socket {}: {e}",
            path.display()
        )
    })?;
    let bound = BoundSocket {
        listener,
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot restrict socket {}: {e}", path.display()))?;
    Ok(bound)
}

/// stdio 形态：单连接，stdin EOF 或空闲超时退出。
pub async fn serve_stdio(daemon: Daemon, idle_timeout: Duration) {
    let gate = Gate::new(&daemon.security, &daemon.caller_id);
    let daemon = Arc::new(Mutex::new(daemon));
    let out = Arc::new(Mutex::new(tokio::io::stdout()));
    serve_stream(
        &daemon,
        &gate,
        tokio::io::stdin(),
        out,
        Some(idle_timeout),
        None,
        true,
    )
    .await;
    // 瞬态形态：本连接结束即进程退出，退出日志走非阻塞写（见 [`exit_info`]）。
    crate::log_exit("stdio session ended; exiting");
    shutdown(&daemon).await;
}

/// 监听形态：每条连接一个任务，共享同一 daemon 状态；无请求且无连接达空闲超时退出。
pub async fn serve_socket(bound: BoundSocket, daemon: Daemon, idle_timeout: Duration) {
    let gate = Arc::new(Gate::new(&daemon.security, &daemon.caller_id));
    let daemon = Arc::new(Mutex::new(daemon));
    let activity = Arc::new(ActivityClock::new());
    let connections = Arc::new(AtomicUsize::new(0));
    let mut tick = tokio::time::interval(IDLE_CHECK_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if should_exit(activity.idle_for(), idle_timeout, connections.load(Ordering::SeqCst)) {
                    tracing::info!("idle timeout reached; exiting");
                    break;
                }
            }
            accepted = bound.listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    activity.touch();
                    spawn_connection(
                        &daemon,
                        &gate,
                        stream,
                        Arc::clone(&activity),
                        Arc::clone(&connections),
                    );
                }
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                }
            },
        }
    }
    // 先摘端点（关 fd + 回收路径）再做收尾：portal Close 可能要等 D-Bus 往返，
    // 期间留着「文件在、却无人接受」的假端点，客户端会连上后无限等待。
    drop(bound);
    shutdown(&daemon).await;
}

/// 空闲退出判定：无请求达 `timeout` **且**没有客户端连着才退。
///
/// 连接存续本身也是理由——`events subscribe` 这类长连接在两次事件之间不发请求，
/// 若只看请求时间戳，端点会在订阅者等待期间自己退出，订阅者拿到 EOF、后续命令
/// 也找不到端点。
fn should_exit(idle_for: Duration, timeout: Duration, active_connections: usize) -> bool {
    active_connections == 0 && idle_for >= timeout
}

/// 连接计数守卫：连接任务存续期间计数 +1，任务结束（含 panic/取消）时 -1。
struct ConnectionGuard(Arc<AtomicUsize>);

impl ConnectionGuard {
    fn acquire(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(counter))
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 单条 socket 连接：读半交给请求循环，写半共享给订阅转发任务。
fn spawn_connection(
    daemon: &Arc<Mutex<Daemon>>,
    gate: &Arc<Gate>,
    stream: UnixStream,
    activity: Arc<ActivityClock>,
    connections: Arc<AtomicUsize>,
) {
    let (reader, writer) = stream.into_split();
    let daemon = Arc::clone(daemon);
    let gate = Arc::clone(gate);
    let out = Arc::new(Mutex::new(writer));
    tokio::spawn(async move {
        let _alive = ConnectionGuard::acquire(&connections);
        serve_stream(&daemon, &gate, reader, out, None, Some(activity), false).await;
    });
}

/// 退出临近的连接日志：瞬态 stdio 连接结束后进程即退出，日志走非阻塞
/// [`crate::log_exit`]——调用方 stderr 管道写满或无读者时，阻塞写会把
/// `std::process::exit` 拖住，进程带着单实例锁残留；socket 连接为 `false`，
/// daemon 继续服务其它连接，保持 tracing 结构化输出与级别过滤。
fn exit_info(transient: bool, msg: &str) {
    if transient {
        crate::log_exit(msg);
    } else {
        tracing::info!("{msg}");
    }
}

/// [`exit_info`] 的 warn 级别版本（瞬态形态只有纯文本行，无级别概念）。
fn exit_warn(transient: bool, msg: &str) {
    if transient {
        crate::log_exit(msg);
    } else {
        tracing::warn!("{msg}");
    }
}

/// 连接级门禁句柄：与 `dispatch` 共用同一份策略真值，外加调用方身份。
///
/// 存活探测不取业务状态锁（长命令占锁时仍须应答），但探测本身是一条已登记的
/// 远程方法：黑名单与确认覆盖对它同样生效。身份与策略在这里做成连接级句柄，
/// 快路径才能在不等待状态锁的前提下调用同一个 [`dispatch::gate`]。
/// `SecurityManager` 的克隆共享同一份配置（`Arc<RwLock<_>>`），`security.grant`
/// / `revoke` 的变更因此对快路径立即可见；调用方身份由 `AGENT_SHELL_AGENT_ID`
/// 在进程装配时注入、此后不变，连接建立时取快照与逐请求读取等价。
#[derive(Clone)]
pub(crate) struct Gate {
    caller_id: String,
    security: SecurityManager,
}

impl Gate {
    pub(crate) fn new(security: &SecurityManager, caller_id: &str) -> Self {
        Self {
            caller_id: caller_id.to_string(),
            security: security.clone(),
        }
    }

    /// 探测应答：门禁 → 结果 → 执行结果审计，与完整分派同源。
    fn answer_ping(&self, req: &Request) -> Response {
        match dispatch::gate(&self.security, &self.caller_id, req) {
            Ok(op) => {
                if let Some(op) = op {
                    self.security.record_execution(&self.caller_id, &op, true);
                }
                Response::ok(req.id, dispatch::daemon_ping_payload())
            }
            Err(refusal) => *refusal,
        }
    }
}

/// 请求服务循环：逐行读 → dispatch → 逐行写。连接 EOF 退出；`idle_timeout`
/// 为 `Some` 时本连接空闲超时退出（stdio 形态，监听形态的空闲退出在监听循环）。
///
/// 响应与通知共用同一 writer 行流；订阅转发任务独立 spawn，必须共享一个互斥
/// writer，否则并发写入会交织半行。本连接创建的订阅在退出时逐个从 hub 注销。
///
/// `transient`：本连接结束即进程退出（CLI/MCP fork/exec 的 stdio 形态）——退出
/// 临近的日志必须非阻塞，见 [`exit_info`]。
async fn serve_stream<R, W>(
    daemon: &Arc<Mutex<Daemon>>,
    gate: &Gate,
    reader: R,
    out: Arc<Mutex<W>>,
    idle_timeout: Option<Duration>,
    activity: Option<Arc<ActivityClock>>,
    transient: bool,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader);
    // 本连接创建的订阅 id：连接结束即注销（见函数末尾）。
    let mut subscription_ids: Vec<Uuid> = Vec::new();
    loop {
        // 空闲超时退出（§22.2 激活策略）：连接上无请求达 idle_timeout 即退，
        // systemd `Restart=on-failure` 语义下正常退出不重启。
        let mut line = String::new();
        let read = match idle_timeout {
            Some(timeout) => {
                match tokio::time::timeout(timeout, lines.read_line(&mut line)).await {
                    Ok(result) => result,
                    Err(_elapsed) => {
                        exit_info(transient, "idle timeout reached");
                        break;
                    }
                }
            }
            None => lines.read_line(&mut line).await,
        };
        let n = match read {
            Ok(n) => n,
            Err(e) => {
                exit_warn(transient, &format!("read error: {e}"));
                break;
            }
        };
        if n == 0 {
            exit_info(transient, "connection closed");
            break;
        }
        if let Some(clock) = activity.as_ref() {
            clock.touch();
        }

        let req = match Request::from_line(&line) {
            Ok(r) => r,
            Err(e) => {
                // 解析失败：id 不可知，回 id=0 的 ParseError（规范允许）。
                let resp = Response::err(0, agent_shell_rpc::RpcErrorCode::ParseError, e);
                if let Err(e) = write_line(&out, &resp.to_line()).await {
                    tracing::debug!("response write failed ({e}); closing connection");
                    break;
                }
                continue;
            }
        };
        // 存活探测不取状态锁：一条长命令（最多一整条命令预算）占着锁时，
        // 默认入口的可应答判定仍必须得到回答，否则会被误判为不可应答而
        // 另起瞬态实例（ADR-017）。免锁不等于免门禁：`daemon.ping` 是已
        // 登记的远程方法，黑名单、确认覆盖与审计规则与 dispatch 同一份
        // （C-104 / C-105 / C-107），由 [`Gate`] 承载。
        if req.method == agent_shell_rpc::method::DAEMON_PING {
            let resp = gate.answer_ping(&req);
            if let Err(e) = write_line(&out, &resp.to_line()).await {
                tracing::debug!("response write failed ({e}); closing connection");
                break;
            }
            continue;
        }
        // dispatch 与订阅取走必须在同一把状态锁内：多连接下若在两步之间让出，
        // 另一条连接的订阅句柄会被本连接转发到错误的 writer。
        let (resp, subs) = {
            let mut daemon = daemon.lock().await;
            let resp = dispatch::dispatch(&mut daemon, &req).await;
            let subs = std::mem::take(&mut daemon.subscriptions);
            (resp, subs)
        };
        // 先登记 id 再写响应：响应写失败会走下面的 break，函数末尾的注销据此覆盖这批
        // 订阅。若登记点在写之后，「响应已写失败而 id 未登记」会留下永久幽灵表项。
        for sub in &subs {
            subscription_ids.push(sub.id());
        }
        if let Err(e) = write_line(&out, &resp.to_line()).await {
            tracing::debug!("response write failed ({e}); closing connection");
            break;
        }

        // events.subscribe 的返回订阅句柄被 dispatch 暂存在 daemon；此处取走
        // 并 spawn 转发任务。订阅句柄 channel 关闭（unsubscribe）时任务退出。
        for mut sub in subs {
            let out = Arc::clone(&out);
            let clock = activity.clone();
            let daemon = Arc::clone(daemon);
            tokio::spawn(async move {
                let id = sub.id();
                while let Some(evt) = sub.recv().await {
                    let params = match serde_json::to_value(evt) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!("event serialize failed: {e}");
                            continue;
                        }
                    };
                    if let Err(e) = write_line(&out, &Notification::event(params).to_line()).await {
                        // 对端写不动了：立即注销自己的 hub 表项再退出，不等连接读循环
                        // 结束（对端只关读方向时表项会一直留着）。
                        tracing::debug!("event write failed ({e}); dropping subscription");
                        daemon.lock().await.hub.unsubscribe(id);
                        break;
                    }
                    // 事件写出同样是活动：订阅流活跃的端点不得按「无请求」判定空闲。
                    if let Some(clock) = clock.as_ref() {
                        clock.touch();
                    }
                }
            });
        }
    }

    // 连接结束（EOF / 读错误 / 空闲 / 写失败）：注销本连接创建的订阅。常驻端点
    // 下订阅长于「一条命令」的生命周期，不注销会永久累积 hub 表项、转发任务与
    // 写半 fd，且 `daemon status` 的 subscribers 长期虚高；订阅客户端被 Ctrl-C
    // 或 SIGKILL 时同样走这里。注销即 drop sender，转发任务随之收到 None 退出。
    if !subscription_ids.is_empty() {
        let daemon = daemon.lock().await;
        for id in subscription_ids {
            daemon.hub.unsubscribe(id);
        }
    }
}

/// 退出前收尾：关闭 portal ScreenCast 会话（D-Bus Close，审查项 #6）。
///
/// 长驻事件脚本不随本进程退出卸载：CLI 每条命令一个瞬态 daemon，退出即卸载
/// 会让 doctor 的事件脚本行在任何后续进程里恒为「未加载」——订阅过也报成
/// 从未装配。脚本实例留在 KWin 侧，装配状态跨进程可观察；实例堆积由下次
/// 装载前的固定名卸载（`unload_event_monitor`）收敛，同一时刻至多一个。
async fn shutdown(daemon: &Arc<Mutex<Daemon>>) {
    let daemon = daemon.lock().await;
    if let Some(capture) = daemon.capture.as_ref() {
        capture.shutdown().await;
    }
    drop(daemon);
    // 审计按进程独占分段同步追加：每次判定返回前记录已写进本进程的分段文件，
    // 退出没有跨文件迁移或内存队列要交接（C-107）。这里只等仍在途的追加写完，
    // 等待有墙钟上限，不把退出拖在慢盘上。
    if !agent_shell_core::audit::flush_pending(EXIT_AUDIT_FLUSH) {
        tracing::warn!("audit appends still in flight at exit; shutdown budget exhausted");
    }
}

/// 写一行并 flush。返回写结果——调用方据此判定连接是否还在：对端消失后既不得
/// 继续读循环，也不得再刷新空闲时钟（死订阅者续命端点的问题根源）。
async fn write_line<W: AsyncWrite + Unpin>(out: &Mutex<W>, line: &str) -> std::io::Result<()> {
    let mut out = out.lock().await;
    out.write_all(line.as_bytes()).await?;
    out.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDaemon;
    use agent_shell_core::security::ConfirmMode;
    use agent_shell_rpc::{method, RpcErrorCode};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    /// 测试用排他 `flock` 持有者：独立打开描述符模拟「另一个进程的写者」。
    ///
    /// daemon 测试直接调 `libc::flock` 造文件锁——被测的正是跨描述符/
    /// 跨进程争用，用同一套内核语义才等价。
    struct ExclusiveFlock {
        file: std::fs::File,
    }

    impl ExclusiveFlock {
        fn acquire(path: &std::path::Path) -> Self {
            use std::os::unix::io::AsRawFd;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .open(path)
                .expect("open audit holder");
            // SAFETY: flock 只作用于本描述符，参数与返回值均无指针解引用。
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            assert_eq!(rc, 0, "flock LOCK_EX on audit log");
            Self { file }
        }
    }

    impl Drop for ExclusiveFlock {
        fn drop(&mut self) {
            use std::os::unix::io::AsRawFd;
            // SAFETY: 同上，关闭锁随描述符释放，这里显式解锁让断言更直观。
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }

    /// 在一条真实 `serve_stream` 连接上跑一条请求并带回响应。
    ///
    /// 用 duplex 传输：编解码、读循环、门禁与写响应都是生产路径，只有字节
    /// 来源被替换，免得为一条探测拉起整个 socket 监听。
    async fn respond_once(daemon: Arc<Mutex<Daemon>>, gate: Gate, request: &str) -> Response {
        let (server, mut client) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(server);
        let out = Arc::new(Mutex::new(writer));
        let serving = tokio::spawn(async move {
            serve_stream(&daemon, &gate, reader, out, None, None, false).await;
        });
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut line)
            .await
            .expect("read response");
        let response = Response::from_line(&line).expect("response json");
        drop(client);
        serving.await.expect("serve task");
        response
    }

    /// `security.status` 对同一操作的结论：快路径与判定查询必须同源。
    async fn status_decision(daemon: &Arc<Mutex<Daemon>>, operation: &str) -> String {
        let mut d = daemon.lock().await;
        let resp = dispatch::dispatch(
            &mut d,
            &Request::new(
                99,
                method::SECURITY_STATUS,
                json!({ "operation": operation }),
            ),
        )
        .await;
        let result = resp.result.expect("security.status result");
        result["check"]["decision"]
            .as_str()
            .expect("decision string")
            .to_string()
    }

    /// 快路径必须走与 dispatch 同一门禁：黑名单命中时返回拒绝、审计留 deny、
    /// `security.status` 给同一结论（C-104 / C-105 / C-107）。
    #[tokio::test]
    async fn ping_fast_path_denies_like_dispatch_and_audits() {
        let (daemon, _sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        daemon
            .security
            .write_config()
            .permissions
            .deny
            .insert("daemon.ping".into(), true);
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let daemon = Arc::new(Mutex::new(daemon));

        let resp = respond_once(
            Arc::clone(&daemon),
            gate,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"daemon.ping\"}\n",
        )
        .await;
        assert_eq!(resp.id, 1);
        assert_eq!(
            resp.error.expect("denied ping must return an error").code,
            RpcErrorCode::Denied as i32
        );
        assert_eq!(status_decision(&daemon, "daemon.ping").await, "deny");

        let d = daemon.lock().await;
        let denies = d.security.audit.query("", "daemon.ping", "deny", None);
        assert_eq!(denies.len(), 1, "deny must be recorded once: {denies:?}");
        let executions = d.security.audit.query("", "daemon.ping", "allow", None);
        assert!(
            executions.is_empty(),
            "denied ping must not leave an execution record: {executions:?}"
        );
    }

    /// 确认覆盖对快路径同样生效：回执是「需确认」，判定查询与审计一致。
    #[tokio::test]
    async fn ping_fast_path_requires_confirmation_like_dispatch() {
        let (daemon, _sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        daemon
            .security
            .write_config()
            .operations
            .confirm
            .insert("daemon.ping".into(), ConfirmMode::Always);
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let daemon = Arc::new(Mutex::new(daemon));

        let resp = respond_once(
            Arc::clone(&daemon),
            gate,
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"daemon.ping\"}\n",
        )
        .await;
        assert_eq!(resp.id, 2);
        assert_eq!(
            resp.error
                .expect("confirm override must short-circuit")
                .code,
            RpcErrorCode::ConfirmationRequired as i32
        );
        assert_eq!(status_decision(&daemon, "daemon.ping").await, "confirm");

        let d = daemon.lock().await;
        let confirms = d.security.audit.query("", "daemon.ping", "confirm", None);
        assert_eq!(
            confirms.len(),
            1,
            "confirm decision must be recorded once: {confirms:?}"
        );
    }

    /// 放行路径与 dispatch 一样双记录：门禁 allow + 执行 allow(result=true)。
    #[tokio::test]
    async fn ping_fast_path_allows_and_records_execution() {
        let (daemon, _sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let daemon = Arc::new(Mutex::new(daemon));

        let resp = respond_once(
            Arc::clone(&daemon),
            gate,
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"daemon.ping\"}\n",
        )
        .await;
        assert_eq!(resp.id, 3);
        let result = resp.result.expect("allowed ping result");
        assert_eq!(result["service"], "agent-shell-daemon");
        assert_eq!(status_decision(&daemon, "daemon.ping").await, "allow");

        let d = daemon.lock().await;
        let entries = d.security.audit.query("", "daemon.ping", "allow", None);
        assert_eq!(entries.len(), 2, "gate + execution: {entries:?}");
        assert!(
            entries.iter().any(|e| !e.result),
            "gate record: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.result),
            "execution record: {entries:?}"
        );
    }

    /// 门禁判定不依赖业务状态锁：锁被长命令占着时，deny 仍须在探测预算内
    /// 返回，且证据与 `security.status` 一致。
    #[tokio::test]
    async fn ping_gate_runs_while_state_lock_is_held() {
        let (daemon, _sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        daemon
            .security
            .write_config()
            .permissions
            .deny
            .insert("daemon.ping".into(), true);
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let daemon = Arc::new(Mutex::new(daemon));
        let (server, mut client) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(server);
        let out = Arc::new(Mutex::new(writer));
        let serving = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            async move { serve_stream(&daemon, &gate, reader, out, None, None, false).await }
        });

        let guard = daemon.lock().await;
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"daemon.ping\"}\n")
            .await
            .expect("write ping");
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_millis(300),
            BufReader::new(&mut client).read_line(&mut line),
        )
        .await
        .expect("gate must answer while the state lock is held")
        .expect("read ping response");
        drop(guard);

        let resp = Response::from_line(&line).expect("ping response");
        assert_eq!(
            resp.error.expect("denied ping must return an error").code,
            RpcErrorCode::Denied as i32
        );
        drop(client);
        serving.await.expect("serve task");
    }

    /// 在 `connections` 条真实 `serve_stream` 连接上并发各发 `per_connection`
    /// 条 ping；返回后才允许断言，保证所有审计写入已经完成。
    async fn ping_over_concurrent_connections(
        daemon: &Arc<Mutex<Daemon>>,
        gate: &Gate,
        connections: usize,
        per_connection: usize,
    ) {
        let mut tasks = Vec::new();
        for connection in 0..connections {
            let daemon = Arc::clone(daemon);
            let gate = gate.clone();
            tasks.push(tokio::spawn(async move {
                let (server, mut client) = tokio::io::duplex(64 * 1024);
                let (reader, writer) = tokio::io::split(server);
                let out = Arc::new(Mutex::new(writer));
                let serving = tokio::spawn(async move {
                    serve_stream(&daemon, &gate, reader, out, None, None, false).await;
                });
                for i in 0..per_connection {
                    let request = format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"daemon.ping\"}}\n",
                        connection * per_connection + i + 1
                    );
                    client
                        .write_all(request.as_bytes())
                        .await
                        .expect("write ping");
                    let mut line = String::new();
                    BufReader::new(&mut client)
                        .read_line(&mut line)
                        .await
                        .expect("read ping response");
                    Response::from_line(&line).expect("ping response");
                }
                drop(client);
                serving.await.expect("serve task");
            }));
        }
        for task in tasks {
            task.await.expect("connection task");
        }
    }

    /// 全部审计文件（旧主日志 + 各进程分段）的非空行必须逐行可解析——拼行会
    /// 在这里显形。
    fn assert_audit_lines_parse(path: &std::path::Path, expected: usize) {
        let mut lines = 0;
        for file in agent_shell_core::audit::audit_files(path) {
            let raw = std::fs::read_to_string(&file).expect("read audit log");
            for line in raw.lines().filter(|l| !l.trim().is_empty()) {
                serde_json::from_str::<agent_shell_core::audit::AuditEntry>(line)
                    .unwrap_or_else(|e| panic!("不可解析的审计行 {line:?}: {e}"));
                lines += 1;
            }
        }
        assert_eq!(lines, expected, "审计记录条数不符（拼行或丢失）");
    }

    /// 多连接并发落审计时每条记录独占一行、查询不丢。
    ///
    /// 三态各跑一轮并发连接：deny / confirm 每请求一条，allow 为「门禁 allow
    /// + 执行 allow」各一条；每轮都核对原始行可解析与 `security.audit` 过滤
    /// 结果，覆盖门禁与执行记录在不同 runtime 线程交错落盘的路径（C-107）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_connections_keep_audit_records_whole() {
        let (daemon, sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let daemon = Arc::new(Mutex::new(daemon));
        let audit_path = sandbox.audit_path();
        let connections = 3;
        let per_connection = 40;

        // deny：每请求一条 deny，无执行记录。
        {
            let d = daemon.lock().await;
            d.security
                .write_config()
                .permissions
                .deny
                .insert("daemon.ping".into(), true);
        }
        ping_over_concurrent_connections(&daemon, &gate, connections, per_connection).await;
        assert_audit_lines_parse(&audit_path, connections * per_connection);
        {
            let d = daemon.lock().await;
            assert_eq!(
                d.security
                    .audit
                    .query("", "daemon.ping", "deny", None)
                    .len(),
                connections * per_connection,
                "deny 每请求一条且查询不丢"
            );
            assert!(d
                .security
                .audit
                .query("", "daemon.ping", "allow", None)
                .is_empty());
        }

        // confirm：每请求一条 confirm。
        {
            let d = daemon.lock().await;
            let mut config = d.security.write_config();
            config.permissions.deny.clear();
            config
                .operations
                .confirm
                .insert("daemon.ping".into(), ConfirmMode::Always);
        }
        ping_over_concurrent_connections(&daemon, &gate, connections, per_connection).await;
        assert_audit_lines_parse(&audit_path, connections * per_connection * 2);
        {
            let d = daemon.lock().await;
            assert_eq!(
                d.security
                    .audit
                    .query("", "daemon.ping", "confirm", None)
                    .len(),
                connections * per_connection,
                "confirm 每请求一条且查询不丢"
            );
        }

        // allow：每请求两条（门禁判定 + 执行结果），查询不丢。
        {
            let d = daemon.lock().await;
            let mut config = d.security.write_config();
            config.operations.confirm.clear();
        }
        ping_over_concurrent_connections(&daemon, &gate, connections, per_connection).await;
        assert_audit_lines_parse(&audit_path, connections * per_connection * 4);
        {
            let d = daemon.lock().await;
            let entries = d.security.audit.query("", "daemon.ping", "allow", None);
            assert_eq!(
                entries.len(),
                connections * per_connection * 2,
                "allow 两步记录都在且查询不丢"
            );
            assert_eq!(
                entries.iter().filter(|e| !e.result).count(),
                connections * per_connection,
                "门禁判定各一条"
            );
            assert_eq!(
                entries.iter().filter(|e| e.result).count(),
                connections * per_connection,
                "执行结果各一条"
            );
            assert_eq!(
                d.security
                    .audit
                    .query("", "daemon.ping", "deny", None)
                    .len()
                    + d.security
                        .audit
                        .query("", "daemon.ping", "confirm", None)
                        .len(),
                connections * per_connection * 2,
                "前两轮记录也仍在文件中"
            );
        }
    }

    /// 客户端在订阅响应写出之前消失时，订阅仍必须被回收。
    ///
    /// 这条路径走的是「响应写失败 → break」：若能留下未登记的订阅，hub 表项永久
    /// 残留（`daemon status.subscribers` 虚高、事件继续被投递给无人接收的通道）。
    /// 用 duplex 造确定性场景：对端先写入请求再立即关闭，daemon 侧读得到请求、
    /// 写响应必然 BrokenPipe。
    #[tokio::test]
    async fn subscription_is_reclaimed_when_response_write_fails() {
        // 沙箱随本测试存活：`serve_stream` 期间的门禁审计落沙箱内。
        let (daemon, _sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let daemon = Arc::new(Mutex::new(daemon));
        let (server, mut client) = tokio::io::duplex(4096);
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"events.subscribe\"}\n")
            .await
            .expect("write subscribe");
        // 对端消失：daemon 的响应写失败，连接循环走 break 路径。
        drop(client);

        let (reader, writer) = tokio::io::split(server);
        let out = Arc::new(Mutex::new(writer));
        // `transient = false`：本用例关心订阅注销，不涉及退出日志形态。
        serve_stream(&daemon, &gate, reader, out, None, None, false).await;

        assert_eq!(
            daemon.lock().await.hub.subscriber_count(),
            0,
            "订阅必须在连接退出时注销（响应写失败也不例外）"
        );
    }

    /// 默认入口的可应答判定（ADR-017）：一条长命令占着状态锁时，ping 仍须
    /// 立即应答。若探测也排在同一把锁后面，端点会被误判为不可应答，
    /// 命令转而派生瞬态实例——同一用户会话里出现第二个服务实例。
    #[tokio::test]
    async fn ping_answers_while_state_lock_is_held() {
        let (daemon, _sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let daemon = Arc::new(Mutex::new(daemon));
        let (server, mut client) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(server);
        let out = Arc::new(Mutex::new(writer));
        let serving = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            let gate = gate.clone();
            async move { serve_stream(&daemon, &gate, reader, out, None, None, false).await }
        });

        // 模拟长命令：状态锁在整个判定窗口内被占用。
        let guard = daemon.lock().await;
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"daemon.ping\"}\n")
            .await
            .expect("write ping");
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_millis(300),
            BufReader::new(&mut client).read_line(&mut line),
        )
        .await
        .expect("ping must be answered while the state lock is held")
        .expect("read ping response");
        drop(guard);
        assert_ne!(read, 0, "connection closed before ping response");

        let resp = Response::from_line(&line).expect("ping response");
        assert_eq!(resp.id, 7);
        assert_eq!(
            resp.result.as_ref().and_then(|v| v.get("service")),
            Some(&serde_json::json!("agent-shell-daemon"))
        );
        drop(client);
        serving.await.expect("serve task");
    }

    /// 旧主日志被另一个进程/描述符持锁时，`daemon.ping` 仍须在探测预算内应答，
    /// 且记录立刻落到本进程自己的分段里（不丢记录、也不等旧锁）。
    ///
    /// CLI 的「可应答」判定只有 500 ms（[`RESIDENT_PROBE_TIMEOUT`]）。审计排在
    /// ping 应答路径上；若退回共享主日志 + 文件锁，另一实例短暂持锁就会让健康
    /// 常驻实例错过窗口、被误判为不可应答而派生瞬态——默认入口的行为漂移
    /// （C-154 同类约束）。本用例同时覆盖与既有 `audit.jsonl` 的共存。
    #[tokio::test]
    async fn ping_answers_and_keeps_audit_when_foreign_lock_is_held() {
        let (daemon, sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let audit_path = sandbox.audit_path();
        let daemon = Arc::new(Mutex::new(daemon));

        // 独立打开描述符持排他锁：与另一个进程的写者等价（flock 按描述符）。
        let guard = ExclusiveFlock::acquire(&audit_path);

        let (server, mut client) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(server);
        let out = Arc::new(Mutex::new(writer));
        let serving = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            let gate = gate.clone();
            async move { serve_stream(&daemon, &gate, reader, out, None, None, false).await }
        });
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"daemon.ping\"}\n")
            .await
            .expect("write ping");
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_millis(500),
            BufReader::new(&mut client).read_line(&mut line),
        )
        .await
        .expect("ping must answer within the probe budget while the audit lock is held")
        .expect("read ping response");
        assert_ne!(read, 0, "connection closed before ping response");
        let resp = Response::from_line(&line).expect("ping response");
        assert_eq!(resp.id, 41);
        assert_eq!(
            resp.result.as_ref().and_then(|v| v.get("service")),
            Some(&serde_json::json!("agent-shell-daemon"))
        );
        drop(client);
        serving.await.expect("serve task");

        // 记录在判定返回前已同步追加到本进程分段：不用等旧锁释放。
        let entries = daemon.lock().await.security.audit.read_all();
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.op == "daemon.ping" && e.decision == "allow")
                .count(),
            2,
            "旧锁被持住时门禁与执行记录仍须完整落盘: {entries:?}"
        );
        assert!(
            std::fs::metadata(&audit_path).is_ok_and(|meta| meta.len() == 0),
            "新机制不得向被锁住的旧主日志追加字节"
        );

        drop(guard);
        let after = daemon.lock().await.security.audit.read_all();
        assert_eq!(after.len(), 2, "锁释放后不得补写出重复行: {after:?}");
        assert_eq!(
            daemon
                .lock()
                .await
                .security
                .audit
                .query("", "daemon.ping", "allow", Some(true))
                .len(),
            1,
            "执行结果记录必须可按维度查到"
        );
    }

    /// 退出不等待审计锁：记录在判定返回时已写入本进程分段，`shutdown` 有界
    /// 返回，且不迁移、不改写、不删除任何审计文件（C-107 / C-154）。
    #[tokio::test]
    async fn shutdown_is_bounded_and_keeps_written_audit_records() {
        let (daemon, sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let audit_path = sandbox.audit_path();
        let daemon = Arc::new(Mutex::new(daemon));

        let guard = ExclusiveFlock::acquire(&audit_path);
        let (server, mut client) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(server);
        let out = Arc::new(Mutex::new(writer));
        let serving = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            async move { serve_stream(&daemon, &gate, reader, out, None, None, false).await }
        });
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"daemon.ping\"}\n")
            .await
            .expect("write ping");
        let mut line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut line)
            .await
            .expect("read ping response");
        drop(client);
        serving.await.expect("serve task");

        let files_before = agent_shell_core::audit::audit_files(&audit_path);
        let bytes_before: Vec<Vec<u8>> = files_before
            .iter()
            .map(|file| std::fs::read(file).expect("read audit file"))
            .collect();

        // 旧锁仍被占用：退出不得等它，也不得写出旁路文件或改写主日志。
        let started = Instant::now();
        shutdown(&daemon).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(300),
            "退出不得等待外部审计锁，实际 {elapsed:?}"
        );
        let files_after = agent_shell_core::audit::audit_files(&audit_path);
        assert_eq!(files_before, files_after, "退出不得迁移或删除审计文件");
        let bytes_after: Vec<Vec<u8>> = files_after
            .iter()
            .map(|file| std::fs::read(file).expect("read audit file"))
            .collect();
        assert_eq!(bytes_before, bytes_after, "退出不得改写已落盘记录");

        let entries = daemon.lock().await.security.audit.read_all();
        assert_eq!(entries.len(), 2, "退出后记录必须可查: {entries:?}");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.op == "daemon.ping" && entry.decision == "allow")
                .count(),
            2
        );
        drop(guard);
    }

    /// 合成器夹具：`close_window` 进入后永不返回，模拟挂死的后端。
    struct HungCloseCompositor {
        entered: Arc<tokio::sync::Notify>,
    }

    fn hung_window() -> agent_shell_core::types::WindowInfo {
        use agent_shell_core::types::{
            DesktopEnvironment, Rect, WindowId, WindowInfo, WindowState, WindowType,
        };
        WindowInfo {
            id: WindowId {
                native_id: "w-1".into(),
                de_type: DesktopEnvironment::GNOME,
            },
            title: "Hung".into(),
            app_id: "hung".into(),
            pid: 42,
            geometry: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            frame_geometry: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            states: vec![WindowState::Normal],
            workspace_id: None,
            monitor_id: None,
            stacking_order: 0,
            desktop_file: None,
            window_type: WindowType::Normal,
            icon_geometry: None,
            keep_above: false,
        }
    }

    #[async_trait::async_trait]
    impl agent_shell_core::component::DesktopComponent for HungCloseCompositor {
        fn name(&self) -> &'static str {
            "hung-close"
        }

        fn component_type(&self) -> agent_shell_core::component::ComponentType {
            agent_shell_core::component::ComponentType::Compositor
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn health(&self) -> agent_shell_core::component::ComponentHealth {
            agent_shell_core::component::ComponentHealth::Healthy
        }
    }

    #[async_trait::async_trait]
    impl agent_shell_core::component::CompositorComponent for HungCloseCompositor {
        fn capabilities(&self) -> agent_shell_core::component::BackendCapabilities {
            agent_shell_core::component::BackendCapabilities::default()
        }

        async fn list_windows(
            &self,
        ) -> agent_shell_core::error::Result<Vec<agent_shell_core::types::WindowInfo>> {
            Ok(vec![hung_window()])
        }

        async fn get_active_window(
            &self,
        ) -> agent_shell_core::error::Result<Option<agent_shell_core::types::WindowInfo>> {
            Ok(None)
        }

        async fn focus_window(
            &self,
            _id: &agent_shell_core::types::WindowId,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn move_window(
            &self,
            _id: &agent_shell_core::types::WindowId,
            _x: i32,
            _y: i32,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn resize_window(
            &self,
            _id: &agent_shell_core::types::WindowId,
            _w: i32,
            _h: i32,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn minimize_window(
            &self,
            _id: &agent_shell_core::types::WindowId,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn unminimize_window(
            &self,
            _id: &agent_shell_core::types::WindowId,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn maximize_window(
            &self,
            _id: &agent_shell_core::types::WindowId,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn close_window(
            &self,
            _id: &agent_shell_core::types::WindowId,
        ) -> agent_shell_core::error::Result<()> {
            self.entered.notify_one();
            std::future::pending().await
        }

        async fn set_window_geometry(
            &self,
            _id: &agent_shell_core::types::WindowId,
            _geo: agent_shell_core::types::Rect,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn get_window_info(
            &self,
            _id: &agent_shell_core::types::WindowId,
        ) -> agent_shell_core::error::Result<agent_shell_core::types::WindowInfo> {
            Ok(hung_window())
        }

        async fn list_workspaces(
            &self,
        ) -> agent_shell_core::error::Result<Vec<agent_shell_core::types::WorkspaceInfo>> {
            Ok(Vec::new())
        }

        async fn activate_workspace(
            &self,
            _id: &agent_shell_core::types::WorkspaceId,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn move_window_to_workspace(
            &self,
            _wid: &agent_shell_core::types::WindowId,
            _ws: &agent_shell_core::types::WorkspaceId,
        ) -> agent_shell_core::error::Result<()> {
            Ok(())
        }

        async fn list_monitors(
            &self,
        ) -> agent_shell_core::error::Result<Vec<agent_shell_core::types::MonitorInfo>> {
            Ok(Vec::new())
        }

        async fn subscribe(
            &self,
        ) -> agent_shell_core::error::Result<Box<dyn agent_shell_core::event::EventStream>>
        {
            Err(agent_shell_core::error::AgentShellError::NotImplemented(
                "fixture has no event stream".into(),
            ))
        }
    }

    /// 挂死的窗口操作不得扣住 daemon 的请求串行化槽位：并发的后续请求必须在
    /// 同一实例上拿到响应，而不是一起挂死。
    #[tokio::test]
    async fn hung_window_op_does_not_block_later_requests() {
        let (mut daemon, _sandbox) = TestDaemon::start(Duration::from_secs(1)).await.into_parts();
        daemon.window_op_budget = Duration::from_millis(50);
        let entered = Arc::new(tokio::sync::Notify::new());
        daemon.install_acceptance_compositor(
            Arc::new(HungCloseCompositor {
                entered: Arc::clone(&entered),
            }),
            agent_shell_core::types::DesktopEnvironment::GNOME,
            event::EventSource::MutterExtension,
        );
        let gate = Gate::new(&daemon.security, &daemon.caller_id);
        let daemon = Arc::new(Mutex::new(daemon));

        // 连接 A：close 进入后端后挂住，直到预算耗尽才返回错误。
        let (a_server, a_client) = tokio::io::duplex(4096);
        let (a_reader, a_writer) = tokio::io::split(a_server);
        let a_daemon = Arc::clone(&daemon);
        let a_gate = gate.clone();
        let a_task = tokio::spawn(async move {
            serve_stream(
                &a_daemon,
                &a_gate,
                a_reader,
                Arc::new(Mutex::new(a_writer)),
                None,
                None,
                false,
            )
            .await;
        });
        let mut a_client = BufReader::new(a_client);
        let close_req = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"windows.op\",\
                         \"params\":{\"op\":\"close\",\"target\":\"w-1\"}}\n";
        a_client
            .get_mut()
            .write_all(close_req.as_bytes())
            .await
            .expect("write close request");
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("close must reach the backend");

        // 连接 B：A 仍占用串行化槽位时发起，必须在预算耗尽后拿到响应。
        let (b_server, b_client) = tokio::io::duplex(4096);
        let (b_reader, b_writer) = tokio::io::split(b_server);
        let b_daemon = Arc::clone(&daemon);
        let b_gate = gate.clone();
        let b_task = tokio::spawn(async move {
            serve_stream(
                &b_daemon,
                &b_gate,
                b_reader,
                Arc::new(Mutex::new(b_writer)),
                None,
                None,
                false,
            )
            .await;
        });
        let mut b_client = BufReader::new(b_client);
        b_client
            .get_mut()
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"windows.list\"}\n")
            .await
            .expect("write list request");
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), b_client.read_line(&mut line))
            .await
            .expect("a later request must not be blocked by the hung window op")
            .expect("read list response");
        let list_resp: serde_json::Value =
            serde_json::from_str(line.trim()).expect("list response json");
        assert!(
            list_resp.get("error").is_none(),
            "later request must succeed: {list_resp}"
        );
        assert_eq!(
            list_resp["result"]["windows"][0]["native_id"].as_str(),
            Some("w-1")
        );

        // A 的 close 在预算耗尽后以错误响应返回，而不是继续挂住。
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), a_client.read_line(&mut line))
            .await
            .expect("close must return within the budget")
            .expect("read close response");
        let close_resp: serde_json::Value =
            serde_json::from_str(line.trim()).expect("close response json");
        assert_eq!(close_resp["error"]["code"].as_i64(), Some(1005));
        assert!(
            close_resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("did not complete"),
            "timeout message must be user-facing: {close_resp}"
        );

        a_task.abort();
        b_task.abort();
    }

    #[tokio::test]
    async fn bind_creates_owner_only_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.sock");
        let _bound = bind_socket(&path).expect("bind on free path");
        let mode = std::fs::metadata(&path)
            .expect("socket metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "socket must be owner-only: {mode:o}");
    }

    #[tokio::test]
    async fn bind_refuses_live_endpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.sock");
        let _live = bind_socket(&path).expect("first bind");
        let err = bind_socket(&path).expect_err("second daemon must not steal a live endpoint");
        assert!(
            err.contains("already listening") && err.contains(&path.display().to_string()),
            "error must name the live endpoint: {err}"
        );
    }

    #[tokio::test]
    async fn bind_reclaims_stale_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.sock");
        // 裸 listener（不经过守卫）：等价于被信号终止的 daemon——不跑析构，文件残留。
        drop(std::os::unix::net::UnixListener::bind(&path).expect("stale listener"));
        assert!(path.exists(), "stale socket file must remain after drop");
        let _bound = bind_socket(&path).expect("stale socket file must be reclaimed");
    }

    /// 正常退出（守卫 Drop）回收绑定路径：同路径立刻可重新绑定，不依赖陈旧接管。
    #[tokio::test]
    async fn drop_unlinks_bound_socket_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.sock");
        let bound = bind_socket(&path).expect("bind on free path");
        assert!(path.exists(), "endpoint must exist while bound");
        drop(bound);
        assert!(!path.exists(), "clean exit must unlink the bound path");
    }

    /// 路径已被新 daemon 重建时，旧守卫不得按路径盲删——身份（dev/ino）不符即放手。
    #[tokio::test]
    async fn drop_spares_socket_bound_by_another_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.sock");
        let old = bind_socket(&path).expect("bind old endpoint");
        std::fs::remove_file(&path).expect("remove old path");
        let _replacement = std::os::unix::net::UnixListener::bind(&path).expect("bind replacement");
        drop(old);
        assert!(
            path.exists(),
            "guard must not unlink a replacement endpoint"
        );
        std::os::unix::net::UnixStream::connect(&path).expect("replacement must stay reachable");
    }

    /// 路径已先被外部摘除时静默通过：析构期 panic 会把收尾变成 abort。
    #[tokio::test]
    async fn drop_tolerates_path_already_unlinked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-shell.sock");
        let bound = bind_socket(&path).expect("bind on free path");
        std::fs::remove_file(&path).expect("unlink path externally");
        drop(bound);
    }

    #[tokio::test]
    async fn bind_creates_missing_parent_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/agent-shell.sock");
        let _bound = bind_socket(&path).expect("bind with missing parent");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn bind_refuses_regular_file_without_deleting_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("precious.txt");
        std::fs::write(&path, "user data").expect("write file");
        let err = bind_socket(&path).expect_err("must not delete a non-socket path");
        assert!(
            err.contains("regular file") && err.contains(&path.display().to_string()),
            "error must name the file and its type: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("file must survive"),
            "user data"
        );
    }

    #[tokio::test]
    async fn bind_refuses_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sock-dir");
        std::fs::create_dir(&path).expect("create dir");
        let err = bind_socket(&path).expect_err("must not bind over a directory");
        assert!(err.contains("directory"), "error must name the type: {err}");
        assert!(path.is_dir(), "directory must survive");
    }

    /// 空闲退出：无请求达超时且**没有客户端连着**才退；连接存续期间不退
    /// （`events subscribe` 这类长连接在两次事件之间不发请求）。
    #[test]
    fn idle_exit_requires_no_active_connection() {
        let timeout = Duration::from_secs(30);
        assert!(!should_exit(Duration::from_secs(29), timeout, 0));
        assert!(should_exit(timeout, timeout, 0));
        assert!(should_exit(Duration::from_secs(60), timeout, 0));
        assert!(
            !should_exit(Duration::from_secs(60), timeout, 1),
            "a connected client keeps the endpoint alive"
        );
    }

    #[test]
    fn connection_guard_counts_and_releases() {
        let counter = Arc::new(AtomicUsize::new(0));
        let first = ConnectionGuard::acquire(&counter);
        let second = ConnectionGuard::acquire(&counter);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        drop(second);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        drop(first);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    /// 活动时钟：touch 后空闲时长回零，未 touch 时单调增长。
    #[test]
    fn activity_clock_tracks_touches() {
        let clock = ActivityClock::new();
        std::thread::sleep(Duration::from_millis(20));
        assert!(clock.idle_for() >= Duration::from_millis(20));
        clock.touch();
        assert!(clock.idle_for() < Duration::from_millis(20));
    }
}
