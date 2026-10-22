//! MCP server 实现——18 tools registered via rmcp ServerHandler。
//!
//! 工具调用经 stdio JSON-RPC 转发到 agent-shell daemon。daemon 在 MCP server
//! 构造时 spawn 一次并保持长连接，避免每次调用重新 fork/exec。

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
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
    /// 定位并 spawn `agent-shell-daemon`，返回长连接。
    fn connect() -> Result<Self, String> {
        let bin = find_daemon_binary()?;
        let mut child = Command::new(&bin)
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
                "列出窗口，含 app_id/title/pid/geometry/workspace/stacking_order/states/window_type/keep_above；可按 app_id 或工作区原生标识过滤",
                json!({
                    "type": "object",
                    "properties": {
                        "app_id": {"type": "string", "description": "应用标识过滤（子串匹配，与命令面 windows list 一致）"},
                        "workspace": {"type": "string", "description": "工作区原生标识（windows list 的 workspace 列，与 workspaces list 的 id 同源），精确匹配"}
                    }
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
                "语义化点击元素：按无障碍角色+名称定位并触发动作接口；定位不到或无障碍不可用时按 fallback_coordinate 坐标点击",
                json!({
                    "type": "object",
                    "properties": {
                        "role": {"type":"string","enum":["button","menu_item","checkbox"],"description": "无障碍角色（AT-SPI 角色名归一匹配）"},
                        "name": {"type":"string","description": "无障碍名称（精确匹配）"},
                        "fallback_coordinate": {"type":"array","items":{"type":"integer"},"minItems":2,"maxItems":2,"description": "坐标兜底 [x, y]：元素未命中或无障碍不可达时使用"}
                    },
                    "anyOf": [{"required":["role"]},{"required":["name"]}]
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
                        "timeout_ms": {"type":"integer","minimum":0,"default":15000, "description": "超时毫秒（默认 15000）；已被 `timeout` 取代，保留兼容"}
                    },
                    "required": ["app_id"]
                }),
            ),
            make_tool(
                "query_a11y_elements",
                "查询无障碍元素：按角色、按名称或全树（all=true）三种方式，角色与名称可组合；结果含 count 与 elements（bus_name/path/name/role/role_code/states）",
                json!({
                    "type": "object",
                    "properties": {
                        "role": {"type":"string", "description": "角色过滤（AT-SPI 角色名归一匹配，如 button/menu item/checkbox）"},
                        "name": {"type":"string", "description": "名称过滤（精确匹配）"},
                        "all": {"type":"boolean","default":false, "description": "全树查询开关：无 role/name 时必须为 true；提供过滤条件时仍按条件过滤"}
                    }
                }),
            ),
            make_tool(
                "subscribe_events",
                "订阅桌面事件（filter 取值见事件词表：类别名如 window/workspace，或事件名如 WindowOpened）。MCP 无流式通知通道，仅返回订阅句柄（subscriber_id），事件不投递到本连接",
                json!({
                    "type": "object",
                    "properties": {
                        "events": {"type":"array","items":{"type":"string"}, "description": "事件过滤词表（逗号连接后作为订阅过滤器）；缺省订阅全部"}
                    }
                }),
            ),
            // Extension 11 tools (§21.35) — total 18
            make_tool(
                "security_check",
                "检查某操作是否被授权：返回安全策略真值；可选 operation 附带该操作名的判定（allow/confirm/deny，与门禁同源）",
                json!({"type":"object","properties":{"operation":{"type":"string","description":"操作名（如 screenshot.capture / windows.focus / mount.mount）；省略则只返回策略真值"}}}),
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
    /// MCP 工具 schema（§17.3）与 daemon RPC 参数契约不同——此函数完成从
    /// MCP 参数到 RPC 参数的转换。需要多步 RPC 的工具（定位后再操作、参数
    /// 组合校验）返回 `Ok(None)`，由 [`Self::execute_tool`] 分派；参数本身
    /// 不合法返回 `Err`（不静默丢弃声明过的入参）。
    fn map_tool(tool_name: &str, args: &Value) -> Result<Option<(&'static str, Value)>, String> {
        let mapped = match tool_name {
            "list_windows" => {
                // app_id → 命令面同名的 filter；workspace → 服务侧工作区谓词。
                // 原值转发：类型校验由服务端统一执行（不在投射层静默丢弃）。
                let filter = args.get("app_id").cloned().unwrap_or(Value::Null);
                let workspace = args.get("workspace").cloned().unwrap_or(Value::Null);
                (
                    "windows.list",
                    json!({ "filter": filter, "workspace": workspace }),
                )
            }
            "focus_window" | "wait_for_window" | "click_element" | "screenshot" => {
                // 多步解析或入参组合校验：由 execute_tool 处理。
                return Ok(None);
            }
            "query_a11y_elements" => {
                // 三个入参原样转发：daemon 侧负责类型校验与「必须有 role/name 或 all」。
                let p = json!({
                    "role": args.get("role"),
                    "name": args.get("name"),
                    "all": args.get("all"),
                });
                ("a11y.query", p)
            }
            "subscribe_events" => {
                let filter = event_filter(args)?;
                ("events.subscribe", json!({ "filter": filter }))
            }
            "security_check" => {
                let operation = args.get("operation").cloned().unwrap_or(Value::Null);
                ("security.status", json!({ "operation": operation }))
            }
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
            _ => return Err(format!("unknown tool: {tool_name}")),
        };
        Ok(Some(mapped))
    }

    /// 执行工具调用——处理需要多步 RPC 的工具（如 focus_window 先解析再操作）。
    async fn execute_tool(&self, tool_name: &str, args: &Value) -> Result<Value, String> {
        match tool_name {
            "focus_window" => self.execute_focus_window(args).await,
            "wait_for_window" => self.execute_wait_for_window(args).await,
            "click_element" => self.execute_click_element(args).await,
            "screenshot" => self.execute_screenshot(args).await,
            _ => {
                let (method, params) = match Self::map_tool(tool_name, args)? {
                    Some(m) => m,
                    None => return Err(format!("unknown tool: {tool_name}")),
                };
                self.conn.lock().await.call(method, params).await
            }
        }
    }

    /// click_element：`a11y.query` 按 role/name 定位 → `a11y.action` 触发动作接口。
    ///
    /// 与命令面同语义：零命中或无障碍不可达时用 `fallback_coordinate` 坐标
    /// 点击兜底（无兜底则如实报错）；多命中不猜——报歧义并要求收窄条件
    /// （契约：目标解析多命中必须报错）。
    async fn execute_click_element(&self, args: &Value) -> Result<Value, String> {
        let role = args.get("role").and_then(Value::as_str);
        let name = args.get("name").and_then(Value::as_str);
        if role.is_none() && name.is_none() {
            return Err("click_element requires `role` and/or `name`".to_string());
        }
        let fallback = parse_fallback_coordinate(args)?;

        let located = {
            let mut conn = self.conn.lock().await;
            conn.call(
                "a11y.query",
                json!({ "role": role, "name": name, "all": false }),
            )
            .await
        };
        let elements = match located {
            Ok(value) => value
                .get("elements")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            Err(e) => {
                // 无障碍不可达：有坐标兜底就走兜底，否则原样上抛（错误码不降级）。
                return match fallback {
                    Some((x, y)) => self.click_at(x, y).await,
                    None => Err(e),
                };
            }
        };
        if elements.is_empty() {
            return match fallback {
                Some((x, y)) => self.click_at(x, y).await,
                None => Err(format!(
                    "a11y element not found (role={role:?}, name={name:?}); \
                     pass `fallback_coordinate` for a coordinate click"
                )),
            };
        }
        if elements.len() > 1 {
            let candidates: Vec<String> = elements
                .iter()
                .take(5)
                .map(|el| {
                    format!(
                        "{:?} ({}) at {}",
                        el.get("name").and_then(Value::as_str).unwrap_or("?"),
                        el.get("role").and_then(Value::as_str).unwrap_or("?"),
                        el.get("path").and_then(Value::as_str).unwrap_or("?"),
                    )
                })
                .collect();
            return Err(format!(
                "a11y element ambiguous: {} matches for role={role:?}, name={name:?}; \
                 narrow `role`/`name`; candidates: {}",
                elements.len(),
                candidates.join(", ")
            ));
        }
        let element = &elements[0];
        let path = element
            .get("path")
            .and_then(Value::as_str)
            .ok_or("malformed a11y.query element: missing path")?;
        let bus = element.get("bus_name").and_then(Value::as_str);
        let action = {
            let mut conn = self.conn.lock().await;
            conn.call("a11y.action", json!({ "path": path, "bus": bus }))
                .await
        };
        match action {
            Ok(value) => Ok(value),
            Err(e) => match fallback {
                Some((x, y)) => self.click_at(x, y).await,
                None => Err(e),
            },
        }
    }

    /// 坐标兜底点击（`input.send` click，左键 + 绝对坐标）；结果原样返回。
    async fn click_at(&self, x: i64, y: i64) -> Result<Value, String> {
        let mut conn = self.conn.lock().await;
        conn.call(
            "input.send",
            json!({ "kind": "click", "payload": { "button": "left", "at": [x, y] } }),
        )
        .await
    }

    /// screenshot：`target` 选择器决定实际捕获范围（全屏 / 窗口 / 区域）。
    ///
    /// 选择器与其它入参冲突时报错，不静默丢弃——否则 `target` 形同虚设。
    async fn execute_screenshot(&self, args: &Value) -> Result<Value, String> {
        let params = screenshot_params(args)?;
        let mut conn = self.conn.lock().await;
        conn.call("screenshot.capture", params).await
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
    /// `--timeout` 的 `agent_shell_rpc::duration::parse_duration` 语义。
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
        // timeout_ms 为负整数时显式报错（schema 亦声明 minimum:0）；非整数
        // （浮点/字符串）仍按缺省 15000 处理，与原 as_u64().unwrap_or(15000)
        // 行为一致（不回退）。
        if let Some(n) = timeout_ms.and_then(|v| v.as_i64()) {
            if n < 0 {
                return Err("timeout_ms 必须为非负整数".to_string());
            }
        }
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
        server_info()
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

/// 握手信息：指令字段 + 工具能力声明。
///
/// 工具能力必须显式声明——调用方据此判定 `tools/list` 与 `tools/call` 可用；
/// 空 capabilities 会被读成「本服务不提供工具」。
fn server_info() -> ServerInfo {
    let mut info = ServerInfo::default();
    info.instructions = Some("agent-shell MCP server for Linux desktop automation".into());
    info.capabilities = ServerCapabilities::builder().enable_tools().build();
    info
}

/// `subscribe_events.events` → daemon `events.subscribe` 的 `filter` 串。
///
/// 词表值由 daemon 校验（非法值返回 InvalidParams 并列出全部合法取值）；
/// 空数组与缺省同义：订阅全部事件。
fn event_filter(args: &Value) -> Result<Option<String>, String> {
    let Some(value) = args.get("events").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .ok_or("subscribe_events events must be an array of strings")?;
    let mut tokens = Vec::with_capacity(items.len());
    for item in items {
        tokens.push(
            item.as_str()
                .ok_or("subscribe_events events must be an array of strings")?,
        );
    }
    Ok((!tokens.is_empty()).then(|| tokens.join(",")))
}

/// `screenshot.area` → `[x, y, width, height]`。
///
/// 非 4 元、非整数或超出 32 位像素域一律报错——坏载荷不得被静默忽略后
/// 变成另一种捕获范围。
fn parse_area(args: &Value) -> Result<Option<[i32; 4]>, String> {
    let Some(value) = args.get("area").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .ok_or("screenshot area must be [x, y, width, height]")?;
    if items.len() != 4 {
        return Err("screenshot area must be [x, y, width, height]".to_string());
    }
    let mut out = [0i32; 4];
    for (slot, item) in out.iter_mut().zip(items) {
        let value = item
            .as_i64()
            .ok_or("screenshot area components must be integers")?;
        *slot = i32::try_from(value)
            .map_err(|_| "screenshot area component out of 32-bit pixel range".to_string())?;
    }
    Ok(Some(out))
}

/// `screenshot.target` 选择器语义 → `screenshot.capture` 参数。
///
/// `target` 显式给出时与 window/area 的组合必须自洽，冲突即报错；缺省时按
/// 已给入参推断（区域 → 区域捕获，窗口 → 窗口捕获，否则全屏）。
fn screenshot_params(args: &Value) -> Result<Value, String> {
    let window = args.get("window").and_then(Value::as_str);
    let area = parse_area(args)?;
    let target = match args.get("target") {
        None | Some(Value::Null) => match (area, window) {
            (Some(_), _) => "area",
            (None, Some(_)) => "window",
            (None, None) => "screen",
        },
        Some(Value::String(s)) => match s.as_str() {
            "screen" | "window" | "area" => s.as_str(),
            other => {
                return Err(format!(
                    "unknown screenshot target: {other} (expect screen|window|area)"
                ))
            }
        },
        Some(_) => return Err("screenshot target must be a string".to_string()),
    };
    let (window, area) = match target {
        "screen" => {
            if window.is_some() || area.is_some() {
                return Err(
                    "screenshot target=screen takes neither `window` nor `area`".to_string()
                );
            }
            (None, None)
        }
        "window" => {
            let window = window.ok_or("screenshot target=window requires `window`")?;
            if area.is_some() {
                return Err("screenshot target=window takes `window` but not `area`".to_string());
            }
            (Some(window), None)
        }
        _ => {
            let area = area.ok_or("screenshot target=area requires `area`")?;
            if window.is_some() {
                return Err("screenshot target=area takes `area` but not `window`".to_string());
            }
            (None, Some(area))
        }
    };
    let output_path = format!("/tmp/agent-shell-mcp-{}.ppm", std::process::id());
    Ok(json!({ "window": window, "area": area, "output_path": output_path }))
}

/// `click_element.fallback_coordinate` → `[x, y]`。
fn parse_fallback_coordinate(args: &Value) -> Result<Option<(i64, i64)>, String> {
    let Some(value) = args.get("fallback_coordinate").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .ok_or("fallback_coordinate must be [x, y] integers")?;
    if items.len() != 2 {
        return Err("fallback_coordinate must be [x, y] integers".to_string());
    }
    let (Some(x), Some(y)) = (items[0].as_i64(), items[1].as_i64()) else {
        return Err("fallback_coordinate must be [x, y] integers".to_string());
    };
    Ok(Some((x, y)))
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
/// workspace target 目录用 `CARGO_MANIFEST_DIR` 拼绝对路径，不依赖 CWD。
/// MCP server 在 mcp/ 子 crate，向上一级是 workspace 根，target/ 在根下。
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
        assert!(names.contains(&"click_element"));
        assert!(names.contains(&"screenshot"));
        assert!(names.contains(&"query_a11y_elements"));
        assert!(names.contains(&"ime_type"));
        assert!(names.contains(&"daemon_status"));
        assert!(names.contains(&"subscribe_events"));
        assert!(names.contains(&"security_check"));
    }

    #[test]
    fn initialize_declares_tools_capability() {
        // 握手必须声明工具能力：空 capabilities 会被读成「不提供工具」。
        let info = server_info();
        assert!(
            info.capabilities.tools.is_some(),
            "initialize must declare `capabilities.tools`"
        );
        let wire = serde_json::to_value(&info).expect("ServerInfo serializable");
        assert!(
            wire.get("capabilities")
                .and_then(|c| c.get("tools"))
                .is_some(),
            "wire capabilities must carry `tools`: {wire}"
        );
    }

    #[test]
    fn test_wait_for_window_timeout_ms_schema_minimum_zero() {
        // 契约声明：timeout_ms 下限为 0，负整数在 MCP schema 层即被拒绝。
        let tools = AgentShellMcpServer::build_tools();
        let tool = tools
            .iter()
            .find(|t| t.name.as_ref() == "wait_for_window")
            .expect("wait_for_window tool registered");
        let timeout_ms = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.get("timeout_ms"))
            .expect("wait_for_window schema has timeout_ms");
        assert_eq!(timeout_ms.get("minimum").and_then(|v| v.as_i64()), Some(0));
    }

    #[test]
    fn test_map_tool_list_windows() {
        let (m, p) = AgentShellMcpServer::map_tool(
            "list_windows",
            &json!({"app_id": "firefox", "workspace": "3"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(m, "windows.list");
        assert_eq!(p["filter"], "firefox");
        assert_eq!(p["workspace"], "3", "workspace must reach the service");
    }

    #[test]
    fn test_map_tool_query_a11y_elements_forwards_three_inputs() {
        let (m, p) = AgentShellMcpServer::map_tool(
            "query_a11y_elements",
            &json!({"role": "button", "name": "OK", "all": true}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(m, "a11y.query");
        assert_eq!(p["role"], "button");
        assert_eq!(p["name"], "OK");
        assert_eq!(p["all"], true);
    }

    #[test]
    fn test_map_tool_subscribe_events_filters() {
        let (m, p) = AgentShellMcpServer::map_tool(
            "subscribe_events",
            &json!({"events": ["WindowOpened", "workspace"]}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(m, "events.subscribe");
        assert_eq!(p["filter"], "WindowOpened,workspace");

        // 缺省/空数组 = 订阅全部：filter 为空，daemon 视为 EventFilter::all。
        let (_, p) = AgentShellMcpServer::map_tool("subscribe_events", &json!({"events": []}))
            .unwrap()
            .unwrap();
        assert_eq!(p["filter"], Value::Null);
    }

    #[test]
    fn test_map_tool_subscribe_events_rejects_non_strings() {
        let args = json!({"events": [1]});
        assert!(AgentShellMcpServer::map_tool("subscribe_events", &args).is_err());
    }

    #[test]
    fn test_map_tool_security_check_forwards_operation() {
        let (m, p) = AgentShellMcpServer::map_tool(
            "security_check",
            &json!({"operation": "screenshot.capture"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(m, "security.status");
        assert_eq!(p["operation"], "screenshot.capture");
    }

    #[test]
    fn test_screenshot_params_target_selects_capture_scope() {
        // target=screen：不含 window/area。
        let p = screenshot_params(&json!({"target": "screen"})).unwrap();
        assert!(p["window"].is_null());
        assert!(p["area"].is_null());
        assert!(p["output_path"].as_str().is_some(), "must have output_path");

        // target=window：window 生效，area 不出现。
        let p = screenshot_params(&json!({"target": "window", "window": "1234"})).unwrap();
        assert_eq!(p["window"], "1234");
        assert!(p["area"].is_null());

        // target=area：area 生效，window 不出现。
        let p = screenshot_params(&json!({"target": "area", "area": [0, 0, 800, 600]})).unwrap();
        assert_eq!(p["area"], json!([0, 0, 800, 600]));
        assert!(p["window"].is_null());

        // 缺省 target：按入参推断。
        let p = screenshot_params(&json!({"area": [1, 2, 3, 4]})).unwrap();
        assert_eq!(p["area"], json!([1, 2, 3, 4]));
        let p = screenshot_params(&json!({"window": "42"})).unwrap();
        assert_eq!(p["window"], "42");
    }

    #[test]
    fn test_screenshot_params_rejects_conflicting_selector() {
        // target 与选择器组合矛盾时必须报错，不得静默忽略。
        assert!(screenshot_params(&json!({"target": "screen", "window": "1"})).is_err());
        assert!(screenshot_params(&json!({"target": "screen", "area": [0, 0, 1, 1]})).is_err());
        assert!(screenshot_params(&json!({"target": "window"})).is_err());
        assert!(screenshot_params(
            &json!({"target": "window", "window": "1", "area": [0, 0, 1, 1]})
        )
        .is_err());
        assert!(screenshot_params(&json!({"target": "area"})).is_err());
        assert!(
            screenshot_params(&json!({"target": "area", "area": [0, 0, 1, 1], "window": "1"}))
                .is_err()
        );
        assert!(screenshot_params(&json!({"target": "fullscreen"})).is_err());
        // 区域形状不合法不得被静默丢弃（旧实现 filter_map 会丢分量）。
        assert!(screenshot_params(&json!({"target": "area", "area": [0, 0, 1]})).is_err());
        assert!(screenshot_params(&json!({"target": "area", "area": [0, 0, 1, "x"]})).is_err());
    }

    #[test]
    fn test_fallback_coordinate_shape() {
        assert_eq!(
            parse_fallback_coordinate(&json!({"fallback_coordinate": [10, 20]})).unwrap(),
            Some((10, 20))
        );
        assert_eq!(parse_fallback_coordinate(&json!({})).unwrap(), None);
        assert!(parse_fallback_coordinate(&json!({"fallback_coordinate": [1]})).is_err());
        assert!(parse_fallback_coordinate(&json!({"fallback_coordinate": [1, 2, 3]})).is_err());
    }

    #[test]
    fn test_map_tool_multi_step_tools_are_not_single_step() {
        for tool in [
            "focus_window",
            "wait_for_window",
            "click_element",
            "screenshot",
        ] {
            assert!(
                AgentShellMcpServer::map_tool(tool, &json!({"app_id": "x"}))
                    .unwrap()
                    .is_none(),
                "{tool} must be dispatched by execute_tool"
            );
        }
    }

    #[test]
    fn test_map_tool_unknown() {
        assert!(AgentShellMcpServer::map_tool("unknown", &Value::Null).is_err());
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
    fn resolve_timeout_ms_rejects_negative_timeout_ms() {
        // 负整数不再静默回落 15000：schema 声明 minimum:0，此处显式报错。
        assert!(AgentShellMcpServer::resolve_timeout_ms(&json!({"timeout_ms": -5})).is_err());
        assert!(AgentShellMcpServer::resolve_timeout_ms(&json!({"timeout_ms": -1})).is_err());
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

    /// MCP 注入的 target 目录用 CARGO_MANIFEST_DIR 拼绝对路径——CWD 无关。
    /// 验证候选目录列表中包含基于 CARGO_MANIFEST_DIR 的 target/debug 与
    /// target/release 绝对路径。
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
