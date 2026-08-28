//! JSON-RPC 方法分派（§22.2 D1：CLI 所有操作经 daemon 统一路由）。
//!
//! 每个方法对应 rpc crate 的一个 `method::` 常量；doctor 的组件行在此
//! 异步收集（合成器 doctor_lines + a11y 探测），CLI 只做渲染。

use crate::state::Daemon;
use agent_shell_core::types::WindowInfo;
use agent_shell_rpc::{
    method, A11yStatusResult, CaptureParams, DoctorResult, InfoResult, InputParams, Request,
    Response, RpcErrorCode, WindowOpKind,
};
use serde_json::{json, Value};

/// 单请求处理入口。返回完整 Response（永不 panic——所有错误走 RPC error）。
pub async fn dispatch(daemon: &mut Daemon, req: &Request) -> Response {
    let result = match req.method.as_str() {
        method::DOCTOR => doctor(daemon).await,
        method::INFO => info(daemon).await,
        method::WINDOWS_LIST => windows_list(daemon, req).await,
        method::WINDOW_INFO => window_info(daemon, req).await,
        method::WINDOW_OP => window_op(daemon, req).await,
        method::WORKSPACES_LIST => workspaces_list(daemon).await,
        method::WORKSPACE_SWITCH => workspace_switch(daemon, req).await,
        method::INPUT_SEND => blocking_input_send(req).await,
        method::SCREENSHOT_CAPTURE => screenshot_capture(daemon, req).await,
        method::A11Y_STATUS => a11y_status().await,
        // ── 事件（§22.5 D4）──
        method::EVENTS_SUBSCRIBE => events_subscribe(daemon, req).await,
        method::EVENTS_UNSUBSCRIBE => events_unsubscribe(daemon, req).await,
        method::EVENTS_REPLAY => events_replay(daemon).await,
        // ── daemon 管理（§22.2）──
        method::DAEMON_STATUS => daemon_status(daemon).await,
        method::DAEMON_SESSIONS => daemon_sessions(daemon).await,
        // ── IME（§22.8 D7）──
        method::IME_ENGINE_LIST => ime_engine_list(daemon).await,
        method::IME_ENGINE_SET => ime_engine_set(daemon, req).await,
        method::IME_ENGINE_CURRENT => ime_engine_current(daemon).await,
        method::IME_TYPE => ime_type(daemon, req).await,
        // ── 扩展系统服务（§21.35 stub）──
        method::SECURITY_STATUS => stub_ok("security.status"),
        method::SECURITY_GRANT => stub_ok("security.grant"),
        method::SECURITY_REVOKE => stub_ok("security.revoke"),
        method::SECURITY_AUDIT => stub_ok("security.audit"),
        method::BRIGHTNESS_GET => stub_ok("brightness.get"),
        method::BRIGHTNESS_SET => stub_ok("brightness.set"),
        method::FILE_PICK => stub_ok("file.pick"),
        method::FILE_TRASH => stub_ok("file.trash"),
        method::FILE_OPEN_DIR => stub_ok("file.open_directory"),
        method::MIME_GET => stub_ok("mime.get"),
        method::MIME_SET => stub_ok("mime.set"),
        method::MIME_DEFAULT_BROWSER => stub_ok("mime.default_browser"),
        method::BLUETOOTH_SCAN => stub_ok("bluetooth.scan"),
        method::BLUETOOTH_CONNECT => stub_ok("bluetooth.connect"),
        method::BLUETOOTH_DISCONNECT => stub_ok("bluetooth.disconnect"),
        method::BLUETOOTH_LIST => stub_ok("bluetooth.list"),
        method::FLATPAK_LIST => stub_ok("flatpak.list"),
        method::FLATPAK_INSTALL => stub_ok("flatpak.install"),
        method::SOFTWARE_UPDATES => stub_ok("software.updates"),
        method::TOUCHPAD_STATUS => stub_ok("touchpad.status"),
        method::TOUCHPAD_SET => stub_ok("touchpad.set"),
        method::KBD_LAYOUT_LIST => stub_ok("kbd.layout.list"),
        method::KBD_LAYOUT_SET => stub_ok("kbd.layout.set"),
        method::SECRET_SET => stub_ok("secret.set"),
        method::SECRET_GET => stub_ok("secret.get"),
        method::SHORTCUT_BIND => stub_ok("shortcut.bind"),
        method::SHORTCUT_TRIGGER => stub_ok("shortcut.trigger"),
        method::TIMER_LIST => stub_ok("timer.list"),
        // ── rootd 特权代理（§23.4）──
        method::SERVICE_CONTROL => service_control(daemon, req).await,
        method::SYSTEM_LOG_VIEW => system_log_view(daemon, req).await,
        method::ROOTD_HELLO => rootd_hello(daemon).await,
        other => {
            return Response::err(
                req.id,
                RpcErrorCode::MethodNotFound,
                format!("unknown method: {other}"),
            )
        }
    };
    match result {
        Ok(v) => Response::ok(req.id, v),
        Err((code, msg)) => Response::err(req.id, code, msg),
    }
}

type RpcResult = Result<Value, (RpcErrorCode, String)>;

fn params_of(req: &Request) -> Result<&Value, (RpcErrorCode, String)> {
    req.params
        .as_ref()
        .ok_or((RpcErrorCode::InvalidParams, "missing params".to_string()))
}

// ───────────────────────── doctor / info ─────────────────────────

async fn doctor(d: &mut Daemon) -> RpcResult {
    let mut lines = Vec::new();
    // 1. DE 检测（core 证据化报告）。
    lines.push(format!(
        "✓ DE 检测        : {}",
        agent_shell_core::de_detection::detect_report_for_doctor()
    ));
    // 2. 合成器通道（daemon 持久化连接的真实状态）。
    match d.compositor_health() {
        Ok(_) => {
            lines.push("✓ 合成器         : kwin-compositor".into());
            let comp_lines = compositor_doctor_lines(d).await;
            lines.extend(comp_lines);
        }
        Err(()) => lines.push("✗ 合成器         : unavailable in this session".into()),
    }
    // 3. AT-SPI Registry 可达性（busctl，与单进程版同口径）。
    lines.push(crate::a11y::atspi_line());
    // 4. capture 组件（三级降级链状态，§13）。
    lines.push(agent_shell_capture::doctor_line(d.capture.as_ref()).await);
    let healthy = !lines.iter().any(|l| l.starts_with('✗'));
    let r = DoctorResult { lines, healthy };
    Ok(serde_json::to_value(r).expect("DoctorResult serializable"))
}

async fn compositor_doctor_lines(d: &Daemon) -> Vec<String> {
    if d.has_compositor() {
        // KWinCompositor::doctor_lines_async 需要实例引用；经 state 层暴露。
        // 异步版本先补齐 /Scripting 懒探测，再渲染桥接行（TSI-2486）。
        d.doctor_lines_async().await
    } else {
        Vec::new()
    }
}

async fn info(d: &Daemon) -> RpcResult {
    let detection = agent_shell_core::de_detection::detect_report_for_doctor();
    let mut capabilities = if d.has_compositor() {
        vec![
            ("window_management".into(), true),
            ("workspace_management".into(), true),
            ("monitor_layout".into(), true),
            ("window_events".into(), false), // T3b
            ("native_input".into(), false),  // T2a portal 会话
            ("virtual_desktops".into(), true),
            ("effects_control".into(), false),
        ]
    } else {
        Vec::new()
    };
    // capture 组件独立于 compositor——纯 X11 会话仍可截图（审查项 #3）。
    capabilities.push(("native_capture".into(), d.capture.is_some()));
    let r = InfoResult {
        detection,
        capabilities,
    };
    Ok(serde_json::to_value(r).expect("InfoResult serializable"))
}

// ───────────────────────── windows ─────────────────────────

async fn windows_list(d: &mut Daemon, req: &Request) -> RpcResult {
    let filter = req
        .params
        .as_ref()
        .and_then(|p| p.get("filter"))
        .and_then(|v| v.as_str());
    let (wins, from_cache) = d.list_windows().await?;
    let filtered: Vec<&WindowInfo> = match filter {
        Some(f) => wins
            .iter()
            .filter(|w| w.app_id == f || w.title.contains(f))
            .collect(),
        None => wins.iter().collect(),
    };
    let items: Vec<Value> = filtered
        .iter()
        .map(|w| {
            json!({
                "native_id": w.id.native_id,
                "title": w.title,
                "app_id": w.app_id,
                "pid": w.pid,
                "x": w.geometry.x,
                "y": w.geometry.y,
                "width": w.geometry.width,
                "height": w.geometry.height,
                "workspace": w.workspace_id.as_ref().map(|ws| &ws.native_id),
            })
        })
        .collect();
    Ok(json!({ "windows": items, "from_cache": from_cache }))
}

async fn window_info(d: &mut Daemon, req: &Request) -> RpcResult {
    let native_id = params_of(req)?
        .get("target")
        .and_then(Value::as_str)
        .ok_or((RpcErrorCode::InvalidParams, "params.target required".into()))?;
    let w = d.window_info(native_id).await?;
    Ok(serde_json::to_value(&w).expect("WindowInfo serializable"))
}

async fn window_op(d: &mut Daemon, req: &Request) -> RpcResult {
    let p: WindowOpParamsDe = serde_json::from_value(params_of(req)?.clone())
        .map_err(|e| (RpcErrorCode::InvalidParams, format!("bad params: {e}")))?;
    let (x, y, w, h) = (
        p.x.unwrap_or(0),
        p.y.unwrap_or(0),
        p.width.unwrap_or(0),
        p.height.unwrap_or(0),
    );
    d.window_op(&p.target, p.op, x, y, w, h).await?;
    Ok(json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
struct WindowOpParamsDe {
    op: WindowOpKind,
    target: String,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
}

async fn workspaces_list(d: &Daemon) -> RpcResult {
    let list = d.list_workspaces().await?;
    Ok(json!({ "workspaces": list }))
}

async fn workspace_switch(d: &Daemon, req: &Request) -> RpcResult {
    let ws = params_of(req)?
        .get("id")
        .and_then(Value::as_str)
        .ok_or((RpcErrorCode::InvalidParams, "params.id required".into()))?;
    d.activate_workspace(ws).await?;
    Ok(json!({ "ok": true }))
}

// ───────────────────────── input ─────────────────────────

/// 输入注入：daemon 经其持久化 X11/XTest 通道执行。
///
/// XTest 是同步 I/O——经 `spawn_blocking` 执行，不阻塞 tokio worker
/// 线程（与 a11y_status 同口径）。
async fn blocking_input_send(req: &Request) -> RpcResult {
    let p: InputParams = serde_json::from_value(params_of(req)?.clone())
        .map_err(|e| (RpcErrorCode::InvalidParams, format!("bad params: {e}")))?;
    tokio::task::spawn_blocking(move || crate::input::execute(p.kind, &p.payload))
        .await
        .map_err(|e| (RpcErrorCode::InternalError, e.to_string()))??;
    Ok(json!({ "ok": true }))
}

// ───────────────────────── screenshot ─────────────────────────

/// 截图捕获：经 capture 模块三级降级链（portal ScreenCast → Screenshot →
/// X11）；窗口直捕仅 X11。链路含 portal 弹窗授权等待——async 直调不占
/// `spawn_blocking`（组件内部已对同步段做 spawn_blocking）。
async fn screenshot_capture(daemon: &mut Daemon, req: &Request) -> RpcResult {
    let p: CaptureParams = serde_json::from_value(params_of(req)?.clone())
        .map_err(|e| (RpcErrorCode::InvalidParams, format!("bad params: {e}")))?;
    let capture = daemon.capture.as_ref().ok_or_else(|| {
        (
            RpcErrorCode::BackendUnavailable,
            "capture unavailable (no portal backend and no DISPLAY)".into(),
        )
    })?;
    let r = crate::capture::capture_to_file(capture, p.window.as_deref(), p.area, &p.output_path)
        .await?;
    Ok(serde_json::to_value(r).expect("CaptureResult serializable"))
}

// ───────────────────────── a11y ─────────────────────────

async fn a11y_status() -> RpcResult {
    let line = tokio::task::spawn_blocking(crate::a11y::atspi_line)
        .await
        .map_err(|e| (RpcErrorCode::InternalError, e.to_string()))?;
    let available = line.starts_with('✓');
    let r = A11yStatusResult {
        available,
        detail: line,
    };
    Ok(serde_json::to_value(r).expect("A11yStatusResult serializable"))
}

// ───────────────────────── 事件 / daemon / IME ─────────────────────────

/// stub_ok — 扩展系统服务的占位响应（§21.35，待 Phase 3 接线）。
fn stub_ok(name: &str) -> RpcResult {
    Ok(json!({"status": "not_implemented", "service": name}))
}

/// 事件订阅——v1 不实现推送，显式返回 not_implemented（§22.5 D4 待 Phase 2 接线）。
async fn events_subscribe(_d: &mut Daemon, _req: &Request) -> RpcResult {
    Err((
        RpcErrorCode::Denied,
        "events.subscribe not implemented in v1 (event push pending Phase 2 wiring)".into(),
    ))
}

/// 事件取消订阅——v1 不实现推送，显式返回 not_implemented。
async fn events_unsubscribe(_d: &mut Daemon, _req: &Request) -> RpcResult {
    Err((
        RpcErrorCode::Denied,
        "events.unsubscribe not implemented in v1 (event push pending Phase 2 wiring)".into(),
    ))
}

async fn events_replay(d: &mut Daemon) -> RpcResult {
    let events = d.ring_buffer.replay();
    let count = events.len();
    Ok(json!({"events": events, "count": count}))
}

async fn daemon_status(d: &mut Daemon) -> RpcResult {
    use agent_shell_rpc::DaemonStatusResult;
    let r = DaemonStatusResult {
        running: true,
        windows_cached: d.cache_len(),
        subscribers: d.subscriber_count(),
        ime_engine: d.ime_session.current_engine(),
    };
    Ok(serde_json::to_value(r).expect("DaemonStatusResult serializable"))
}

async fn daemon_sessions(d: &mut Daemon) -> RpcResult {
    let sessions = d.portal_sessions.list_sessions();
    Ok(json!({"sessions": sessions}))
}

async fn ime_engine_list(d: &mut Daemon) -> RpcResult {
    Ok(json!({"engines": d.ime_session.list_engines()}))
}

async fn ime_engine_set(d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let engine = params
        .get("engine")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing engine".into()))?;
    d.ime_session.set_engine(engine);
    Ok(json!({"set": engine}))
}

async fn ime_engine_current(d: &mut Daemon) -> RpcResult {
    Ok(json!({"engine": d.ime_session.current_engine()}))
}

async fn ime_type(d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let text = params
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing text".into()))?;
    let result = d.ime_session.type_text(text);
    Ok(serde_json::to_value(result).expect("ImeTypeResult serializable"))
}

// ───────────────────────── rootd 特权代理（§23.4） ─────────────────────────

/// rootd 版本对账（§23.4.3：daemon 与 rootd 需匹配安全模型版本）。
///
/// rootd 未安装时返回 "rootd not installed" 并降级（§23.2）。
/// rootd 已安装但版本不匹配时拒绝服务。
async fn rootd_hello(_d: &mut Daemon) -> RpcResult {
    match crate::rootd_client::connect().await {
        Some(proxy) => {
            let version = proxy
                .hello()
                .await
                .map_err(|e| (RpcErrorCode::BackendError, format!("rootd Hello: {e}")))?;
            // 解析 rootd 返回的 security_model 版本并与 daemon 自身比较
            let parsed: Value = serde_json::from_str(&version).map_err(|e| {
                (
                    RpcErrorCode::BackendError,
                    format!("rootd Hello parse: {e}"),
                )
            })?;
            let rootd_sm = parsed
                .get("security_model")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            // daemon 自身安全模型版本（与 rootd lib.rs SECURITY_MODEL_VERSION 一致）
            const DAEMON_SECURITY_MODEL: u64 = 1;
            if rootd_sm != DAEMON_SECURITY_MODEL {
                return Ok(json!({
                    "rootd_installed": true,
                    "version_mismatch": true,
                    "daemon_security_model": DAEMON_SECURITY_MODEL,
                    "rootd_security_model": rootd_sm,
                    "error": "security model version mismatch — refusing service"
                }));
            }
            Ok(json!({
                "rootd_installed": true,
                "version_mismatch": false,
                "version": version
            }))
        }
        None => Ok(json!({ "rootd_installed": false, "error": "rootd not installed" })),
    }
}

/// 启停系统服务（rootd ServiceStart/Stop/Restart）。
///
/// 参数：{ "action": "start|stop|restart", "unit": "nginx.service" }
/// rootd 未安装时返回降级错误。
async fn service_control(_d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let action = params
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing action".into()))?;
    let unit = params
        .get("unit")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing unit".into()))?;

    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;

    match action {
        "start" => proxy
            .service_start(unit)
            .await
            .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?,
        "stop" => proxy
            .service_stop(unit)
            .await
            .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?,
        "restart" => proxy
            .service_restart(unit)
            .await
            .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?,
        other => {
            return Err((
                RpcErrorCode::InvalidParams,
                format!("unknown action: {other} (expected start/stop/restart)"),
            ))
        }
    }
    Ok(json!({ "accepted": true, "action": action, "unit": unit }))
}

/// 查看系统日志（rootd JournalQuery）。
///
/// 参数：{ "filter": { "unit": "nginx", "priority": "err" } }
/// rootd 未安装时返回降级错误。
async fn system_log_view(_d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let filter = params
        .get("filter")
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_else(|| "{}".to_string());

    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — system journal unavailable".into(),
    ))?;

    let result = proxy
        .journal_query(&filter)
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    Ok(json!({ "result": result }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_rpc::method;
    use std::time::Duration;

    fn req(method_name: &str, params: Option<Value>) -> Request {
        match params {
            Some(p) => Request::new(42, method_name, p),
            None => Request::without_params(42, method_name),
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req("bogus.method", None)).await;
        let err = resp.error.expect("error");
        assert_eq!(err.code, RpcErrorCode::MethodNotFound as i32);
        assert!(err.message.contains("bogus.method"));
    }

    #[tokio::test]
    async fn doctor_always_responds_even_without_compositor() {
        // daemon 契约：装配失败不 panic，doctor 逐项如实报告。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::DOCTOR, None)).await;
        let v = resp.result.expect("ok");
        let r: DoctorResult = serde_json::from_value(v).expect("DoctorResult");
        assert!(!r.lines.is_empty());
        assert!(r.lines[0].starts_with("✓ DE 检测"));
    }

    #[tokio::test]
    async fn missing_params_is_invalid_params_not_panic() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        for m in [method::WINDOW_INFO, method::WINDOW_OP] {
            let resp = dispatch(&mut d, &req(m, None)).await;
            assert_eq!(
                resp.error.expect("error").code,
                RpcErrorCode::InvalidParams as i32,
                "{m}"
            );
        }
    }

    #[tokio::test]
    async fn input_bad_payload_is_invalid_params() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(
            &mut d,
            &req(
                method::INPUT_SEND,
                Some(json!({ "kind": "key", "payload": {} })),
            ),
        )
        .await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::InvalidParams as i32
        );
    }

    #[tokio::test]
    async fn screenshot_negative_area_rejected_gracefully() {
        // 审查项 #1 回归锚定：负坐标走 RPC error 而非 panic。
        // （无 DISPLAY 环境 → BackendUnavailable 先于裁剪；两条路径都必须是 error。）
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let params = json!({
            "area": [-5, -3, 100, 100],
            "output_path": "/tmp/never-written.ppm",
        });
        let resp = dispatch(&mut d, &req(method::SCREENSHOT_CAPTURE, Some(params))).await;
        assert!(resp.error.is_some(), "must not succeed or panic without X");
    }

    #[tokio::test]
    async fn events_subscribe_returns_not_implemented() {
        // v1 不实现事件推送——subscribe 返回 Denied（not_implemented）。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::EVENTS_SUBSCRIBE, None)).await;
        assert!(resp.error.is_some(), "subscribe must return error");
        assert_eq!(resp.error.unwrap().code, RpcErrorCode::Denied as i32);
    }

    #[tokio::test]
    async fn events_unsubscribe_returns_not_implemented() {
        // v1 不实现事件推送——unsubscribe 返回 Denied（not_implemented）。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(
            &mut d,
            &req(
                method::EVENTS_UNSUBSCRIBE,
                Some(json!({"subscriber_id": "x"})),
            ),
        )
        .await;
        assert!(resp.error.is_some(), "unsubscribe must return error");
        assert_eq!(resp.error.unwrap().code, RpcErrorCode::Denied as i32);
    }

    #[tokio::test]
    async fn events_replay_returns_ring_buffer_contents() {
        // 审查项 #2：ring_buffer 非空时 events_replay 返回事件。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.ring_buffer
            .push(json!({"type": "window_list", "native_id": "x"}));
        let resp = dispatch(&mut d, &req(method::EVENTS_REPLAY, None)).await;
        let v = resp.result.expect("replay ok");
        let count = v.get("count").and_then(|v| v.as_u64()).expect("count");
        assert_eq!(count, 1);
    }
}
