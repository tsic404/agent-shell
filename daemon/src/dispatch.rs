//! JSON-RPC 方法分派（§22.2 D1：CLI 所有操作经 daemon 统一路由）。
//!
//! 每个方法对应 rpc crate 的一个 `method::` 常量；doctor 的组件行在此
//! 异步收集（合成器 doctor_lines + a11y 探测），CLI 只做渲染。

use crate::state::Daemon;
use agent_shell_core::error::AgentShellError;
use agent_shell_core::security::{Operation, PermissionDecision, PermissionLevel};
use agent_shell_core::types::{SemanticTarget, WindowInfo};
use agent_shell_rpc::{
    method, A11yElementResult, A11yQueryResult, A11yStatusResult, CaptureParams, DoctorResult,
    InfoResult, InputParams, Request, Response, RpcErrorCode, WindowOpKind,
};
use event::EventFilter;
use serde_json::{json, Value};
use uuid::Uuid;

/// 单请求处理入口。返回完整 Response（永不 panic——所有错误走 RPC error）。
pub async fn dispatch(daemon: &mut Daemon, req: &Request) -> Response {
    // 统一权限 gate（审查项 #4）：在真实命令入口 match 之前判定。
    // `security.*` 是策略管理面（bootstrap 可达性）豁免普通级别 gate。
    // 威胁模型（审查项 F2）：daemon stdin/控制 socket 属用户会话（同 UID），
    // caller_id 由同 UID 进程经 env 注入、可伪造——按 caller_id 门禁不构成
    // 额外权限边界。故 grant/revoke 提权原语以 `"*"` 为管理面边界：仅本地
    // 调用方（未注入 agent id 的 CLI/MCP）可授权/撤销；具名 agent 必须经
    // `"*"`。未知方法返回 None 交下方 match 报 MethodNotFound。
    let management = security_operation_for(&req.method);
    if management {
        if let Some(reason) = check_management_permission(&daemon.caller_id) {
            // 管理面 caller 门禁拒绝与普通权限拒绝同审计语义：deny 必须落痕
            //（TSI-2513：越权尝试此前零记录）。
            if daemon.security.config.security.audit_log {
                daemon
                    .security
                    .audit
                    .log(&daemon.caller_id, &req.method, "deny", false);
            }
            return Response::err(req.id, RpcErrorCode::Denied, reason);
        }
    }
    if let Some(op) = operation_for(&req.method, &req.params) {
        let caller = daemon.caller_id.clone();
        match daemon.security.check_permission(&caller, &op) {
            PermissionDecision::Allow => {}
            PermissionDecision::Deny(reason) => {
                return Response::err(req.id, RpcErrorCode::Denied, reason);
            }
            PermissionDecision::Confirm(mode) => {
                return Response::err(
                    req.id,
                    RpcErrorCode::ConfirmationRequired,
                    format!("{op} ({mode})"),
                );
            }
        }
    }
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
        method::A11Y_QUERY => a11y_query(daemon, req).await,
        // ── 事件（§22.5 D4）──
        method::EVENTS_SUBSCRIBE => events_subscribe(daemon, req).await,
        method::EVENTS_UNSUBSCRIBE => events_unsubscribe(daemon, req).await,
        method::EVENTS_REPLAY => events_replay(daemon, req).await,
        // ── daemon 管理（§22.2）──
        method::DAEMON_STATUS => daemon_status(daemon).await,
        method::DAEMON_SESSIONS => daemon_sessions(daemon).await,
        // ── IME（§22.8 D7）──
        method::IME_ENGINE_LIST => ime_engine_list(daemon).await,
        method::IME_ENGINE_SET => ime_engine_set(daemon, req).await,
        method::IME_ENGINE_CURRENT => ime_engine_current(daemon).await,
        method::IME_TYPE => ime_type(daemon, req).await,
        // ── 安全边界（§22.7 D6）──
        method::SECURITY_STATUS => security_status(daemon).await,
        method::SECURITY_GRANT => security_grant(daemon, req).await,
        method::SECURITY_REVOKE => security_revoke(daemon, req).await,
        method::SECURITY_AUDIT => security_audit(daemon, req).await,
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
        method::PROCESS_KILL => process_kill(daemon, req).await,
        method::JOB_STATUS => job_status(daemon, req).await,
        method::PACKAGE_INSTALL => package_install(daemon, req).await,
        method::PACKAGE_REMOVE => package_remove(daemon, req).await,
        method::PACKAGE_UPDATE => package_update(daemon, req).await,
        method::PACKAGE_REFRESH => package_refresh(daemon).await,
        method::ROOTD_HELLO => rootd_hello(daemon).await,
        method::HOSTNAME_SET => hostname_set(daemon, req).await,
        method::MOUNT => mount(daemon, req).await,
        method::UNMOUNT => unmount(daemon, req).await,
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

/// 方法名 → 权限操作映射（统一 gate 用）。`security.*` 与未知方法返回 `None`。
fn operation_for(method_name: &str, params: &Option<Value>) -> Option<Operation> {
    use PermissionLevel::*;

    if method_name == method::WINDOW_OP {
        // windows.op 按 params.op 细分级别（close=L2，其余写操作=L1）。
        let kind = params
            .as_ref()
            .and_then(|p| p.get("op"))
            .and_then(|v| v.as_str());
        return Some(match kind {
            Some("close") => Operation::new("windows.close", L2),
            Some("focus") => Operation::new("windows.focus", L1),
            Some("move") => Operation::new("windows.move", L1),
            Some("resize") => Operation::new("windows.resize", L1),
            Some("minimize") => Operation::new("windows.minimize", L1),
            _ => return None, // 非法/缺省 op 交 handler 报 InvalidParams。
        });
    }

    const OPS: &[(&str, PermissionLevel)] = &[
        (method::DOCTOR, L0),
        (method::INFO, L0),
        (method::WINDOWS_LIST, L0),
        (method::WINDOW_INFO, L0),
        (method::WORKSPACES_LIST, L0),
        (method::WORKSPACE_SWITCH, L1),
        (method::INPUT_SEND, L1),
        (method::SCREENSHOT_CAPTURE, L2),
        (method::A11Y_STATUS, L0),
        (method::EVENTS_SUBSCRIBE, L0),
        (method::EVENTS_UNSUBSCRIBE, L0),
        (method::EVENTS_REPLAY, L0),
        (method::DAEMON_STATUS, L0),
        (method::DAEMON_SESSIONS, L0),
        (method::IME_ENGINE_LIST, L0),
        (method::IME_ENGINE_SET, L1),
        (method::IME_ENGINE_CURRENT, L0),
        (method::IME_TYPE, L1),
        (method::BRIGHTNESS_GET, L0),
        (method::BRIGHTNESS_SET, L1),
        (method::FILE_PICK, L0),
        (method::FILE_TRASH, L2),
        (method::FILE_OPEN_DIR, L0),
        (method::MIME_GET, L0),
        (method::MIME_SET, L1),
        (method::MIME_DEFAULT_BROWSER, L1),
        (method::BLUETOOTH_SCAN, L0),
        (method::BLUETOOTH_CONNECT, L1),
        (method::BLUETOOTH_DISCONNECT, L1),
        (method::BLUETOOTH_LIST, L0),
        (method::FLATPAK_LIST, L0),
        (method::FLATPAK_INSTALL, L2),
        (method::SOFTWARE_UPDATES, L0),
        (method::TOUCHPAD_STATUS, L0),
        (method::TOUCHPAD_SET, L1),
        (method::KBD_LAYOUT_LIST, L0),
        (method::KBD_LAYOUT_SET, L1),
        (method::SECRET_SET, L3),
        (method::SECRET_GET, L3),
        (method::SHORTCUT_BIND, L1),
        (method::SHORTCUT_TRIGGER, L1),
        (method::TIMER_LIST, L0),
        (method::TIMER_NEXT, L0),
        (method::SERVICE_CONTROL, L3),
        (method::SYSTEM_LOG_VIEW, L0),
        (method::PROCESS_KILL, L3),
        (method::JOB_STATUS, L0),
        (method::PACKAGE_INSTALL, L3),
        (method::PACKAGE_REMOVE, L3),
        (method::PACKAGE_UPDATE, L3),
        (method::PACKAGE_REFRESH, L3),
        (method::ROOTD_HELLO, L0),
        (method::HOSTNAME_SET, L3),
        (method::MOUNT, L4),
        (method::UNMOUNT, L4),
    ];
    OPS.iter()
        .find(|(m, _)| *m == method_name)
        .map(|(m, l)| Operation::new(m, *l))
}

/// `grant` / `revoke` 为提权原语，门禁策略与普通操作不同：两者按
/// §22.7「所有命令必经」落 caller 校验（F2）。`"*"` 是本地调用方
/// （未注入 `AGENT_SHELL_AGENT_ID` 的 CLI/MCP 子进程），放行；具名 agent
/// 不可自行授予白名单，必须经 `"*"`（本地用户/编排层）完成。其余
/// `security.*` 管理面（status/audit）只读，免 caller 校验。
fn security_operation_for(method_name: &str) -> bool {
    matches!(
        method_name,
        method::SECURITY_GRANT | method::SECURITY_REVOKE
    )
}

/// 管理面 caller 门禁：`"*"` 放行，其余拒绝。
fn check_management_permission(caller_id: &str) -> Option<String> {
    if caller_id == "*" {
        None
    } else {
        Some(format!(
            "security grant/revoke requires local caller (got {caller_id:?}); \
             set AGENT_SHELL_AGENT_ID=* for the local orchestrator"
        ))
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
            lines.push(format!("✓ 合成器         : {}", d.compositor_name()));
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
        // 异步版本先补齐 /Scripting 懒探测，再渲染桥接行（TSI-2486）；
        // DDE 会话经 DdeCompositor::doctor_lines_async 呈现 deepin-kwin 分支。
        d.doctor_lines_async().await
    } else {
        Vec::new()
    }
}

async fn info(d: &Daemon) -> RpcResult {
    let detection = agent_shell_core::de_detection::detect_report_for_doctor();
    // 能力位表逐字段来自合成器声明（TSI-2501/2486 反模式：不得硬编码）。
    // capture 组件独立于 compositor（纯 X11 会话仍可截图），native_capture
    // 由 capture 探针真值决定（与 TSI-2569 的 Treeland window_management:false 同源）。
    let caps = d.compositor_capabilities();
    let capabilities = vec![
        ("window_management".into(), caps.window_management),
        ("workspace_management".into(), caps.workspace_management),
        ("monitor_layout".into(), caps.monitor_layout),
        ("window_events".into(), caps.window_events),
        ("workspace_events".into(), caps.workspace_events),
        ("native_input".into(), caps.native_input),
        ("native_capture".into(), d.capture.is_some()),
        ("virtual_desktops".into(), caps.virtual_desktops),
        ("effects_control".into(), caps.effects_control),
    ];
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

/// 语义查询（§14.3）：AT-SPI 树按 (role, name) 定位元素，投影为协议载荷。
async fn a11y_query(d: &Daemon, req: &Request) -> RpcResult {
    // 参数校验先于后端可用性判定：无效请求恒返回 InvalidParams，
    // 与 AT-SPI 是否可达无关（不信任客户端输入）。`role`/`name` 键
    // 缺省或值为 `null` 均视为未提供；存在且非字符串 → InvalidParams。
    let params = params_of(req)?;
    let role = match params.get("role") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err((
                RpcErrorCode::InvalidParams,
                "a11y.query role must be a string".to_string(),
            ))
        }
    };
    let name = match params.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err((
                RpcErrorCode::InvalidParams,
                "a11y.query name must be a string".to_string(),
            ))
        }
    };
    if role.is_none() && name.is_none() {
        return Err((
            RpcErrorCode::InvalidParams,
            "a11y.query requires `role` and/or `name`".to_string(),
        ));
    }
    let component = d.a11y.as_ref().ok_or((
        RpcErrorCode::BackendUnavailable,
        "AT-SPI unavailable".to_string(),
    ))?;
    let target = SemanticTarget::ByAccessibility {
        role,
        name,
        parent_role: None,
        parent_name: None,
    };
    let nodes = component
        .locator()
        .locate(&target)
        .await
        .map_err(|e| match e {
            AgentShellError::BackendUnavailable(msg) => (RpcErrorCode::BackendUnavailable, msg),
            other => (RpcErrorCode::BackendError, other.to_string()),
        })?;
    let elements = nodes
        .into_iter()
        .map(|n| A11yElementResult {
            bus_name: n.bus_name,
            path: n.path,
            name: n.name,
            role: n.role.name,
            role_code: n.role.code,
            states: n.states.0,
        })
        .collect::<Vec<_>>();
    let r = A11yQueryResult {
        count: elements.len(),
        elements,
    };
    Ok(serde_json::to_value(r).expect("A11yQueryResult serializable"))
}

// ───────────────────────── 事件 / daemon / IME ─────────────────────────

// ───────────────────────── 安全边界（§22.7 D6）─────────────────────────

/// 返回当前安全配置真值：默认确认级别、白名单、黑名单、审计路径。
async fn security_status(d: &mut Daemon) -> RpcResult {
    let cfg = &d.security.config;
    let allow: serde_json::Map<_, _> = cfg
        .permissions
        .allow
        .iter()
        .map(|(agent, levels)| {
            (
                agent.clone(),
                serde_json::Value::Array(
                    levels
                        .iter()
                        .map(|l| serde_json::Value::from(l.as_str()))
                        .collect(),
                ),
            )
        })
        .collect();
    Ok(json!({
        "default_confirm_level": cfg.security.default_confirm_level.as_str(),
        "allow": allow,
        "deny": cfg.permissions.deny,
        "audit_path": d.security.audit.path(),
    }))
}

/// `security.grant {agent_id, level}` —— 授权 agent 到指定级别并持久化。
async fn security_grant(d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let agent_id = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing agent_id".into()))?;
    let level = params
        .get("level")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing level".into()))?
        .parse::<agent_shell_core::security::PermissionLevel>()
        .map_err(|e| (RpcErrorCode::InvalidParams, e))?;
    d.security.grant(agent_id, level).map_err(|e| {
        tracing::warn!("security.grant failed: {e}");
        (RpcErrorCode::InternalError, e)
    })?;
    Ok(json!({"granted": agent_id, "level": level.as_str()}))
}

/// `security.revoke {agent_id}` —— 撤销 agent 白名单并持久化。
async fn security_revoke(d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let agent_id = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing agent_id".into()))?;
    d.security.revoke(agent_id).map_err(|e| {
        tracing::warn!("security.revoke failed: {e}");
        (RpcErrorCode::InternalError, e)
    })?;
    Ok(json!({"revoked": agent_id}))
}

/// `security.audit [{agent_id}, {op}, {decision}]` —— 过滤读回审计日志。
async fn security_audit(d: &mut Daemon, req: &Request) -> RpcResult {
    let params = req.params.as_ref().cloned().unwrap_or_default();
    let agent_id = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let op = params.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let decision = params
        .get("decision")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let entries = d.security.audit.query(agent_id, op, decision);
    Ok(json!({"entries": entries}))
}

/// stub_ok — 扩展系统服务的占位响应（§21.35，待 Phase 3 接线）。
fn stub_ok(name: &str) -> RpcResult {
    Ok(json!({"status": "not_implemented", "service": name}))
}

/// 事件订阅（§22.5 D4）：解析过滤器、创建订阅句柄，返回 subscriber_id。
///
/// 推送在 `serve_connection` 层：dispatch 返回订阅句柄后由主循环 spawn
/// 转发任务，把匹配事件序列化为 JSON-RPC notification 写入 stdout。
///
/// **当前范围（Phase 2/TSI-2317）**：`EventNormalizer` 尚未装配，KWin
/// `subscribe()` 未被 daemon 消费——事件仅在 `windows_list` 触发窗口缓存
/// 刷新时经差分产生。因此 `events subscribe` 只交付 `subscriber_id` 协议
/// 与 replay 数据源，不含持续的原始事件流推送。偏差详见设计文档 §22.5。
async fn events_subscribe(d: &mut Daemon, req: &Request) -> RpcResult {
    let filter = parse_event_filter(req)?;
    let sub = d.hub.subscribe(filter);
    let id = sub.id().to_string();
    // 订阅句柄暂存于 daemon，由 serve_connection 取走并 spawn 转发任务。
    d.subscriptions.push(sub);
    Ok(json!({"subscriber_id": id}))
}

/// 事件取消订阅（§22.5 D4）：幂等，未知 id 同样返回 unsubscribed。
async fn events_unsubscribe(d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let raw = params
        .get("subscriber_id")
        .and_then(|v| v.as_str())
        .ok_or((
            RpcErrorCode::InvalidParams,
            "missing subscriber_id".to_string(),
        ))?;
    let id = Uuid::parse_str(raw).map_err(|_| {
        (
            RpcErrorCode::InvalidParams,
            format!("invalid subscriber_id: {raw}"),
        )
    })?;
    d.hub.unsubscribe(id);
    Ok(json!({"unsubscribed": true}))
}

async fn events_replay(d: &mut Daemon, req: &Request) -> RpcResult {
    let filter = parse_event_filter(req)?;
    let events = d
        .ring
        .snapshot()
        .into_iter()
        .filter(|e| filter.matches(e))
        .map(|e| serde_json::to_value(e).expect("DesktopEvent serializable"))
        .collect::<Vec<_>>();
    let count = events.len();
    Ok(json!({"events": events, "count": count}))
}

/// 解析 events.subscribe 的可选 `filter` 字符串（逗号分隔类别）。
fn parse_event_filter(req: &Request) -> Result<EventFilter, (RpcErrorCode, String)> {
    let Some(params) = req.params.as_ref() else {
        return Ok(EventFilter::all());
    };
    let Some(raw) = params.get("filter").and_then(|v| v.as_str()) else {
        return Ok(EventFilter::all());
    };
    if raw.trim().is_empty() {
        return Ok(EventFilter::all());
    }
    let mut f = EventFilter::default();
    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match token {
            "all" => return Ok(EventFilter::all()),
            "window" => f.window_events = true,
            "workspace" => f.workspace_events = true,
            "monitor" => f.monitor_events = true,
            "input" => f.input_events = true,
            "app" => f.app_events = true,
            "a11y" => f.a11y_events = true,
            "power" => f.power_events = true,
            other => {
                return Err((
                    RpcErrorCode::InvalidParams,
                    format!("unknown event filter: {other}"),
                ))
            }
        }
    }
    Ok(f)
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

/// 设置系统主机名（rootd HostnameSet，§23.4）。
///
/// 参数：{ "hostname": "workstation-01" }
/// 主机名校验在 rootd `validate_hostname` 完成——daemon 纯路由，
/// 不重复校验（rootd 是唯一的权威边界）。rootd 未安装时返回降级错误。
async fn hostname_set(_d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let hostname = params
        .get("hostname")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing hostname".into()))?;

    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;

    proxy
        .hostname_set(hostname)
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    Ok(json!({ "accepted": true, "hostname": hostname }))
}

/// 杀进程（rootd ProcessKill，§23.4）。
///
/// 参数：{ "pid": 1234, "signal": 15 }
/// pid/signal 的语义校验在 rootd `process_kill` 完成——daemon 纯路由，
/// 不重复校验（rootd 是唯一的权威边界）。此处仅做表示层窄化：JSON 取值为
/// i64，而 D-Bus 签名为 i32，超范围必须拒绝而非静默截断（`2³²+1234` 截断
/// 成 `1234` 会绕过 rootd 校验并对无关进程发信号）。rootd 未安装时返回降级错误。
async fn process_kill(_d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let pid = params
        .get("pid")
        .and_then(|v| v.as_i64())
        .ok_or((RpcErrorCode::InvalidParams, "missing pid".into()))?;
    let signal = params
        .get("signal")
        .and_then(|v| v.as_i64())
        .ok_or((RpcErrorCode::InvalidParams, "missing signal".into()))?;
    // 表示层窄化（i64 JSON → i32 D-Bus）：超范围拒绝，与 rootd `i32::try_from`
    // 语义一致，防 `pid > i32::MAX` / 回绕值绕过 rootd 校验。
    let pid = i32::try_from(pid)
        .map_err(|_| (RpcErrorCode::InvalidParams, "pid out of i32 range".into()))?;
    let signal = i32::try_from(signal).map_err(|_| {
        (
            RpcErrorCode::InvalidParams,
            "signal out of i32 range".into(),
        )
    })?;

    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;

    proxy
        .process_kill(pid, signal)
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    Ok(json!({ "accepted": true, "pid": pid, "signal": signal }))
}

// ───────────────────────── mount / unmount（§23.4） ─────────────────────────

/// 挂载文件系统（rootd Mount）。
///
/// 参数：{ "device": "/dev/sda1", "target": "/mnt/data", "fstype": "ext4",
///         "options": ["rw", "noatime"] }
/// rootd 未安装时返回降级错误。
async fn mount(_d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let device = params
        .get("device")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing device".into()))?;
    let target = params
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing target".into()))?;
    let fstype = params
        .get("fstype")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing fstype".into()))?;
    let options: Vec<String> = params
        .get("options")
        .map(parse_mount_options)
        .transpose()?
        .unwrap_or_default();

    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;

    proxy
        .mount(device, target, fstype, options.clone())
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    Ok(json!({
        "accepted": true,
        "device": device,
        "target": target,
        "fstype": fstype,
        "options": options,
    }))
}

/// 卸载文件系统（rootd Unmount）。
///
/// 参数：{ "target": "/mnt/data" }
/// rootd 未安装时返回降级错误。
async fn unmount(_d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let target = params
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing target".into()))?;

    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;

    proxy
        .unmount(target)
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    Ok(json!({ "accepted": true, "target": target }))
}

/// 解析 `options` 参数：必须是字符串数组，逐项取 `&str`。
fn parse_mount_options(v: &Value) -> Result<Vec<String>, (RpcErrorCode, String)> {
    let arr = v.as_array().ok_or((
        RpcErrorCode::InvalidParams,
        "options must be an array".into(),
    ))?;
    arr.iter()
        .map(|e| {
            e.as_str().map(str::to_owned).ok_or((
                RpcErrorCode::InvalidParams,
                "option must be a string".into(),
            ))
        })
        .collect()
}

/// 查询 job 状态（rootd JobStatus）。
///
/// 参数：{ "job_id": "job-3" }。返回 rootd 的 JSON
/// `{found, method, progress, done, success, exit_code, stderr}`；
/// 未知 job 时 rootd 返回 `{"found": false}`（job 可能已被 drain 淘汰）。
/// rootd 未安装 → `BackendUnavailable` 降级错误。
async fn job_status(_d: &mut Daemon, req: &Request) -> RpcResult {
    let params = params_of(req)?;
    let job_id = params
        .get("job_id")
        .and_then(|v| v.as_str())
        .ok_or((RpcErrorCode::InvalidParams, "missing job_id".into()))?;

    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — job status unavailable".into(),
    ))?;

    let result = proxy
        .job_status(job_id)
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    let parsed: Value = serde_json::from_str(&result).map_err(|e| {
        (
            RpcErrorCode::BackendError,
            format!("rootd JobStatus parse: {e}"),
        )
    })?;
    Ok(parsed)
}

// ───────────────────────── pkg（rootd 特权链路，§23.4） ─────────────────────────

/// 提取 `packages` 参数：必须存在且为字符串数组。
fn packages_param(req: &Request) -> Result<Vec<String>, (RpcErrorCode, String)> {
    let params = params_of(req)?;
    params
        .get("packages")
        .map(parse_packages)
        .transpose()?
        .ok_or((RpcErrorCode::InvalidParams, "missing packages".into()))
}

/// 解析 `packages` 数组：逐项必须是字符串。
fn parse_packages(v: &Value) -> Result<Vec<String>, (RpcErrorCode, String)> {
    let arr = v.as_array().ok_or((
        RpcErrorCode::InvalidParams,
        "packages must be an array".into(),
    ))?;
    arr.iter()
        .map(|e| {
            e.as_str().map(str::to_owned).ok_or((
                RpcErrorCode::InvalidParams,
                "package must be a string".into(),
            ))
        })
        .collect()
}

/// 解析 rootd 返回的 job JSON（`{job_id, pm}`）并原样上抛。
fn parse_package_result(result: &str) -> RpcResult {
    serde_json::from_str::<Value>(result).map_err(|e| {
        (
            RpcErrorCode::BackendError,
            format!("rootd package parse: {e}"),
        )
    })
}

/// 安装系统软件包（rootd PackageInstall，§23.4）。
///
/// 参数：{ "packages": ["nginx", "curl"] }。包名合法性由 rootd 校验
/// （CLI/daemon 不重复校验）。返回 rootd 的 `{job_id, pm}`。
async fn package_install(_d: &mut Daemon, req: &Request) -> RpcResult {
    let packages = packages_param(req)?;
    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;
    let result = proxy
        .package_install(packages)
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    parse_package_result(&result)
}

/// 移除系统软件包（rootd PackageRemove，§23.4）。
async fn package_remove(_d: &mut Daemon, req: &Request) -> RpcResult {
    let packages = packages_param(req)?;
    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;
    let result = proxy
        .package_remove(packages)
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    parse_package_result(&result)
}

/// 升级系统软件包（rootd PackageUpdate，§23.4；空数组 = 全部升级）。
async fn package_update(_d: &mut Daemon, req: &Request) -> RpcResult {
    let packages = packages_param(req)?;
    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;
    let result = proxy
        .package_update(packages)
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    parse_package_result(&result)
}

/// 刷新包元数据缓存（rootd PackageRefresh，§23.4；无参数）。
async fn package_refresh(_d: &mut Daemon) -> RpcResult {
    let proxy = crate::rootd_client::connect().await.ok_or((
        RpcErrorCode::BackendUnavailable,
        "rootd not installed — privileged operation unavailable".into(),
    ))?;
    let result = proxy
        .package_refresh()
        .await
        .map_err(|e| (RpcErrorCode::BackendError, format!("rootd: {e}")))?;
    parse_package_result(&result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shell_core::security::{AgentShellConfig, SecurityManager};
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
    async fn info_reports_capabilities_from_compositor_truth() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::INFO, None)).await;
        let v = resp.result.expect("ok");
        let r: InfoResult = serde_json::from_value(v).expect("InfoResult");

        // 能力位表固定 9 行且名称顺序稳定（TSI-2569：不得硬编码）。
        let names: Vec<&str> = r.capabilities.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "window_management",
                "workspace_management",
                "monitor_layout",
                "window_events",
                "workspace_events",
                "native_input",
                "native_capture",
                "virtual_desktops",
                "effects_control"
            ]
        );

        // 逐字段等于合成器能力真值；native_capture 例外——capture 组件
        // 独立于 compositor（纯 X11 会话仍可截图）。
        let caps = d.compositor_capabilities();
        for (name, enabled) in &r.capabilities {
            let expected = match name.as_str() {
                "window_management" => caps.window_management,
                "workspace_management" => caps.workspace_management,
                "monitor_layout" => caps.monitor_layout,
                "window_events" => caps.window_events,
                "workspace_events" => caps.workspace_events,
                "native_input" => caps.native_input,
                "native_capture" => d.capture.is_some(),
                "virtual_desktops" => caps.virtual_desktops,
                "effects_control" => caps.effects_control,
                other => panic!("unknown capability row: {other}"),
            };
            assert_eq!(*enabled, expected, "capability row {name}");
        }

        // 无合成器（CI/headless）不得硬编码 window_management:true。
        if !d.has_compositor() {
            let wm = r
                .capabilities
                .iter()
                .find(|(n, _)| n == "window_management")
                .map(|(_, enabled)| *enabled)
                .expect("window_management row");
            assert!(!wm, "headless must report window_management=false");
        }
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
    async fn a11y_query_non_string_role_name_is_invalid_params() {
        // TSI-2480 QA 回归锚定：类型校验先于后端可用性判定，
        // headless（无 AT-SPI bus）环境也须返回 InvalidParams。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        for params in [json!({"role": 123}), json!({"name": true})] {
            let resp = dispatch(&mut d, &req(method::A11Y_QUERY, Some(params))).await;
            assert_eq!(
                resp.error.expect("error").code,
                RpcErrorCode::InvalidParams as i32
            );
        }
    }

    #[tokio::test]
    async fn a11y_query_null_is_treated_as_absent() {
        // `null` 与缺省等价：单条件仍有效，绝不可判 InvalidParams。
        // 有 AT-SPI bus 时返回结果（含 count），headless 时退化
        // BackendUnavailable——两条路径都证明 null 未触发类型/缺参误拒。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(
            &mut d,
            &req(
                method::A11Y_QUERY,
                Some(json!({"role": "pushButton", "name": null})),
            ),
        )
        .await;
        match resp.error {
            Some(err) => {
                assert_ne!(
                    err.code,
                    RpcErrorCode::InvalidParams as i32,
                    "null must not be InvalidParams"
                );
                // headless → BackendUnavailable；AT-SPI 探测成功但定位阶段
                // bus 掉线 → BackendError。两条后端路径都证明 null 未误拒。
            }
            None => {
                let v = resp.result.expect("query ok");
                assert!(v.get("count").is_some(), "result must carry count: {v}");
            }
        }
    }

    #[tokio::test]
    async fn events_subscribe_returns_subscriber_id() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::EVENTS_SUBSCRIBE, None)).await;
        let v = resp.result.expect("subscribe ok");
        let id = v.get("subscriber_id").and_then(|v| v.as_str()).expect("id");
        Uuid::parse_str(id).expect("valid uuid");
        assert_eq!(d.subscriptions.len(), 1);
        assert_eq!(d.hub.subscriber_count(), 1);
    }

    #[tokio::test]
    async fn events_subscribe_unknown_filter_is_invalid_params() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(
            &mut d,
            &req(method::EVENTS_SUBSCRIBE, Some(json!({"filter": "bogus"}))),
        )
        .await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::InvalidParams as i32
        );
    }

    #[tokio::test]
    async fn events_unsubscribe_invalid_id_is_invalid_params() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        for params in [json!({}), json!({"subscriber_id": "not-a-uuid"})] {
            let resp = dispatch(&mut d, &req(method::EVENTS_UNSUBSCRIBE, Some(params))).await;
            assert_eq!(
                resp.error.expect("error").code,
                RpcErrorCode::InvalidParams as i32
            );
        }
    }

    #[tokio::test]
    async fn events_unsubscribe_is_idempotent() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::EVENTS_UNSUBSCRIBE, None)).await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::InvalidParams as i32
        );
        // 有效但未知的 uuid：幂等成功。
        let uuid = Uuid::new_v4().to_string();
        let resp = dispatch(
            &mut d,
            &req(
                method::EVENTS_UNSUBSCRIBE,
                Some(json!({"subscriber_id": uuid})),
            ),
        )
        .await;
        let v = resp.result.expect("unsubscribe ok");
        assert_eq!(v.get("unsubscribed").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn events_replay_returns_ring_contents() {
        // events --replay 数据源：push 一个真实 DesktopEvent 后 count=1。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.ring.push(event::DesktopEvent::WindowClosed {
            id: test_window_id(),
            source: event::EventSource::KWinWayland,
            occurred_at: std::time::Instant::now(),
        });
        let resp = dispatch(&mut d, &req(method::EVENTS_REPLAY, None)).await;
        let v = resp.result.expect("replay ok");
        let count = v.get("count").and_then(|v| v.as_u64()).expect("count");
        assert_eq!(count, 1);
        let events = v.get("events").and_then(|v| v.as_array()).expect("events");
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn events_replay_honors_filter() {
        // 阻塞 3 回归锚定：`filter` 参数必须被 events_replay 应用，
        // 未匹配类别（含 Noop）被过滤。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.ring.push(event::DesktopEvent::WindowClosed {
            id: test_window_id(),
            source: event::EventSource::KWinWayland,
            occurred_at: std::time::Instant::now(),
        });
        d.ring.push(event::DesktopEvent::Noop);

        let resp = dispatch(
            &mut d,
            &req(method::EVENTS_REPLAY, Some(json!({"filter": "window"}))),
        )
        .await;
        let v = resp.result.expect("replay ok");
        assert_eq!(v.get("count").and_then(|v| v.as_u64()), Some(1));

        let resp = dispatch(
            &mut d,
            &req(method::EVENTS_REPLAY, Some(json!({"filter": "input"}))),
        )
        .await;
        let v = resp.result.expect("replay ok");
        assert_eq!(v.get("count").and_then(|v| v.as_u64()), Some(0));
    }

    fn test_window_id() -> agent_shell_core::types::WindowId {
        agent_shell_core::types::WindowId {
            native_id: "test-1".into(),
            de_type: agent_shell_core::types::DesktopEnvironment::KDE,
        }
    }

    #[tokio::test]
    async fn deny_short_circuits_before_handler() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.security
            .config
            .permissions
            .deny
            .insert("input.send".into(), true);
        let resp = dispatch(&mut d, &req(method::INPUT_SEND, Some(json!({})))).await;
        assert_eq!(resp.error.expect("error").code, RpcErrorCode::Denied as i32);
    }

    #[tokio::test]
    async fn above_level_without_whitelist_returns_confirmation_required() {
        // 默认配置：`"*"` 无白名单 → service.control（L3）需确认。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::SERVICE_CONTROL, Some(json!({})))).await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::ConfirmationRequired as i32
        );
    }

    #[tokio::test]
    async fn process_kill_default_config_returns_confirmation_required() {
        // process.kill（L3）与 service.control 同级：默认配置需确认，
        // 在 handler 之前短路——不触碰 rootd。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(
            &mut d,
            &req(
                method::PROCESS_KILL,
                Some(json!({ "pid": 1234, "signal": 15 })),
            ),
        )
        .await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::ConfirmationRequired as i32
        );
    }

    #[tokio::test]
    async fn process_kill_gate_pass_reaches_handler() {
        // L4 白名单放行后进入 handler：缺失 pid 必须报 InvalidParams
        //（证明门禁放行且 handler 已执行，而非 Denied 短路）。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        for params in [json!({}), json!({ "signal": 15 }), json!({ "pid": 1234 })] {
            let resp = dispatch(&mut d, &req(method::PROCESS_KILL, Some(params))).await;
            assert_eq!(
                resp.error.expect("error").code,
                RpcErrorCode::InvalidParams as i32
            );
        }
    }

    #[tokio::test]
    async fn process_kill_rejects_pid_out_of_i32_range() {
        // L4 放行后进入 handler：`pid > i32::MAX` 与回绕值（2³²+1234 → 1234）
        // 必须在 daemon 侧拒绝，否则 rootd 只见截断后的合法正数、对无关进程发信号。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        let wrap = (1i64 << 32) + 1234;
        for params in [
            json!({ "pid": (i32::MAX as i64) + 1, "signal": 15 }),
            json!({ "pid": wrap, "signal": 15 }),
            json!({ "pid": 1234, "signal": (i32::MAX as i64) + 1 }),
            json!({ "pid": 1234, "signal": wrap }),
        ] {
            let resp = dispatch(&mut d, &req(method::PROCESS_KILL, Some(params))).await;
            assert_eq!(
                resp.error.expect("error").code,
                RpcErrorCode::InvalidParams as i32
            );
        }
    }

    #[tokio::test]
    async fn allow_passes_gate_and_reaches_handler() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        // gate 已过、handler 已执行：bad payload 返回 InvalidParams（而非 Denied）。
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
    async fn screenshot_capture_requires_confirmation_when_whitelist_below_l2() {
        // F1：截图含屏幕内容，L2；`"*"` 白名单只有 L1 时必须确认（L0 只读
        // 不会触发确认，故该断言同时证明截图不再是 L0 只读映射）。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.security
            .config
            .permissions
            .allow
            .insert("*".into(), vec![PermissionLevel::L1]);
        let resp = dispatch(&mut d, &req(method::SCREENSHOT_CAPTURE, Some(json!({})))).await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::ConfirmationRequired as i32
        );
    }

    #[tokio::test]
    async fn grant_revoked_for_named_agent() {
        // F2：具名 caller 不可自我提权——grant/revoke 必须经 `"*"`。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "agent-x".into();
        d.security = SecurityManager::with_config(AgentShellConfig::default());
        let resp = dispatch(
            &mut d,
            &req(
                method::SECURITY_GRANT,
                Some(json!({"agent_id": "agent-x", "level": "L4"})),
            ),
        )
        .await;
        let err = resp.error.expect("error");
        assert_eq!(err.code, RpcErrorCode::Denied as i32);
        assert!(!d.security.config.permissions.allow.contains_key("agent-x"));
    }

    #[tokio::test]
    async fn management_deny_is_audited() {
        // TSI-2513 回归锚定：具名 caller 越权调用 security.grant/revoke 被拒后，
        // audit.jsonl 必须至少 1 行 deny（此前管理面拒绝完全无痕）。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "agent-x".into();
        let audit_path = std::env::temp_dir().join(format!(
            "agent-shell-dispatch-audit-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&audit_path);
        let mut cfg = AgentShellConfig::default();
        cfg.security.audit_log_path = Some(audit_path.to_string_lossy().into_owned());
        d.security = SecurityManager::with_config(cfg);
        for m in [method::SECURITY_GRANT, method::SECURITY_REVOKE] {
            let resp = dispatch(
                &mut d,
                &req(m, Some(json!({"agent_id": "agent-x", "level": "L4"}))),
            )
            .await;
            assert_eq!(
                resp.error.expect("error").code,
                RpcErrorCode::Denied as i32,
                "{m}"
            );
        }
        let denies = d.security.audit.query("", "", "deny");
        assert!(!denies.is_empty(), "audit.jsonl must record deny entries");
        assert!(denies.iter().all(|e| e.agent_id == "agent-x" && !e.result));
        let _ = std::fs::remove_file(&audit_path);
    }

    #[tokio::test]
    async fn grant_allowed_for_local_caller() {
        // F2 反例：`"*"`（本地 CLI/MCP，未注入 agent id）通过门禁、可达 handler。
        // 非法 level 由 handler 报 InvalidParams——合法 grant 会写盘污染真实配置，
        // 故用 InvalidParams 断言门禁放行；成功 grant 路径由 core security 测试覆盖。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(
            &mut d,
            &req(
                method::SECURITY_GRANT,
                Some(json!({"agent_id": "agent-x", "level": "INVALID"})),
            ),
        )
        .await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::InvalidParams as i32
        );
    }
    #[tokio::test]
    async fn hostname_set_default_config_returns_confirmation_required() {
        // hostname.set（L3）与 service.control/process.kill 同级：
        // 默认配置需确认，在 handler 之前短路——不触碰 rootd。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(
            &mut d,
            &req(
                method::HOSTNAME_SET,
                Some(json!({ "hostname": "workstation-01" })),
            ),
        )
        .await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::ConfirmationRequired as i32
        );
    }

    #[tokio::test]
    async fn job_status_missing_job_id_is_invalid_params() {
        // headless 环境：job_id 缺省先于 rootd 可用性判定报 InvalidParams。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::JOB_STATUS, Some(json!({})))).await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::InvalidParams as i32
        );
    }

    #[tokio::test]
    async fn mount_requires_confirmation_without_whitelist() {
        // 默认配置无白名单 → mount（L4）需确认，且在 handler 之前短路
        // （rootd 未安装时不会走到 BackendUnavailable）。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::MOUNT, Some(json!({})))).await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::ConfirmationRequired as i32
        );
    }

    #[tokio::test]
    async fn hostname_set_gate_pass_reaches_handler() {
        // L4 白名单放行后进入 handler：缺失/非字符串 hostname 必须报
        // InvalidParams（证明门禁放行且 handler 已执行，而非 Denied 短路）。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        for params in [
            json!({}),
            json!({ "hostname": 123 }),
            json!({ "name": "workstation-01" }),
        ] {
            let resp = dispatch(&mut d, &req(method::HOSTNAME_SET, Some(params))).await;
            assert_eq!(
                resp.error.expect("error").code,
                RpcErrorCode::InvalidParams as i32
            );
        }
    }

    #[tokio::test]
    async fn mount_validates_params_after_gate_passes() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        let resp = dispatch(&mut d, &req(method::MOUNT, Some(json!({})))).await;
        let err = resp.error.expect("error");
        assert_eq!(err.code, RpcErrorCode::InvalidParams as i32);
        assert_eq!(err.message, "missing device");
    }

    #[tokio::test]
    async fn hostname_set_valid_params_reaches_rootd_degrade() {
        // L4 放行后进入 handler：合法 hostname 通过参数提取，到达 rootd
        // 连接。本环境无 rootd → BackendUnavailable 降级，证明成功路径的
        // 前置链路（参数解析 + 门禁放行）完整；rootd Ok 分支需实机。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        let resp = dispatch(
            &mut d,
            &req(
                method::HOSTNAME_SET,
                Some(json!({ "hostname": "workstation-01" })),
            ),
        )
        .await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::BackendUnavailable as i32
        );
    }

    #[tokio::test]
    async fn unmount_validates_target_after_gate_passes() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        let resp = dispatch(&mut d, &req(method::UNMOUNT, Some(json!({})))).await;
        let err = resp.error.expect("error");
        assert_eq!(err.code, RpcErrorCode::InvalidParams as i32);
        assert_eq!(err.message, "missing target");
    }

    #[test]
    fn mount_options_rejects_non_array_and_non_string() {
        assert_eq!(
            parse_mount_options(&json!("rw")).unwrap_err().0,
            RpcErrorCode::InvalidParams
        );
        assert_eq!(
            parse_mount_options(&json!([1, 2])).unwrap_err().0,
            RpcErrorCode::InvalidParams
        );
        assert_eq!(
            parse_mount_options(&json!(["rw", "noatime"])).unwrap(),
            vec!["rw".to_string(), "noatime".to_string()]
        );
    }

    #[tokio::test]
    async fn job_status_requires_params() {
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(&mut d, &req(method::JOB_STATUS, None)).await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::InvalidParams as i32
        );
    }

    #[tokio::test]
    async fn package_install_default_config_returns_confirmation_required() {
        // package.install（L3）与 service.control/process.kill 同级：
        // 默认配置需确认，在 handler 之前短路——不触碰 rootd。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        let resp = dispatch(
            &mut d,
            &req(
                method::PACKAGE_INSTALL,
                Some(json!({ "packages": ["nginx"] })),
            ),
        )
        .await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::ConfirmationRequired as i32
        );
    }

    #[tokio::test]
    async fn package_install_bad_params_after_gate_pass_is_invalid_params() {
        // L4 白名单放行后进入 handler：缺失/非数组/非字符串元素必须报
        // InvalidParams（证明门禁放行且 handler 已执行，而非 Denied 短路）。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        for params in [
            json!({}),
            json!({ "packages": 5 }),
            json!({ "packages": [1] }),
            json!({ "packages": ["ok", 2] }),
        ] {
            let resp = dispatch(&mut d, &req(method::PACKAGE_INSTALL, Some(params))).await;
            assert_eq!(
                resp.error.expect("error").code,
                RpcErrorCode::InvalidParams as i32
            );
        }
    }

    #[tokio::test]
    async fn package_refresh_gate_pass_reaches_rootd_degrade() {
        // L4 放行后进入 handler：无参数，到达 rootd 连接。本环境无 rootd
        // → BackendUnavailable 降级，证明成功路径前置链路（门禁放行 +
        // handler 执行）完整；rootd Ok 分支需实机。
        let mut d = Daemon::connect(Duration::from_secs(1)).await;
        d.caller_id = "trusted".into();
        d.security
            .config
            .permissions
            .allow
            .insert("trusted".into(), vec![PermissionLevel::L4]);
        let resp = dispatch(&mut d, &req(method::PACKAGE_REFRESH, Some(json!({})))).await;
        assert_eq!(
            resp.error.expect("error").code,
            RpcErrorCode::BackendUnavailable as i32
        );
    }
}
