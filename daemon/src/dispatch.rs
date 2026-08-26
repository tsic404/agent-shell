//! JSON-RPC 方法分派（§22.2 D1：CLI 所有操作经 daemon 统一路由）。
//!
//! 每个方法对应 rpc crate 的一个 `method::` 常量；doctor 的组件行在此
//! 异步收集（合成器 doctor_lines + a11y 探测），CLI 只做渲染。

use crate::state::Daemon;
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
        method::WINDOWS_LIST => windows_list(daemon).await,
        method::WINDOW_INFO => window_info(daemon, req).await,
        method::WINDOW_OP => window_op(daemon, req).await,
        method::WORKSPACES_LIST => workspaces_list(daemon).await,
        method::WORKSPACE_SWITCH => workspace_switch(daemon, req).await,
        method::INPUT_SEND => blocking_input_send(req).await,
        method::SCREENSHOT_CAPTURE => screenshot_capture(daemon, req).await,
        method::A11Y_STATUS => a11y_status().await,
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
        // KWinCompositor::doctor_lines 需要实例引用；经 state 层暴露。
        d.doctor_lines()
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

async fn windows_list(d: &mut Daemon) -> RpcResult {
    let (wins, from_cache) = d.list_windows().await?;
    let items: Vec<Value> = wins
        .iter()
        .map(|w| {
            json!({
                "native_id": w.id.native_id,
                "title": w.title,
                "app_id": w.app_id,
                "pid": w.pid,
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
}
