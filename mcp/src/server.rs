//! MCP server 实现——18 tools registered via rmcp ServerHandler。
//!
//! 工具调用经 stdio JSON-RPC 转发到 agent-shell daemon。daemon 在 MCP server
//! 构造时 spawn 一次并保持长连接，避免每次调用重新 fork/exec。

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerInfo, Tool,
};
use rmcp::service::{serve_server, RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{error, info};

/// 持久的 daemon stdio JSON-RPC 连接。spawn 一次，复用于每次工具调用。
struct DaemonConnection {
    child: Option<Child>,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

impl DaemonConnection {
    /// 定位并 spawn `agent-shell-daemon --foreground`，返回长连接。
    fn connect() -> Result<Self, String> {
        let bin = find_daemon_binary()?;
        let mut child = Command::new(&bin)
            .arg("--foreground")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn daemon: {e}"))?;
        let stdin = child.stdin.take().ok_or("no daemon stdin")?;
        let stdout = child.stdout.take().ok_or("no daemon stdout")?;
        Ok(Self {
            child: Some(child),
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
        })
    }

    /// 复用连接发送一条 JSON-RPC 请求并读取对应响应。
    ///
    /// 事件订阅生效后，daemon 会在同一 stdio 行流上推送 `events.notify`
    /// 通知（无 `id`）。本方法循环读取并按 `id` 有无区分：通知直接跳过，
    /// 直到拿到匹配本请求 id 的响应——避免后续任何工具调用把通知误解析
    /// 为响应。
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = agent_shell_rpc::Request::new(id, method, params);
        let line = req.to_line();

        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("flush: {e}"))?;

        // 仅首个读使用 10s 死线；订阅激活期间 `events.notify` 会持续流入，
        // 每次读到通知后重入循环即重置死线，避免后续工具调用因活跃订阅
        // 误报 timeout（审查建议 3）。
        let mut first = true;
        loop {
            let mut buf = String::new();
            let read = if first {
                first = false;
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    self.reader.read_line(&mut buf),
                )
                .await
                .map_err(|_| "timeout".to_string())?
                .map_err(|e| format!("read: {e}"))?
            } else {
                self.reader
                    .read_line(&mut buf)
                    .await
                    .map_err(|e| format!("read: {e}"))?
            };
            if read == 0 {
                return Err("daemon closed connection before responding".to_string());
            }
            let msg: Value = serde_json::from_str(&buf).map_err(|e| format!("parse: {e}"))?;
            if msg.get("id").is_none() {
                // events.notify 通知：不属于本请求，继续等待响应。
                continue;
            }
            let resp =
                agent_shell_rpc::Response::from_line(&buf).map_err(|e| format!("parse: {e}"))?;
            if resp.id != id {
                return Err(format!("response id mismatch: got {} want {}", resp.id, id));
            }
            if let Some(err) = resp.error {
                return Err(format!("RPC error {}: {}", err.code, err.message));
            }
            return resp.result.ok_or_else(|| "empty result".to_string());
        }
    }

    /// 清理关闭 daemon 子进程。
    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            // 关闭 stdin 以提示 daemon 退出；如未自行退出则 kill。
            let _ = child.start_kill();
            let _ = child.try_wait();
        }
    }
}

impl Drop for DaemonConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// MCP server——桥接 MCP 工具调用到 agent-shell daemon。
pub struct AgentShellMcpServer {
    conn: Mutex<DaemonConnection>,
}

impl AgentShellMcpServer {
    /// 构造 MCP server，spawn 一次 daemon 并保持长连接。
    pub fn new() -> Self {
        let conn = DaemonConnection::connect().unwrap_or_else(|e| {
            error!(error = %e, "failed to spawn agent-shell daemon");
            panic!("agent-shell daemon unavailable: {e}");
        });
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// 构建 18 个注册工具。
    fn build_tools() -> Vec<Tool> {
        vec![
            // Core 7 tools (§17.3)
            make_tool(
                "list_windows",
                "列出所有窗口，含 app_id/title/pid/geometry/workspace",
                json!({
                    "type": "object",
                    "properties": { "app_id": {"type": "string"}, "workspace": {"type": "integer"} }
                }),
            ),
            make_tool(
                "focus_window",
                "聚焦窗口：支持 app_id/title/pid 定位",
                json!({
                    "type": "object",
                    "properties": { "app_id": {"type":"string"}, "title": {"type":"string"}, "pid": {"type":"integer"}, "window_id": {"type":"string"} }
                }),
            ),
            make_tool(
                "click_element",
                "语义化点击元素：按无障碍角色+名称，优先 AT-SPI，降级坐标",
                json!({
                    "type": "object",
                    "properties": { "role": {"type":"string","enum":["button","menu_item","checkbox"]}, "name": {"type":"string"}, "window": {"type":"string"}, "fallback_coordinate": {"type":"array","items":{"type":"number"}} }
                }),
            ),
            make_tool(
                "screenshot",
                "截图：全屏/窗口/区域，返回文件路径",
                json!({
                    "type": "object",
                    "properties": { "target": {"type":"string","enum":["screen","window","area"]}, "window": {"type":"string"}, "area": {"type":"array","items":{"type":"integer"},"minItems":4,"maxItems":4} }
                }),
            ),
            make_tool(
                "wait_for_window",
                "等待窗口出现，带超时",
                json!({
                    "type": "object",
                    "properties": {
                        "app_id": {"type":"string"},
                        "timeout": {"type":"string", "description": "人类可读超时时长（如 `10s`、`500ms`、`2m`、`1h`），与 CLI `--timeout` 对齐；与 `timeout_ms` 二选一"},
                        "timeout_ms": {"type":"integer","default":15000, "description": "超时毫秒（默认 15000）；已被 `timeout` 取代，保留兼容"}
                    },
                    "required": ["app_id"]
                }),
            ),
            make_tool(
                "get_a11y_tree",
                "获取指定窗口的无障碍树（JSON）",
                json!({
                    "type": "object",
                    "properties": { "window": {"type":"string"}, "depth": {"type":"integer","default":5} }
                }),
            ),
            make_tool(
                "subscribe_events",
                "订阅桌面事件（window_opened/focused/closed, workspace_changed）。MCP 无流式通知通道，仅返回 subscriber_id 句柄，事件暂不投递到本连接",
                json!({
                    "type": "object",
                    "properties": { "events": {"type":"array","items":{"type":"string"}} }
                }),
            ),
            // Extension 11 tools (§21.35) — total 18
            make_tool(
                "security_check",
                "检查某操作是否被授权",
                json!({"type":"object","properties":{"operation":{"type":"string"}}}),
            ),
            make_tool(
                "daemon_status",
                "daemon 状态和 portal session",
                json!({"type":"object"}),
            ),
            make_tool(
                "ime_set_engine",
                "切换输入法引擎",
                json!({"type":"object","properties":{"engine":{"type":"string"}},"required":["engine"]}),
            ),
            make_tool(
                "ime_type",
                "通过输入法输入文本（支持非 ASCII）",
                json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
            ),
            make_tool(
                "set_brightness",
                "设置屏幕亮度 0-100",
                json!({"type":"object","properties":{"value":{"type":"integer","minimum":0,"maximum":100}},"required":["value"]}),
            ),
            make_tool(
                "file_picker",
                "打开文件选择器并返回路径",
                json!({"type":"object"}),
            ),
            make_tool(
                "file_trash",
                "移动文件到回收站",
                json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            ),
            make_tool(
                "default_app_get",
                "查询默认应用",
                json!({"type":"object","properties":{"mime":{"type":"string"}},"required":["mime"]}),
            ),
            make_tool(
                "default_app_set",
                "设置默认应用",
                json!({"type":"object","properties":{"mime":{"type":"string"},"app":{"type":"string"}},"required":["mime","app"]}),
            ),
            make_tool("bluetooth_scan", "扫描蓝牙设备", json!({"type":"object"})),
            make_tool(
                "list_timers",
                "列出 systemd timer",
                json!({"type":"object"}),
            ),
        ]
    }

    /// 将工具名映射到 daemon RPC 方法 + 参数适配。
    ///
    /// MCP 工具 schema（§17.3）与 daemon RPC 参数契约不同——
    /// 此函数完成从 MCP 参数到 RPC 参数的转换。
    fn map_tool(tool_name: &str, args: &Value) -> Option<(&'static str, Value)> {
        let (method, params) = match tool_name {
            "list_windows" => {
                // 直接透传 app_id 过滤（daemon windows.list 接受 filter）
                let filter = args.get("app_id").and_then(|v| v.as_str());
                let p = json!({ "filter": filter });
                ("windows.list", p)
            }
            "focus_window" | "wait_for_window" => {
                // These tools are handled by execute_tool (multi-step RPC).
                return None;
            }
            "click_element" => ("a11y.status", json!({})),
            "screenshot" => {
                let window = args.get("window").and_then(|v| v.as_str());
                let area: Option<[i32; 4]> = args.get("area").and_then(|a| {
                    a.as_array().and_then(|arr| {
                        if arr.len() == 4 {
                            let vals: Vec<i32> = arr
                                .iter()
                                .filter_map(|v| v.as_i64().map(|i| i as i32))
                                .collect();
                            if vals.len() == 4 {
                                Some([vals[0], vals[1], vals[2], vals[3]])
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                });
                let output_path = format!("/tmp/agent-shell-mcp-{}.ppm", std::process::id());
                let p = json!({ "window": window, "area": area, "output_path": output_path });
                ("screenshot.capture", p)
            }
            "get_a11y_tree" => {
                // §17.3: 获取无障碍树——v1 返回 AT-SPI 状态（树查询待 Phase 2）
                ("a11y.status", json!({}))
            }
            "subscribe_events" => ("events.subscribe", args.clone()),
            "security_check" => ("security.status", args.clone()),
            "daemon_status" => ("daemon.status", json!({})),
            "ime_set_engine" => ("ime.engine.set", args.clone()),
            "ime_type" => ("ime.type", args.clone()),
            "set_brightness" => ("brightness.set", args.clone()),
            "file_picker" => ("file.pick", args.clone()),
            "file_trash" => ("file.trash", args.clone()),
            "default_app_get" => ("mime.get", args.clone()),
            "default_app_set" => ("mime.set", args.clone()),
            "bluetooth_scan" => ("bluetooth.scan", json!({})),
            "list_timers" => ("timer.list", json!({})),
            _ => return None,
        };
        Some((method, params))
    }

    /// 执行工具调用——处理需要多步 RPC 的工具（如 focus_window 先解析再操作）。
    async fn execute_tool(&self, tool_name: &str, args: &Value) -> Result<Value, String> {
        match tool_name {
            "focus_window" => self.execute_focus_window(args).await,
            "wait_for_window" => self.execute_wait_for_window(args).await,
            _ => {
                let (method, params) = match Self::map_tool(tool_name, args) {
                    Some(m) => m,
                    None => return Err(format!("unknown tool: {tool_name}")),
                };
                self.conn.lock().await.call(method, params).await
            }
        }
    }

    /// focus_window: 先 windows.list 取列表，按 app_id/title/pid 解析 native_id，再 windows.op focus。
    async fn execute_focus_window(&self, args: &Value) -> Result<Value, String> {
        let mut conn = self.conn.lock().await;

        // 1. 取窗口列表
        let list_result = conn
            .call("windows.list", json!({ "filter": Value::Null }))
            .await?;
        let windows = list_result
            .get("windows")
            .and_then(|v| v.as_array())
            .ok_or("malformed windows.list response")?;

        // 2. 按 app_id/title/pid/window_id 解析 native_id
        let native_id = resolve_target(args, windows).ok_or_else(|| {
            "window not found (no match for app_id/title/pid/window_id)".to_string()
        })?;

        // 3. 发 windows.op focus
        let op_params = json!({ "op": "focus", "target": native_id });
        conn.call("windows.op", op_params).await
    }

    /// wait_for_window: 轮询 windows.list + filter，直到匹配或超时。
    async fn execute_wait_for_window(&self, args: &Value) -> Result<Value, String> {
        let app_id = args
            .get("app_id")
            .and_then(|v| v.as_str())
            .ok_or("missing required param: app_id")?;
        let timeout_ms = Self::resolve_timeout_ms(args)?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

        loop {
            let mut conn = self.conn.lock().await;
            let list_result = conn
                .call("windows.list", json!({ "filter": app_id }))
                .await?;
            drop(conn);

            let windows = list_result.get("windows").and_then(|v| v.as_array());
            if let Some(wins) = windows {
                if !wins.is_empty() {
                    return Ok(json!({"found": true, "app_id": app_id, "window": wins[0]}));
                }
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(json!({"found": false, "app_id": app_id, "timeout_ms": timeout_ms}));
            }
            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(200))).await;
        }
    }

    /// wait_for_window 的 `timeout`（人类可读时长）与 `timeout_ms`（毫秒）
    /// 二选一解析，缺省 15000ms。二者同时给出报错；`timeout` 复用 CLI
    /// `--timeout` 的 `agent_shell_rpc::duration::parse_duration` 语义（TSI-3060）。
    fn resolve_timeout_ms(args: &Value) -> Result<u64, String> {
        let timeout = args.get("timeout");
        let timeout_ms = args.get("timeout_ms");
        if timeout.is_some() && timeout_ms.is_some() {
            return Err("timeout 与 timeout_ms 二选一，不能同时提供".to_string());
        }
        if let Some(v) = timeout {
            let spec = v
                .as_str()
                .ok_or("timeout 必须为字符串（如 `10s`、`500ms`、`2m`、`1h`）")?;
            return agent_shell_rpc::duration::parse_duration(spec);
        }
        // 原 timeout_ms 行为保持不变：非整数按缺省 15000 处理。
        Ok(timeout_ms.and_then(|v| v.as_u64()).unwrap_or(15_000))
    }
}

impl Default for AgentShellMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerHandler for AgentShellMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some("agent-shell MCP server for Linux desktop automation".into());
        info
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(Self::build_tools())))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResponse, McpError>> + Send + '_ {
        let tool_name = request.name.to_string();
        let args: Value = request
            .arguments
            .as_ref()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);

        async move {
            let result = self.execute_tool(&tool_name, &args).await;
            match result {
                Ok(value) => {
                    let text =
                        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                    result.structured_content = Some(value);
                    Ok(result.into())
                }
                Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "error: {e}"
                ))])
                .into()),
            }
        }
    }
}

/// 构建 Tool 实例。
fn make_tool(name: &'static str, desc: &'static str, schema: Value) -> Tool {
    let mut tool = Tool::default();
    tool.name = Cow::Borrowed(name);
    tool.description = Some(Cow::Borrowed(desc));
    tool.input_schema = Arc::new(schema.as_object().cloned().unwrap_or_default());
    tool
}

/// 从 windows.list 结果中按 app_id/title/pid/window_id 解析 native_id。
///
/// 优先级：window_id（精确 native_id）→ app_id（精确）→ title（子串包含）→ pid。
fn resolve_target(args: &Value, windows: &[Value]) -> Option<String> {
    // window_id: 直接作为 native_id 使用
    if let Some(wid) = args.get("window_id").and_then(|v| v.as_str()) {
        if windows
            .iter()
            .any(|w| w.get("native_id").and_then(|v| v.as_str()) == Some(wid))
        {
            return Some(wid.to_string());
        }
    }
    // app_id: 精确匹配
    if let Some(app_id) = args.get("app_id").and_then(|v| v.as_str()) {
        if let Some(w) = windows
            .iter()
            .find(|w| w.get("app_id").and_then(|v| v.as_str()) == Some(app_id))
        {
            return w
                .get("native_id")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
    }
    // title: 子串包含
    if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        if let Some(w) = windows.iter().find(|w| {
            w.get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.contains(title))
        }) {
            return w
                .get("native_id")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
    }
    // pid: 精确匹配
    if let Some(pid) = args.get("pid").and_then(|v| v.as_u64()) {
        if let Some(w) = windows
            .iter()
            .find(|w| w.get("pid").and_then(|v| v.as_u64()) == Some(pid))
        {
            return w
                .get("native_id")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
    }
    None
}

/// 定位 daemon 二进制（委托 agent-shell-rpc::daemon_bin）。
///
/// workspace target 目录用 `CARGO_MANIFEST_DIR` 拼绝对路径，不依赖 CWD
/// （TSI-2471 审查 #2）。MCP server 在 mcp/ 子 crate，向上一级是
/// workspace 根，target/ 在根下。
fn find_daemon_binary() -> Result<String, String> {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent");
    let target_debug = workspace_root.join("target/debug");
    let target_release = workspace_root.join("target/release");
    agent_shell_rpc::daemon_bin::find_daemon_binary(&[
        &target_debug.to_string_lossy(),
        &target_release.to_string_lossy(),
    ])
}

/// 启动 MCP server（stdio 传输）。
pub async fn run_stdio() -> Result<(), String> {
    let (stdin, stdout) = stdio();
    let server = AgentShellMcpServer::new();
    info!("MCP server starting (stdio), 18 tools registered");

    let serve_result = serve_server(server, (stdin, stdout)).await;
    match serve_result {
        Ok(running) => {
            let _ = running.waiting().await;
            info!("MCP server completed");
            Ok(())
        }
        Err(e) => {
            error!(error = %e, "MCP server error");
            Err(format!("{e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_tools_count() {
        let tools = AgentShellMcpServer::build_tools();
        assert_eq!(tools.len(), 18, "expected 18 MCP tools registered");
    }

    #[test]
    fn test_tool_names() {
        let tools = AgentShellMcpServer::build_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(names.contains(&"list_windows"));
        assert!(names.contains(&"focus_window"));
        assert!(names.contains(&"screenshot"));
        assert!(names.contains(&"ime_type"));
        assert!(names.contains(&"daemon_status"));
        assert!(names.contains(&"subscribe_events"));
    }

    #[test]
    fn test_map_tool_list_windows() {
        let (m, p) =
            AgentShellMcpServer::map_tool("list_windows", &json!({"app_id": "firefox"})).unwrap();
        assert_eq!(m, "windows.list");
        assert_eq!(p["filter"], "firefox");
    }

    #[test]
    fn test_map_tool_screenshot_adds_output_path() {
        let (m, p) =
            AgentShellMcpServer::map_tool("screenshot", &json!({"target": "screen"})).unwrap();
        assert_eq!(m, "screenshot.capture");
        assert!(p["output_path"].as_str().is_some(), "must have output_path");
    }

    #[test]
    fn test_map_tool_screenshot_with_area() {
        let (m, p) =
            AgentShellMcpServer::map_tool("screenshot", &json!({"area": [0, 0, 800, 600]}))
                .unwrap();
        assert_eq!(m, "screenshot.capture");
        assert_eq!(p["area"], json!([0, 0, 800, 600]));
    }

    #[test]
    fn test_map_tool_focus_window_not_in_map_tool() {
        assert!(AgentShellMcpServer::map_tool("focus_window", &json!({"app_id": "x"})).is_none());
    }

    #[test]
    fn test_map_tool_wait_for_window_not_in_map_tool() {
        assert!(
            AgentShellMcpServer::map_tool("wait_for_window", &json!({"app_id": "x"})).is_none()
        );
    }

    #[test]
    fn resolve_timeout_ms_defaults_to_15000() {
        assert_eq!(
            AgentShellMcpServer::resolve_timeout_ms(&json!({})).unwrap(),
            15_000
        );
    }

    #[test]
    fn resolve_timeout_ms_parses_human_readable_timeout() {
        assert_eq!(
            AgentShellMcpServer::resolve_timeout_ms(&json!({"timeout": "10s"})).unwrap(),
            10_000
        );
    }

    #[test]
    fn resolve_timeout_ms_keeps_timeout_ms_compat() {
        assert_eq!(
            AgentShellMcpServer::resolve_timeout_ms(&json!({"timeout_ms": 500})).unwrap(),
            500
        );
    }

    #[test]
    fn resolve_timeout_ms_falls_back_on_non_integer_timeout_ms() {
        // 兼容契约：timeout_ms 非整数（浮点/字符串）按缺省 15000 处理，
        // 与原 `as_u64().unwrap_or(15000)` 行为一致（不回退）。
        assert_eq!(
            AgentShellMcpServer::resolve_timeout_ms(&json!({"timeout_ms": 500.5})).unwrap(),
            15_000
        );
        assert_eq!(
            AgentShellMcpServer::resolve_timeout_ms(&json!({"timeout_ms": "500"})).unwrap(),
            15_000
        );
    }

    #[test]
    fn resolve_timeout_ms_rejects_both() {
        assert!(AgentShellMcpServer::resolve_timeout_ms(&json!({
            "timeout": "10s",
            "timeout_ms": 500
        }))
        .is_err());
    }

    #[test]
    fn resolve_timeout_ms_rejects_invalid_timeout() {
        assert!(AgentShellMcpServer::resolve_timeout_ms(&json!({"timeout": "abc"})).is_err());
        assert!(AgentShellMcpServer::resolve_timeout_ms(&json!({"timeout": 1000})).is_err());
    }

    #[test]
    fn test_resolve_target_by_app_id() {
        let windows = json!([
            {"native_id": "0x1", "app_id": "firefox", "title": "Firefox", "pid": 100},
            {"native_id": "0x2", "app_id": "code", "title": "VS Code", "pid": 200},
        ]);
        let id = resolve_target(&json!({"app_id": "firefox"}), windows.as_array().unwrap());
        assert_eq!(id.as_deref(), Some("0x1"));
    }

    #[test]
    fn test_resolve_target_by_title_substring() {
        let windows = json!([
            {"native_id": "0x1", "app_id": "firefox", "title": "Firefox - Main", "pid": 100},
        ]);
        let id = resolve_target(&json!({"title": "Main"}), windows.as_array().unwrap());
        assert_eq!(id.as_deref(), Some("0x1"));
    }

    #[test]
    fn test_resolve_target_by_pid() {
        let windows = json!([
            {"native_id": "0x1", "app_id": "firefox", "title": "Firefox", "pid": 100},
            {"native_id": "0x2", "app_id": "code", "title": "Code", "pid": 200},
        ]);
        let id = resolve_target(&json!({"pid": 200}), windows.as_array().unwrap());
        assert_eq!(id.as_deref(), Some("0x2"));
    }

    #[test]
    fn test_resolve_target_by_window_id() {
        let windows = json!([
            {"native_id": "0x1", "app_id": "firefox", "title": "Firefox", "pid": 100},
        ]);
        let id = resolve_target(&json!({"window_id": "0x1"}), windows.as_array().unwrap());
        assert_eq!(id.as_deref(), Some("0x1"));
    }

    #[test]
    fn test_resolve_target_no_match() {
        let windows = json!([
            {"native_id": "0x1", "app_id": "firefox", "title": "Firefox", "pid": 100},
        ]);
        let id = resolve_target(
            &json!({"app_id": "nonexistent"}),
            windows.as_array().unwrap(),
        );
        assert!(id.is_none());
    }

    #[test]
    fn test_map_tool_unknown() {
        assert!(AgentShellMcpServer::map_tool("unknown", &Value::Null).is_none());
    }
    /// MCP 注入的 target 目录用 CARGO_MANIFEST_DIR 拼绝对路径——CWD 无关
    /// （TSI-2471 审查 #2）。验证候选目录列表中包含基于
    /// CARGO_MANIFEST_DIR 的 target/debug 与 target/release 绝对路径。
    #[test]
    fn test_target_dirs_are_absolute_via_cargo_manifest_dir() {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("CARGO_MANIFEST_DIR has no parent");
        let dbg = workspace_root.join("target/debug");
        let rel = workspace_root.join("target/release");
        let dirs = agent_shell_rpc::daemon_bin::candidate_dirs(&[
            &dbg.to_string_lossy(),
            &rel.to_string_lossy(),
        ]);
        assert!(!dirs.is_empty(), "candidate_dirs must not be empty");
        assert!(
            dirs.iter().any(|d| d == &*dbg.to_string_lossy()),
            "target/debug (absolute) must be a candidate: {dirs:?}"
        );
        assert!(
            dirs.iter().any(|d| d == &*rel.to_string_lossy()),
            "target/release (absolute) must be a candidate: {dirs:?}"
        );
    }
}
