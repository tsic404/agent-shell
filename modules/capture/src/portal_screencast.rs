//! portal ScreenCast → PipeWire 流式捕获（设计文档 §13.2）。
//!
//! 流程固定五步：CreateSession → SelectSources → Start →
//! OpenPipeWireRemote 拿 (node_id, fd) → PipeWireNode 订阅节点取帧。
//!
//! 会话由常驻 daemon 持有复用（§21.22/21.24）：CLI 瞬态建会话会反复弹窗。
//! ScreenCast 超时 10s / 重试 1 次（§19.3）。
//!
//! **无交互授权路径**（§22.7 D5）：`SelectSources` 携带 `persist_mode` +
//! `restore_token`——portal 静默恢复先前会话，Start 不弹窗；
//! Start 响应返回新 `restore_token` 由调用方持久化（token 单次有效）。

use std::collections::HashMap;
use std::time::Duration;

use agent_shell_core::error::{dbus_error, AgentShellError, Result};
use zbus::zvariant::{self, ObjectPath};

use crate::portal_common::{
    drain_response_with_timeout, portal_proxy, prepare_response_stream, sender_part,
    wait_for_response, PORTAL_SERVICE,
};
use crate::types::{CaptureTarget, Frame, PixelFormat};

/// portal 会话持久化模式（xdg-desktop-portal ScreenCast §SelectSources persist_mode）。
///
/// 0 = 不持久化；1 = 应用运行期间持久；2 = 持久至显式撤销。
/// 设计文档 §22.7 用 3 表示 persist_until_revoked，实际 portal 规范值为 2。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistMode {
    /// 不持久化（默认）。
    None,
    /// 应用运行期间持久。
    WhileRunning,
    /// 持久至显式撤销——daemon 重启后可静默恢复。
    UntilRevoked,
}

impl PersistMode {
    fn as_u32(self) -> u32 {
        match self {
            PersistMode::None => 0,
            PersistMode::WhileRunning => 1,
            PersistMode::UntilRevoked => 2,
        }
    }
}

/// 建立 ScreenCast 会话的可选参数。
///
/// `restore_token` 用于尝试恢复先前持久化的会话（避免弹窗）；
/// `persist_mode` 控制本次会话是否持久化、返回新 token。
#[derive(Clone, Debug, Default)]
pub struct ScreenCastOptions {
    /// 尝试恢复的 restore_token（上次 Start 返回的新 token）。
    pub restore_token: Option<String>,
    /// 本次会话的持久化模式。None = 不设 persist_mode（portal 默认 0）。
    pub persist_mode: Option<PersistMode>,
}

/// ScreenCast 通道默认超时（§19.3）。
pub const SCREENCAST_TIMEOUT: Duration = Duration::from_secs(10);

/// ScreenCast 子预算内为释放 portal 资源预留的固定清理时间。
///
/// 取消未完成的 `org.freedesktop.portal.Request` 与关闭已创建的
/// `org.freedesktop.portal.Session` 各占一半。调用方须从 ScreenCast
/// 探测预算中扣除它，保证超时清理不挤占后续交互截图。
pub const SCREENCAST_CLEANUP_BUDGET: Duration = Duration::from_millis(200);

/// 清理预算中等待 PipeWire worker 退出的上限。
const SCREENCAST_WORKER_SHUTDOWN_BUDGET: Duration = Duration::from_millis(100);

/// 清理预算中用于 Request.Close / Session.Close 的总额。
const SCREENCAST_PORTAL_CLOSE_BUDGET: Duration = Duration::from_millis(100);

/// 通过 ScreenCast portal 建立 PipeWire 流并持续取帧。
///
/// 内部线程运行 PipeWire main loop；`capture_frame` 从共享槽位
/// 取最新帧。会话句柄由本对象持有，Drop 时关闭 portal session。
pub struct ScreenCastCapture {
    conn: zbus::Connection,
    session_path: ObjectPath<'static>,
    /// 是否已成功显式关闭 portal 会话；Drop 据此决定是否补发 Close。
    closed: std::sync::atomic::AtomicBool,
    /// 关闭信号：Drop 时通知内部线程退出 main loop 并释放资源。
    shutdown: std::sync::mpsc::Sender<()>,
    /// worker 退出通知；Drop 用它把同步 join 限制在清理预算内。
    /// 用 Mutex 包裹使接收端满足 `Sync`（捕获器放入 `Arc` 共享）。
    worker_done: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// 清理是否已由显式 shutdown 或 Drop 接管；避免 Drop 二次计时。
    shutdown_handled: std::sync::atomic::AtomicBool,
    /// worker 是否已在清理截止内退出；Drop 只在确认退出后同步 join。
    worker_finished: std::sync::atomic::AtomicBool,
    /// 最新帧共享槽位（worker 写，capture_frame 读）。
    latest: std::sync::Arc<(std::sync::Mutex<Option<Frame>>, std::sync::Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// `start_with_options` 产出：捕获器 + Start 返回的新 `restore_token`。
///
/// 调用方应在持久化存储中用此新 token 覆盖旧值（token 单次有效）。
pub struct ScreenCastSession {
    /// 捕获器（`capture_frame` / `close_session` 委托给它）。
    pub capture: ScreenCastCapture,
    /// Start 响应返回的新 restore_token（persist_mode 授权后才有）。
    pub restore_token: Option<String>,
}

impl ScreenCastCapture {
    /// 走完整 portal 五步流程建立流（无持久化/恢复）。
    ///
    /// 阻塞直至用户在 portal 弹窗确认（或超时 `SCREENCAST_TIMEOUT`）。
    pub async fn start(conn: zbus::Connection, target: CaptureTarget) -> Result<Self> {
        let session = Self::start_with_options(conn, target, &ScreenCastOptions::default()).await?;
        Ok(session.capture)
    }

    /// 走完整 portal 五步流程，携带 `persist_mode` / `restore_token`。
    ///
    /// 传 `restore_token` 时 portal 尝试静默恢复先前会话——恢复成功则
    /// Start 不弹窗；失败则正常弹窗（portal 忽略无效 token）。
    /// 设 `persist_mode` 后 Start 响应携带新 `restore_token`，经返回值
    /// [`ScreenCastSession::restore_token`] 交调用方持久化。
    pub async fn start_with_options(
        conn: zbus::Connection,
        target: CaptureTarget,
        opts: &ScreenCastOptions,
    ) -> Result<ScreenCastSession> {
        let proxy = portal_proxy(&conn, "org.freedesktop.portal.ScreenCast")
            .await
            .map_err(|e| AgentShellError::DBus(format!("ScreenCast proxy: {e}")))?;
        Self::start_via_proxy(&conn, &proxy, target, opts, None).await
    }

    /// 在绝对截止前走完五步流程。
    ///
    /// `deadline` 覆盖全部建立等待，包括 Request 订阅注册（总线 AddMatch）与
    /// 调用方已消耗的前置时间。截止到达后先取消未完成的 Request、再关闭已
    /// 创建的 portal Session，之后才返回错误；调用方另需预留
    /// [`SCREENCAST_CLEANUP_BUDGET`]。
    pub async fn start_with_deadline(
        conn: zbus::Connection,
        target: CaptureTarget,
        opts: &ScreenCastOptions,
        deadline: tokio::time::Instant,
    ) -> Result<ScreenCastSession> {
        let proxy = match tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            portal_proxy(&conn, "org.freedesktop.portal.ScreenCast"),
        )
        .await
        {
            Ok(Ok(proxy)) => proxy,
            Ok(Err(e)) => return Err(AgentShellError::DBus(format!("ScreenCast proxy: {e}"))),
            Err(_) => {
                return Err(AgentShellError::Timeout(
                    "portal ScreenCast proxy exceeded the ScreenCast deadline".into(),
                ))
            }
        };
        Self::start_via_proxy(&conn, &proxy, target, opts, Some(deadline)).await
    }

    async fn start_via_proxy(
        conn: &zbus::Connection,
        proxy: &zbus::Proxy<'_>,
        target: CaptureTarget,
        opts: &ScreenCastOptions,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<ScreenCastSession> {
        let pid = std::process::id();
        // 每步使用不同的 handle_token——portal Request 路径由
        // `/org/freedesktop/portal/desktop/request/<sender>/<token>` 计算，
        // 重用同一 token 会导致路径冲突，后端仅响应首个请求。
        let sender = sender_part(conn)
            .ok_or_else(|| AgentShellError::DBus("no unique name on session bus".into()))?;
        let create_token = format!("agent_shell_sc_create_{pid}");
        let select_token = format!("agent_shell_sc_select_{pid}");
        let start_token = format!("agent_shell_sc_start_{pid}");
        let req_prefix = "/org/freedesktop/portal/desktop/request/";
        // 每步等待上限：有总预算时取剩余时间，且不超过单步默认值。
        let remaining = |deadline: Option<tokio::time::Instant>| match deadline {
            Some(d) => d
                .saturating_duration_since(tokio::time::Instant::now())
                .min(SCREENCAST_TIMEOUT),
            None => SCREENCAST_TIMEOUT,
        };
        let mut cleanup = PortalSessionCleanup::new(conn.clone());

        // 1. CreateSession：方法返回值是 **Request 对象路径**；
        //    真正的 session_handle 在其 Response 信号的
        //    results["session_handle"] 里（xdg-desktop-portal 规范）。
        let create_path = ObjectPath::try_from(format!("{req_prefix}{sender}/{create_token}"))
            .map_err(|e| AgentShellError::DBus(format!("create path: {e}")))?;
        let mut create_stream = match wait_within(
            deadline,
            "CreateSession subscription",
            prepare_response_stream(conn, &create_path),
        )
        .await
        {
            Ok(stream) => stream,
            Err(e) => {
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(e);
            }
        };
        cleanup.track_request(create_path.as_str().to_owned());
        let mut o = std::collections::HashMap::<&str, zvariant::Value>::new();
        o.insert("handle_token", zvariant::Value::from(create_token.as_str()));
        o.insert(
            "session_handle_token",
            zvariant::Value::from(format!("agent_shell_{pid}")),
        );
        // CreateSession 非幂等（服务端真实创建 portal session）——重试若首次
        // 调用已到达 portal 而回复丢失，会留下永不被 Close 的孤儿 session。
        // 故不重试：瞬时失败由 capture 组件的三级降级链（ScreenCast → Screenshot
        // → X11）兜底，AccessDenied 经 dbus_error 归一到 Permission（不重试）。
        let create_body = (o,);
        let step = remaining(deadline);
        let create_request: zvariant::OwnedObjectPath =
            match tokio::time::timeout(step, proxy.call("CreateSession", &create_body)).await {
                Ok(Ok(path)) => path,
                Ok(Err(e)) => {
                    let e = dbus_error(format!("CreateSession: {e}"));
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(e);
                }
                Err(_) => {
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(AgentShellError::Timeout(format!(
                        "portal CreateSession method timed out after {}ms",
                        step.as_millis()
                    )));
                }
            };
        if create_request.as_str() != create_path.as_str() {
            cleanup.track_request(create_request.as_str().to_owned());
        }
        // 注意：wait_for_response 在方法返回后才订阅 Response 信号，
        // 重新引入竞态——但此路径仅在后端不按规范返回路径时触发（非默认路径）。
        let (_, create_results) = if create_request.as_str() == create_path.as_str() {
            match drain_response_with_timeout(
                &mut create_stream,
                remaining(deadline),
                "CreateSession",
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(e);
                }
            }
        } else {
            tracing::warn!(
                "CreateSession path mismatch: expected {}, got {}",
                create_path,
                create_request
            );
            match wait_within(
                deadline,
                "CreateSession response (path mismatch)",
                wait_for_response(conn, &create_request, remaining(deadline)),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(e);
                }
            }
        };
        cleanup.clear_request();
        let Some(session_handle) =
            crate::portal_common::string_field(&create_results, "session_handle")
        else {
            cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
            return Err(AgentShellError::Capture(
                "ScreenCast: no session_handle in CreateSession response".into(),
            ));
        };
        let session_path: ObjectPath<'static> =
            match zvariant::ObjectPath::try_from(session_handle.to_owned()) {
                Ok(path) => path.into_owned(),
                Err(e) => {
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(AgentShellError::Capture(format!("bad session_handle: {e}")));
                }
            };
        // CreateSession 成功即持有远端会话：此后任何失败/超时都必须 Close。
        cleanup.track_session(session_path.clone());

        // 2. SelectSources：persist_mode + restore_token 在此传入
        //    （xdg-desktop-portal ScreenCast §SelectSources）。
        let select_path = ObjectPath::try_from(format!("{req_prefix}{sender}/{select_token}"))
            .map_err(|e| AgentShellError::DBus(format!("select path: {e}")))?;
        let mut select_stream = match wait_within(
            deadline,
            "SelectSources subscription",
            prepare_response_stream(conn, &select_path),
        )
        .await
        {
            Ok(stream) => stream,
            Err(e) => {
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(e);
            }
        };
        cleanup.track_request(select_path.as_str().to_owned());
        let mut o = std::collections::HashMap::<&str, zvariant::Value>::new();
        o.insert("handle_token", zvariant::Value::from(select_token.as_str()));
        o.insert("types", zvariant::Value::from(target.source_type_u32()));
        o.insert("multiple", zvariant::Value::from(false));
        if let Some(pm) = opts.persist_mode {
            o.insert("persist_mode", zvariant::Value::from(pm.as_u32()));
        }
        if let Some(rt) = &opts.restore_token {
            o.insert("restore_token", zvariant::Value::from(rt.as_str()));
        }
        let step = remaining(deadline);
        let select_request: zvariant::OwnedObjectPath = match tokio::time::timeout(
            step,
            proxy.call::<_, (
                &ObjectPath<'_>,
                std::collections::HashMap<&str, zvariant::Value>,
            ), zvariant::OwnedObjectPath>("SelectSources", &(&session_path, o)),
        )
        .await
        {
            Ok(Ok(path)) => path,
            Ok(Err(e)) => {
                let e = dbus_error(format!("SelectSources: {e}"));
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(e);
            }
            Err(_) => {
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(AgentShellError::Timeout(format!(
                    "portal SelectSources method timed out after {}ms",
                    step.as_millis()
                )));
            }
        };
        if select_request.as_str() != select_path.as_str() {
            cleanup.track_request(select_request.as_str().to_owned());
        }
        if select_request.as_str() == select_path.as_str() {
            match drain_response_with_timeout(
                &mut select_stream,
                remaining(deadline),
                "SelectSources",
            )
            .await
            {
                Ok(_) => {}
                Err(e) => {
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(e);
                }
            }
        } else {
            // 同 CreateSession 路径失配回退：wait_for_response 在方法返回后
            // 才订阅 Response 信号，重新引入竞态——但此路径仅在后端不按规范
            // 返回路径时触发（非默认路径）。
            tracing::warn!(
                "SelectSources path mismatch: expected {}, got {}",
                select_path,
                select_request
            );
            match wait_within(
                deadline,
                "SelectSources response (path mismatch)",
                wait_for_response(conn, &select_request, remaining(deadline)),
            )
            .await
            {
                Ok(_) => {}
                Err(e) => {
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(e);
                }
            }
        }
        cleanup.clear_request();

        // 3. Start：触发用户确认弹窗（或静默恢复）；Response 携带 streams
        //    数组与可选的新 restore_token。
        let start_path = ObjectPath::try_from(format!("{req_prefix}{sender}/{start_token}"))
            .map_err(|e| AgentShellError::DBus(format!("start path: {e}")))?;
        let mut start_stream = match wait_within(
            deadline,
            "Start subscription",
            prepare_response_stream(conn, &start_path),
        )
        .await
        {
            Ok(stream) => stream,
            Err(e) => {
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(e);
            }
        };
        cleanup.track_request(start_path.as_str().to_owned());
        let mut o = std::collections::HashMap::<&str, zvariant::Value>::new();
        o.insert("handle_token", zvariant::Value::from(start_token.as_str()));
        let step = remaining(deadline);
        let start_request: zvariant::OwnedObjectPath = match tokio::time::timeout(
            step,
            proxy.call::<_, (
                &ObjectPath<'_>,
                &str,
                std::collections::HashMap<&str, zvariant::Value>,
            ), zvariant::OwnedObjectPath>("Start", &(&session_path, "", o)),
        )
        .await
        {
            Ok(Ok(path)) => path,
            Ok(Err(e)) => {
                let e = dbus_error(format!("Start: {e}"));
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(e);
            }
            Err(_) => {
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(AgentShellError::Timeout(format!(
                    "portal Start method timed out after {}ms",
                    step.as_millis()
                )));
            }
        };
        if start_request.as_str() != start_path.as_str() {
            cleanup.track_request(start_request.as_str().to_owned());
        }
        let (_, results) = if start_request.as_str() == start_path.as_str() {
            match drain_response_with_timeout(&mut start_stream, remaining(deadline), "Start").await
            {
                Ok(v) => v,
                Err(e @ AgentShellError::Timeout(_)) => {
                    // KDE 后端在 PipeWire 目标解析失败时不会发送 Response
                    // （`Stream error: target not found`），
                    // 超时是 agent-shell 的预期兜底——提示操作者查 portal 日志。
                    tracing::warn!(
                        "ScreenCast Start: portal Response never arrived (check PipeWire state and \
                         `journalctl --user -u plasma-xdg-desktop-portal-kde.service`): {e}"
                    );
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(e);
                }
                Err(e) => {
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(e);
                }
            }
        } else {
            // 同 CreateSession 路径失配回退：wait_for_response 在方法返回后
            // 才订阅 Response 信号，重新引入竞态——但此路径仅在后端不按规范
            // 返回路径时触发（非默认路径）。
            tracing::warn!(
                "Start path mismatch: expected {}, got {}",
                start_path,
                start_request
            );
            match wait_within(
                deadline,
                "Start response (path mismatch)",
                wait_for_response(conn, &start_request, remaining(deadline)),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                    return Err(e);
                }
            }
        };
        cleanup.clear_request();
        let Some(node_id) = extract_node_id(&results) else {
            cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
            return Err(AgentShellError::Capture(
                "ScreenCast: no stream node in response".into(),
            ));
        };
        // Start 响应可能携带新 restore_token（persist_mode 授权后）。
        let new_restore_token =
            crate::portal_common::string_field(&results, "restore_token").map(str::to_owned);

        // 4. OpenPipeWireRemote → fd
        let step = remaining(deadline);
        let fd: zbus::zvariant::OwnedFd = match tokio::time::timeout(
            step,
            proxy.call(
                "OpenPipeWireRemote",
                &(&session_path, HashMap::<&str, zvariant::Value>::new()),
            ),
        )
        .await
        {
            Ok(Ok(fd)) => fd,
            Ok(Err(e)) => {
                let e = dbus_error(format!("OpenPipeWireRemote: {e}"));
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(e);
            }
            Err(_) => {
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(AgentShellError::Timeout(format!(
                    "portal OpenPipeWireRemote timed out after {}ms",
                    step.as_millis()
                )));
            }
        };

        // 5. 启动 PipeWire 订阅线程
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let (worker_done_tx, worker_done) = std::sync::mpsc::channel::<()>();
        let latest = std::sync::Arc::new((
            std::sync::Mutex::<Option<Frame>>::new(None),
            std::sync::Condvar::new(),
        ));
        let latest_for_thread = std::sync::Arc::clone(&latest);
        // OwnedFd 是 RawFd 包装；跨线程交给 pw 前复制出 owned fd。
        use std::os::fd::AsFd as _;
        let raw_fd = match fd.as_fd().try_clone_to_owned() {
            Ok(fd) => fd,
            Err(e) => {
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(AgentShellError::Capture(format!("dup pipewire fd: {e}")));
            }
        };
        let worker = match std::thread::Builder::new()
            .name("pw-screencast".into())
            .spawn(move || {
                #[cfg(feature = "portal-test-hooks")]
                crate::portal_common::test_hooks::apply_worker_start_delay();
                run_pipewire_node(raw_fd, node_id, latest_for_thread, shutdown_rx);
                let _ = worker_done_tx.send(());
            }) {
            Ok(worker) => worker,
            Err(e) => {
                cleanup.release(SCREENCAST_CLEANUP_BUDGET).await;
                return Err(AgentShellError::Other(Box::new(e)));
            }
        };

        let session_path = cleanup.take_session();
        Ok(ScreenCastSession {
            capture: Self {
                conn: conn.clone(),
                session_path,
                closed: std::sync::atomic::AtomicBool::new(false),
                shutdown: shutdown_tx,
                worker_done: std::sync::Mutex::new(Some(worker_done)),
                shutdown_handled: std::sync::atomic::AtomicBool::new(false),
                worker_finished: std::sync::atomic::AtomicBool::new(false),
                latest,
                worker: Some(worker),
            },
            restore_token: new_restore_token,
        })
    }

    /// portal 服务是否可达（装配探测用；不弹窗）。
    pub async fn available(conn: &zbus::Connection) -> bool {
        use zbus::fdo::DBusProxy;
        match DBusProxy::new(conn).await {
            Ok(dbus) => dbus
                .name_has_owner(PORTAL_SERVICE.try_into().unwrap())
                .await
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// 取最新一帧（自上次调用以来的最新帧；尚无帧则阻塞等待至多一个超时周期）。
    pub async fn capture_frame(&self) -> Result<Frame> {
        let slot = std::sync::Arc::clone(&self.latest);
        let wait = frame_wait_timeout();
        tokio::task::spawn_blocking(move || {
            let (lock, cvar) = &*slot;
            let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
            let (mut guard, _timeout) = cvar
                .wait_timeout_while(guard, wait, |f| f.is_none())
                .unwrap_or_else(|p| p.into_inner());
            guard
                .take()
                .ok_or_else(|| AgentShellError::Timeout("no PipeWire frame arrived in time".into()))
        })
        .await
        .map_err(|e| AgentShellError::Other(Box::new(e)))?
    }

    /// portal session 对象路径（PortalSessionManager 登记用）。
    pub fn session_path(&self) -> &ObjectPath<'static> {
        &self.session_path
    }

    /// 关闭 portal session（`org.freedesktop.portal.Session.Close`；幂等）。
    pub async fn close_session(&self) -> Result<()> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        close_portal_session(&self.conn, self.session_path.as_str()).await?;
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// 在绝对 `deadline` 前完成显式 Close 与 worker 退出。
    ///
    /// 两项清理并发进行并共享同一个截止；截止用尽即返回，由调用方降级。
    /// 之后的 Drop 只做无阻塞收尾，不会再次等待或补发。
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) {
        use std::sync::atomic::Ordering;

        if self.shutdown_handled.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.shutdown.send(());

        let wait_worker = async {
            let receiver = self
                .worker_done
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            let Some(receiver) = receiver else {
                return;
            };
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if budget.is_zero() {
                return;
            }
            let finished = tokio::time::timeout(
                budget,
                tokio::task::spawn_blocking(move || receiver.recv_timeout(budget).is_ok()),
            )
            .await;
            if matches!(finished, Ok(Ok(true))) {
                self.worker_finished.store(true, Ordering::SeqCst);
            }
        };

        let close_portal = async {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if budget.is_zero() {
                return;
            }
            let closed = tokio::time::timeout(
                budget,
                close_portal_session(&self.conn, self.session_path.as_str()),
            )
            .await;
            if matches!(closed, Ok(Ok(()))) {
                self.closed.store(true, Ordering::SeqCst);
            }
        };

        tokio::join!(wait_worker, close_portal);
    }
}

fn frame_wait_timeout() -> Duration {
    #[cfg(feature = "portal-test-hooks")]
    if let Some(timeout) = crate::portal_common::test_hooks::frame_wait_timeout() {
        return timeout;
    }
    SCREENCAST_TIMEOUT
}

impl Drop for ScreenCastCapture {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        if !self.shutdown_handled.swap(true, Ordering::SeqCst) {
            let _ = self.shutdown.send(());
            let deadline = tokio::time::Instant::now() + SCREENCAST_CLEANUP_BUDGET;
            if !self.closed.load(Ordering::SeqCst) {
                spawn_portal_cleanup_until(
                    self.conn.clone(),
                    None,
                    Some(self.session_path.clone()),
                    deadline,
                );
            }
            let receiver = self
                .worker_done
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            if let Some(receiver) = receiver {
                let budget = deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(SCREENCAST_WORKER_SHUTDOWN_BUDGET);
                if receiver.recv_timeout(budget).is_ok() {
                    self.worker_finished.store(true, Ordering::SeqCst);
                }
            }
        }

        if self.worker_finished.load(Ordering::SeqCst) {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

/// 在绝对截止前等待一个建立步骤；无截止时退回单步默认超时。
async fn wait_within<T>(
    deadline: Option<tokio::time::Instant>,
    label: &str,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let budget = match deadline {
        Some(d) => d
            .saturating_duration_since(tokio::time::Instant::now())
            .min(SCREENCAST_TIMEOUT),
        None => SCREENCAST_TIMEOUT,
    };
    tokio::time::timeout(budget, fut).await.map_err(|_| {
        AgentShellError::Timeout(format!("portal {label} exceeded the ScreenCast deadline"))
    })?
}

/// 从 CreateSession 成功起持有的 portal 资源清理守卫。
///
/// 记录已创建的会话与最后一个未完成的 Request。显式失败/超时路径调用
/// [`Self::release`] 在固定预算内发出 `Request.Close` 与 `Session.Close`；
/// 建立 future 被硬取消时由 Drop 在总线 executor 上补发一次有界清理。
struct PortalSessionCleanup {
    conn: zbus::Connection,
    session_path: Option<ObjectPath<'static>>,
    pending_request: Option<String>,
}

impl PortalSessionCleanup {
    fn new(conn: zbus::Connection) -> Self {
        Self {
            conn,
            session_path: None,
            pending_request: None,
        }
    }

    fn track_request(&mut self, path: String) {
        self.pending_request = Some(path);
    }

    fn clear_request(&mut self) {
        self.pending_request = None;
    }

    fn track_session(&mut self, path: ObjectPath<'static>) {
        self.session_path = Some(path);
    }

    fn take_session(&mut self) -> ObjectPath<'static> {
        self.session_path
            .take()
            .expect("session tracked after CreateSession")
    }

    /// 在 `budget` 内先取消未完成 Request、再关闭已创建的 Session。
    async fn release(&mut self, budget: Duration) {
        let step = budget.min(SCREENCAST_PORTAL_CLOSE_BUDGET) / 2;
        if let Some(request) = self.pending_request.take() {
            let _ = tokio::time::timeout(step, close_portal_request(&self.conn, &request)).await;
        }
        if let Some(session) = self.session_path.take() {
            let _ = tokio::time::timeout(step, close_portal_session(&self.conn, session.as_str()))
                .await;
        }
    }
}

impl Drop for PortalSessionCleanup {
    fn drop(&mut self) {
        let request = self.pending_request.take();
        let session = self.session_path.take();
        if request.is_none() && session.is_none() {
            return;
        }
        spawn_portal_cleanup(self.conn.clone(), request, session);
    }
}

/// 在当前 Tokio 运行时上补发一次有界清理；无运行时时放弃（进程退出场景）。
fn spawn_portal_cleanup(
    conn: zbus::Connection,
    request: Option<String>,
    session: Option<ObjectPath<'static>>,
) {
    let deadline = tokio::time::Instant::now() + SCREENCAST_CLEANUP_BUDGET;
    spawn_portal_cleanup_until(conn, request, session, deadline);
}

/// 在同一个绝对截止前补发请求取消与会话关闭。
fn spawn_portal_cleanup_until(
    conn: zbus::Connection,
    request: Option<String>,
    session: Option<ObjectPath<'static>>,
    deadline: tokio::time::Instant,
) {
    if request.is_none() && session.is_none() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("no tokio runtime for ScreenCast portal cleanup; skipping");
        return;
    };
    handle.spawn(async move {
        if let Some(request) = request {
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if !budget.is_zero() {
                let _ = tokio::time::timeout(budget, close_portal_request(&conn, &request)).await;
            }
        }
        if let Some(session) = session {
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if !budget.is_zero() {
                let _ = tokio::time::timeout(budget, close_portal_session(&conn, session.as_str()))
                    .await;
            }
        }
    });
}

/// 取消未完成的 portal Request（`org.freedesktop.portal.Request.Close`）。
async fn close_portal_request(conn: &zbus::Connection, path: &str) -> Result<()> {
    conn.call_method(
        Some(PORTAL_SERVICE),
        path,
        Some("org.freedesktop.portal.Request"),
        "Close",
        &(),
    )
    .await
    .map_err(|e| AgentShellError::DBus(format!("Close request {path}: {e}")))?;
    Ok(())
}

/// 关闭已创建的 portal 会话（`org.freedesktop.portal.Session.Close`）。
async fn close_portal_session(conn: &zbus::Connection, path: &str) -> Result<()> {
    conn.call_method(
        Some(PORTAL_SERVICE),
        path,
        Some("org.freedesktop.portal.Session"),
        "Close",
        &(),
    )
    .await
    .map_err(|e| AgentShellError::DBus(format!("Close session {path}: {e}")))?;
    Ok(())
}

/// 从 Start 的 Response 结果字典提取第一个流的 node id。
///
/// `streams` 类型为 `a(ua{sv})`。
fn extract_node_id(results: &HashMap<String, zvariant::OwnedValue>) -> Option<u32> {
    let streams = results.get("streams")?;
    let arr = streams.downcast_ref::<zvariant::Array>().ok()?;
    let first = arr.first()?;
    let structure = first.downcast_ref::<zvariant::Structure>().ok()?;
    let fields = structure.fields();
    let node = fields.first()?.downcast_ref::<u32>().ok()?;
    Some(node)
}

/// PipeWire 端：连 fd、订阅 node、把最新帧写进共享槽位。
///
/// 在专用线程上运行独立 main loop（pipewire crate 的回调是同步的，
/// 不与 tokio 运行时耦合）；`shutdown_rx` 收到消息即 quit。
fn run_pipewire_node(
    fd: std::os::fd::OwnedFd,
    node_id: u32,
    latest: std::sync::Arc<(std::sync::Mutex<Option<Frame>>, std::sync::Condvar)>,
    shutdown_rx: std::sync::mpsc::Receiver<()>,
) {
    use pipewire as pw;
    use pipewire::spa;
    use pipewire::spa::param::format_utils;
    use pipewire::spa::pod::Pod;
    pw::init();
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else {
        tracing::error!("pipewire: MainLoop creation failed");
        return;
    };
    let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else {
        tracing::error!("pipewire: Context creation failed");
        return;
    };
    // connect_fd：通过 portal 下发的 fd 连接远端 graph（非本地默认实例）。
    let core = match context.connect_fd(fd, None) {
        Ok(core) => core,
        Err(e) => {
            tracing::error!("pipewire: connect_fd failed: {e}");
            return;
        }
    };

    struct UserData {
        format: spa::param::video::VideoInfoRaw,
        latest: std::sync::Arc<(std::sync::Mutex<Option<Frame>>, std::sync::Condvar)>,
        negotiated: bool,
        /// 协商出的像素格式；None = 未协商或协商出不受支持格式。
        pixel_format: Option<PixelFormat>,
    }

    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };
    let Ok(stream) = pw::stream::StreamBox::new(&core, "agent-shell-screencast", props) else {
        tracing::error!("pipewire: Stream creation failed");
        return;
    };

    let data = UserData {
        format: spa::param::video::VideoInfoRaw::new(),
        latest,
        negotiated: false,
        pixel_format: None,
    };

    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(|_stream, user_data, id, param| {
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            if format_utils::parse_format(param).is_err() {
                return;
            }
            if user_data.format.parse(param).is_ok() {
                user_data.pixel_format = match user_data.format.format() {
                    pw::spa::param::video::VideoFormat::BGRA => Some(PixelFormat::Bgra),
                    pw::spa::param::video::VideoFormat::BGRx => Some(PixelFormat::Bgrx),
                    pw::spa::param::video::VideoFormat::RGBA => Some(PixelFormat::Rgba),
                    other => {
                        tracing::warn!("pipewire: unexpected negotiated format {other:?}");
                        None
                    }
                };
                user_data.negotiated = true;
            }
        })
        .process(|stream, user_data| {
            // 未协商出受支持的格式时不产帧（消费方按 Frame.format 解析）。
            let Some(pixel_format) = user_data.pixel_format else {
                return;
            };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data0 = &mut datas[0];
            let size = data0.chunk().size() as usize;
            let Some(bytes) = data0.data() else { return };
            if size == 0 || size > bytes.len() {
                return;
            }
            let rect = user_data.format.size();
            let frame = Frame {
                data: bytes[..size].to_vec(),
                width: rect.width.max(1),
                height: rect.height.max(1),
                stride: (size / rect.height.max(1) as usize),
                format: pixel_format,
            };
            let (lock, cvar) = &*user_data.latest;
            if let Ok(mut guard) = lock.lock() {
                *guard = Some(frame);
                cvar.notify_all();
            }
        })
        .register();
    let _listener = match listener {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("pipewire: listener register failed: {e}");
            return;
        }
    };

    // 枚举常见 BGRA/BGRx/RGBA 布局；实际格式由 portal 端协商，
    // 协商结果在 param_changed 回调里解析。
    use pw::spa::param::format::FormatProperties;
    use pw::spa::param::video::VideoFormat;
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRA,
            VideoFormat::BGRA,
            VideoFormat::BGRx,
            VideoFormat::RGBA
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map(|v| v.0.into_inner())
    .unwrap_or_default();
    let Some(pod) = Pod::from_bytes(&values) else {
        tracing::error!("pipewire: pod serialization failed");
        return;
    };

    if let Err(e) = stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut [pod],
    ) {
        tracing::error!("pipewire: stream connect failed: {e}");
        return;
    }

    // mainloop.run() 永久阻塞无法外部退出；改为 iterate 轮询驱动，
    // 每 50ms 处理一轮事件并检查 shutdown channel。
    loop {
        if shutdown_rx.recv_timeout(Duration::from_millis(50)).is_ok() {
            break;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_mode_as_u32_matches_portal_spec() {
        // xdg-desktop-portal ScreenCast §SelectSources persist_mode：
        // 0=none, 1=while_running, 2=until_revoked。
        assert_eq!(PersistMode::None.as_u32(), 0);
        assert_eq!(PersistMode::WhileRunning.as_u32(), 1);
        assert_eq!(PersistMode::UntilRevoked.as_u32(), 2);
    }

    #[test]
    fn options_default_has_no_persistence() {
        // 默认选项不设 persist_mode、不传 restore_token——弹窗路径。
        let opts = ScreenCastOptions::default();
        assert!(opts.restore_token.is_none());
        assert!(opts.persist_mode.is_none());
    }

    #[test]
    fn options_with_token_and_persist_mode() {
        // 完整无交互路径：restore_token + persist_mode=UntilRevoked。
        let opts = ScreenCastOptions {
            restore_token: Some("abc123".into()),
            persist_mode: Some(PersistMode::UntilRevoked),
        };
        assert_eq!(opts.restore_token.as_deref(), Some("abc123"));
        assert_eq!(opts.persist_mode, Some(PersistMode::UntilRevoked));
    }

    #[test]
    fn handle_tokens_are_unique_per_step() {
        // 三步 portal 调用各需不同 handle_token——重复 token 导致
        // Request 路径冲突，后端仅响应首步（10s 挂起根因）。
        let pid = std::process::id();
        let create = format!("agent_shell_sc_create_{pid}");
        let select = format!("agent_shell_sc_select_{pid}");
        let start = format!("agent_shell_sc_start_{pid}");
        let mut tokens = vec![create.as_str(), select.as_str(), start.as_str()];
        tokens.sort();
        tokens.dedup();
        assert_eq!(tokens.len(), 3, "handle_token must be unique per step");
    }

    #[test]
    fn cleanup_budget_covers_worker_shutdown_and_portal_close() {
        // Drop 的同步 worker 等待与随后的 Request/Session Close 共用 200ms。
        assert_eq!(
            SCREENCAST_WORKER_SHUTDOWN_BUDGET + SCREENCAST_PORTAL_CLOSE_BUDGET,
            SCREENCAST_CLEANUP_BUDGET
        );
    }
}
