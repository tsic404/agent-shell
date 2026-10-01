//! daemon 请求服务循环（§22.2 激活策略）。
//!
//! 两种接入形态共用同一循环（逐行读请求 → dispatch → 逐行写响应）：
//! - stdio：CLI/MCP fork/exec 的瞬态子进程，stdin EOF 或空闲超时退出；
//! - Unix socket（`--socket`）：常驻显式端点，多连接并发接入、请求在 daemon
//!   状态锁上串行执行，无请求达空闲超时即退出。

use crate::dispatch;
use crate::path_guard::describe_file_type;
use crate::state::Daemon;
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
pub(crate) struct BoundSocket {
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
pub(crate) fn bind_socket(path: &Path) -> Result<BoundSocket, String> {
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
pub(crate) async fn serve_stdio(daemon: Daemon, idle_timeout: Duration) {
    let daemon = Arc::new(Mutex::new(daemon));
    let out = Arc::new(Mutex::new(tokio::io::stdout()));
    serve_stream(&daemon, tokio::io::stdin(), out, Some(idle_timeout), None).await;
    tracing::info!("stdio session ended; exiting");
    shutdown(&daemon).await;
}

/// 监听形态：每条连接一个任务，共享同一 daemon 状态；无请求且无连接达空闲超时退出。
pub(crate) async fn serve_socket(bound: BoundSocket, daemon: Daemon, idle_timeout: Duration) {
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
                    spawn_connection(&daemon, stream, Arc::clone(&activity), Arc::clone(&connections));
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
    stream: UnixStream,
    activity: Arc<ActivityClock>,
    connections: Arc<AtomicUsize>,
) {
    let (reader, writer) = stream.into_split();
    let daemon = Arc::clone(daemon);
    let out = Arc::new(Mutex::new(writer));
    tokio::spawn(async move {
        let _alive = ConnectionGuard::acquire(&connections);
        serve_stream(&daemon, reader, out, None, Some(activity)).await;
    });
}

/// 请求服务循环：逐行读 → dispatch → 逐行写。连接 EOF 退出；`idle_timeout`
/// 为 `Some` 时本连接空闲超时退出（stdio 形态，监听形态的空闲退出在监听循环）。
///
/// 响应与通知共用同一 writer 行流；订阅转发任务独立 spawn，必须共享一个互斥
/// writer，否则并发写入会交织半行。本连接创建的订阅在退出时逐个从 hub 注销。
async fn serve_stream<R, W>(
    daemon: &Arc<Mutex<Daemon>>,
    reader: R,
    out: Arc<Mutex<W>>,
    idle_timeout: Option<Duration>,
    activity: Option<Arc<ActivityClock>>,
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
                        tracing::info!("idle timeout reached");
                        break;
                    }
                }
            }
            None => lines.read_line(&mut line).await,
        };
        let n = match read {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("read error: {e}");
                break;
            }
        };
        if n == 0 {
            tracing::info!("connection closed");
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
    use std::os::unix::fs::PermissionsExt;

    /// 客户端在订阅响应写出之前消失时，订阅仍必须被回收。
    ///
    /// 这条路径走的是「响应写失败 → break」：若能留下未登记的订阅，hub 表项永久
    /// 残留（`daemon status.subscribers` 虚高、事件继续被投递给无人接收的通道）。
    /// 用 duplex 造确定性场景：对端先写入请求再立即关闭，daemon 侧读得到请求、
    /// 写响应必然 BrokenPipe。
    #[tokio::test]
    async fn subscription_is_reclaimed_when_response_write_fails() {
        let daemon = Arc::new(Mutex::new(Daemon::connect(Duration::from_secs(1)).await));
        let (server, mut client) = tokio::io::duplex(4096);
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"events.subscribe\"}\n")
            .await
            .expect("write subscribe");
        // 对端消失：daemon 的响应写失败，连接循环走 break 路径。
        drop(client);

        let (reader, writer) = tokio::io::split(server);
        let out = Arc::new(Mutex::new(writer));
        serve_stream(&daemon, reader, out, None, None).await;

        assert_eq!(
            daemon.lock().await.hub.subscriber_count(),
            0,
            "订阅必须在连接退出时注销（响应写失败也不例外）"
        );
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
