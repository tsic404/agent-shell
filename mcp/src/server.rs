//! MCP server 实现——18 tools registered via rmcp ServerHandler。
//!
//! 工具调用经 stdio JSON-RPC 转发到 agent-shell daemon。连接选择与命令面同一条
//! 规则（§22.2 激活策略）：`AGENT_SHELL_SOCKET` 给出显式端点时直连常驻实例，
//! 否则派生瞬态 daemon；每个 MCP 进程同时只保持一条连接，复用于每次工具调用。

use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_shell_rpc::RpcErrorCode;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{serve_server, RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{error, info};

/// 单次请求的上限：端点连接（含对端不响应）与首个应答行各用同一上限。
///
/// 连接也受同一上限约束——调用方持着连接锁，端点卡住时不得让后续调用整体无响应。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// 派生形态保留的 daemon stderr 上限（字节）。进程在应答前退出时，尾部输出
/// 就是根因（如单实例锁竞争的 `daemon already running`），不得丢弃。
const STDERR_TAIL_CAP: usize = 64 * 1024;

/// 工具调用的失败来源，决定错误码归属。
///
/// 远程方法错误保留 daemon 给出的错误码（C-013 投影不变量）；连接层失败没有
/// 远程码，按项目段取值——daemon 未运行/派生失败取 1001，已连接却无响应的
/// 后端失败取 1005。两者都不得退化成裸 `timeout`。
#[derive(Debug)]
enum CallError {
    /// daemon 返回的 JSON-RPC 错误（项目码或规范码）。
    Rpc { code: i32, message: String },
    /// 传输不可用（派生/连接失败、进程提前退出、无响应）。
    Transport { code: i32, message: String },
    /// 未经远程方法的本地失败（未知工具、入参适配）。
    Local(String),
}

impl CallError {
    fn transport(code: RpcErrorCode, message: impl Into<String>) -> Self {
        Self::Transport {
            code: code as i32,
            message: message.into(),
        }
    }

    fn local(message: impl Into<String>) -> Self {
        Self::Local(message.into())
    }

    fn is_transport(&self) -> bool {
        matches!(self, Self::Transport { .. })
    }

    /// MCP 错误内容行：带码的失败保持 `RPC error <code>: <message>` 形状。
    fn render(&self) -> String {
        match self {
            Self::Rpc { code, message } | Self::Transport { code, message } => {
                format!("RPC error {code}: {message}")
            }
            Self::Local(message) => message.clone(),
        }
    }
}

/// daemon 传输：显式端点上的常驻实例，或派生的瞬态子进程。
struct DaemonConnection {
    /// 派生形态的子进程；显式端点形态为 None（daemon 不由本进程拉起）。
    child: Option<Child>,
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    /// 派生形态的 stderr 尾部；显式端点形态无 stderr 管道。
    stderr_tail: Option<Arc<Mutex<VecDeque<u8>>>>,
    next_id: u64,
    /// 诊断用的端点描述：错误信息必须指认本连接连到哪。
    endpoint: String,
}

impl DaemonConnection {
    /// 建立连接：显式端点优先，否则派生瞬态 daemon。
    async fn connect() -> Result<Self, CallError> {
        match socket_endpoint() {
            Some(path) => Self::connect_socket(&path).await,
            None => Self::connect_spawned().await,
        }
    }

    /// 直连显式端点。连不上不回退派生：端点由操作方指定，回退会把「常驻实例
    /// 没起来」伪装成一次成功的临时实例调用。
    async fn connect_socket(path: &Path) -> Result<Self, CallError> {
        let unavailable =
            |detail: String| CallError::transport(RpcErrorCode::DaemonUnavailable, detail);
        // tokio 的 connect 是「非阻塞 connect + 等可写」：backlog 满或端点进程被
        // SIGSTOP 时永不就绪。调用方持着连接锁，不设上限就会让此后每次 tools/call
        // 整体无响应且没有错误行——上限与应答死线同值，超时按 1001 显式失败。
        let stream = tokio::time::timeout(RESPONSE_TIMEOUT, UnixStream::connect(path))
            .await
            .map_err(|_| {
                unavailable(format!(
                    "cannot connect to daemon socket {} within {}s (AGENT_SHELL_SOCKET names \
                     the resident endpoint; drop it to auto-spawn a transient daemon)",
                    path.display(),
                    RESPONSE_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|e| {
                unavailable(format!(
                    "cannot connect to daemon socket {}: {e} (AGENT_SHELL_SOCKET names the \
                     resident endpoint; drop it to auto-spawn a transient daemon)",
                    path.display()
                ))
            })?;
        let peer = stream.peer_cred().map_err(|e| {
            unavailable(format!(
                "cannot read peer credentials of {}: {e}",
                path.display()
            ))
        })?;
        agent_shell_rpc::endpoint::verify_peer_uid(
            path,
            peer.uid(),
            agent_shell_rpc::endpoint::current_uid(),
        )
        .map_err(|detail| CallError::transport(RpcErrorCode::DaemonUnavailable, detail))?;
        let (read, write) = stream.into_split();
        Ok(Self {
            child: None,
            stdin: Box::new(write),
            reader: BufReader::new(Box::new(read)),
            stderr_tail: None,
            next_id: 1,
            endpoint: format!("socket {}", path.display()),
        })
    }

    /// 自动激活：派生瞬态 daemon（stdio 承载 JSON-RPC），stderr 收尾部供诊断。
    async fn connect_spawned() -> Result<Self, CallError> {
        let unavailable =
            |detail: String| CallError::transport(RpcErrorCode::DaemonUnavailable, detail);
        let bin = find_daemon_binary().map_err(unavailable)?;
        let mut child = Command::new(&bin)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| unavailable(format!("spawn {bin}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| unavailable(format!("{bin} has no stdin")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| unavailable(format!("{bin} has no stdout")))?;
        let stderr_tail = child.stderr.take().map(spawn_stderr_drain);
        Ok(Self {
            child: Some(child),
            stdin: Box::new(stdin),
            reader: BufReader::new(Box::new(stdout)),
            stderr_tail,
            next_id: 1,
            endpoint: format!("spawned {bin}"),
        })
    }

    /// 复用连接发送一条 JSON-RPC 请求并读取对应响应。
    ///
    /// 事件订阅生效后，daemon 会在同一 stdio 行流上推送 `events.notify`
    /// 通知（无 `id`）。本方法循环读取并按 `id` 有无区分：通知直接跳过，
    /// 直到拿到匹配本请求 id 的响应——避免后续任何工具调用把通知误解析
    /// 为响应。
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = agent_shell_rpc::Request::new(id, method, params);
        let line = req.to_line();
        let endpoint = self.endpoint.clone();
        let spawned = self.child.is_some();

        self.stdin.write_all(line.as_bytes()).await.map_err(|e| {
            CallError::transport(
                RpcErrorCode::DaemonUnavailable,
                format!("write to {endpoint}: {e}"),
            )
        })?;
        self.stdin.flush().await.map_err(|e| {
            CallError::transport(
                RpcErrorCode::DaemonUnavailable,
                format!("flush to {endpoint}: {e}"),
            )
        })?;

        // 仅首个读使用死线；订阅激活期间 `events.notify` 会持续流入，每次读到
        // 通知后重入循环即重置死线，避免后续工具调用因活跃订阅误报超时。
        let mut first = true;
        loop {
            let mut buf = String::new();
            let read = if first {
                first = false;
                tokio::time::timeout(RESPONSE_TIMEOUT, self.reader.read_line(&mut buf))
                    .await
                    .map_err(|_| no_response_error(&endpoint, spawned, method))?
            } else {
                self.reader.read_line(&mut buf).await
            }
            .map_err(|e| {
                CallError::transport(
                    RpcErrorCode::BackendError,
                    format!("read from {endpoint}: {e}"),
                )
            })?;
            if read == 0 {
                // 派生形态下 stdout 随子进程退出而关闭，EOF 即「应答前退出」；
                // 不再另起 select 观察进程退出，避免已入管道但未成行的应答被丢弃。
                let status = match self.child.as_mut() {
                    Some(child) => child.try_wait().ok().flatten(),
                    None => None,
                };
                return Err(closed_error(&endpoint, &self.stderr_tail, status).await);
            }
            let msg: Value = serde_json::from_str(&buf).map_err(|e| {
                CallError::transport(
                    RpcErrorCode::BackendError,
                    format!("malformed response line from {endpoint}: {e}"),
                )
            })?;
            if msg.get("id").is_none() {
                // events.notify 通知：不属于本请求，继续等待响应。
                continue;
            }
            let resp = agent_shell_rpc::Response::from_line(&buf).map_err(|e| {
                CallError::transport(
                    RpcErrorCode::BackendError,
                    format!("malformed response line from {endpoint}: {e}"),
                )
            })?;
            if resp.id != id {
                return Err(CallError::transport(
                    RpcErrorCode::BackendError,
                    format!(
                        "response id mismatch from {endpoint}: got {} want {}",
                        resp.id, id
                    ),
                ));
            }
            if let Some(err) = resp.error {
                return Err(CallError::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            return resp.result.ok_or_else(|| {
                CallError::transport(
                    RpcErrorCode::BackendError,
                    format!("empty result from {endpoint} for {method}"),
                )
            });
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

/// 连接在应答前结束（EOF）：daemon 退出或端点对端关闭。
///
/// 派生形态 stdout 随子进程退出而关闭，`try_wait` 通常已能给出退出状态；未收割到
/// 时只少一行状态，不改变「显式失败 + stderr 根因」的语义。
async fn closed_error(
    endpoint: &str,
    tail: &Option<Arc<Mutex<VecDeque<u8>>>>,
    status: Option<std::process::ExitStatus>,
) -> CallError {
    let detail = stderr_tail_text(tail).await;
    let base = match status {
        Some(status) => format!("daemon ({endpoint}) exited before responding ({status})"),
        None => format!("daemon ({endpoint}) closed the connection before responding"),
    };
    CallError::transport(RpcErrorCode::DaemonUnavailable, with_stderr(base, &detail))
}

/// 追加 stderr 尾部：空输出不加分隔符，错误行保持单行。
fn with_stderr(mut message: String, detail: &str) -> String {
    if !detail.is_empty() {
        message.push_str("; daemon stderr: ");
        message.push_str(detail);
    }
    message
}

/// 派生 daemon 的 stderr 尾部缓冲：后台任务持续排空管道（管道写满会让 daemon
/// 阻塞），只保留最近 [`STDERR_TAIL_CAP`] 字节供退出诊断。
fn spawn_stderr_drain(mut stderr: tokio::process::ChildStderr) -> Arc<Mutex<VecDeque<u8>>> {
    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_CAP)));
    let shared = Arc::clone(&tail);
    tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut buf = shared.lock().await;
                    buf.extend(&chunk[..n]);
                    if buf.len() > STDERR_TAIL_CAP {
                        let excess = buf.len() - STDERR_TAIL_CAP;
                        buf.drain(..excess);
                    }
                }
            }
        }
    });
    tail
}

/// 错误行里引用的 stderr 末段上限与行数上限。
///
/// 缓冲本身按 [`STDERR_TAIL_CAP`] 保留（诊断需要），但错误信息面向用户：一次工具
/// 失败不得变成上千行日志，只引用最后几行。
const STDERR_ERROR_BYTES: usize = 2 * 1024;
const STDERR_ERROR_LINES: usize = 4;

/// 尾部缓冲的可读文本（无缓冲或空输出时为空串）。
///
/// 只取最后 [`STDERR_ERROR_BYTES`] 字节与最后 [`STDERR_ERROR_LINES`] 行，换行折叠为
/// ` | `，错误行保持单行可读。
async fn stderr_tail_text(tail: &Option<Arc<Mutex<VecDeque<u8>>>>) -> String {
    let Some(tail) = tail else {
        return String::new();
    };
    let buf = tail.lock().await;
    let start = buf.len().saturating_sub(STDERR_ERROR_BYTES);
    let bytes: Vec<u8> = buf.iter().skip(start).copied().collect();
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let kept = &lines[lines.len().saturating_sub(STDERR_ERROR_LINES)..];
    kept.join(" | ")
}

/// 首个应答行超时：派生形态下把单实例锁等待这一常见根因写进错误，操作方据此
/// 改为接入常驻实例，而不是把上限当修复。
fn no_response_error(endpoint: &str, spawned: bool, method: &str) -> CallError {
    let hint = if spawned {
        "; a spawned daemon waits up to 30s on the session single-instance lock when a \
         resident daemon holds it — point AGENT_SHELL_SOCKET at the resident endpoint to reuse it"
    } else {
        ""
    };
    CallError::transport(
        RpcErrorCode::BackendError,
        format!(
            "no response from {endpoint} within {}s for {method}{hint}",
            RESPONSE_TIMEOUT.as_secs()
        ),
    )
}

/// 显式端点：`AGENT_SHELL_SOCKET`（空白视为未设置；判据在
/// `agent_shell_rpc::endpoint`，与命令面 `--socket` 同源）。
fn socket_endpoint() -> Option<PathBuf> {
    std::env::var_os("AGENT_SHELL_SOCKET")
        .map(PathBuf::from)
        .filter(|path| !agent_shell_rpc::endpoint::is_blank(path))
}

/// MCP server——桥接 MCP 工具调用到 agent-shell daemon。
pub struct AgentShellMcpServer {
    /// 懒建立的 daemon 连接；传输层失败后丢弃，下次调用重新建立。
    conn: Mutex<Option<DaemonConnection>>,
}

impl AgentShellMcpServer {
    /// 构造 MCP server。连接按需建立：daemon 不可达是工具调用的错误（1001），
    /// 不是进程启动失败——客户端拿到的必须是可读的项目错误码。
    pub fn new() -> Self {
        Self {
            conn: Mutex::new(None),
        }
    }

    /// 一次远程方法调用：连接按需建立并串行复用（每个进程同时一条连接）。
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let mut slot = self.conn.lock().await;
        if slot.is_none() {
            *slot = Some(DaemonConnection::connect().await?);
        }
        let conn = slot.as_mut().ok_or_else(|| {
            CallError::transport(
                RpcErrorCode::DaemonUnavailable,
                "daemon connection not established",
            )
        })?;
        let result = conn.call(method, params).await;
        if matches!(&result, Err(e) if e.is_transport()) {
            // 断链不留在槽里：下次调用重新建立连接（失败的调用本身不重试）。
            *slot = None;
        }
        result
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
                "聚焦窗口：按 window_id（原生标识，精确）或 app_id/title/pid 定位；多命中报歧义",
                json!({
                    "type": "object",
                    "properties": { "app_id": {"type":"string"}, "title": {"type":"string"}, "pid": {"type":"integer"}, "window_id": {"type":"string"}},
                    "anyOf": [{"required":["window_id"]},{"required":["app_id"]},{"required":["title"]},{"required":["pid"]}]
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
    async fn execute_tool(&self, tool_name: &str, args: &Value) -> Result<Value, CallError> {
        match tool_name {
            "focus_window" => self.execute_focus_window(args).await,
            "wait_for_window" => self.execute_wait_for_window(args).await,
            "click_element" => self.execute_click_element(args).await,
            "screenshot" => self.execute_screenshot(args).await,
            _ => {
                let (method, params) =
                    match Self::map_tool(tool_name, args).map_err(CallError::local)? {
                        Some(m) => m,
                        None => return Err(CallError::local(format!("unknown tool: {tool_name}"))),
                    };
                self.rpc(method, params).await
            }
        }
    }

    /// click_element：`a11y.query` 按 role/name 定位 → `a11y.action` 触发动作接口。
    ///
    /// 与命令面同语义：零命中或无障碍不可达时用 `fallback_coordinate` 坐标
    /// 点击兜底（无兜底则如实报错）；多命中不猜——报歧义并要求收窄条件
    /// （契约：目标解析多命中必须报错）。
    async fn execute_click_element(&self, args: &Value) -> Result<Value, CallError> {
        let role = args.get("role").and_then(Value::as_str);
        let name = args.get("name").and_then(Value::as_str);
        if role.is_none() && name.is_none() {
            return Err(CallError::local(
                "click_element requires `role` and/or `name`",
            ));
        }
        let fallback = parse_fallback_coordinate(args).map_err(CallError::local)?;

        let located = self
            .rpc(
                "a11y.query",
                json!({ "role": role, "name": name, "all": false }),
            )
            .await;
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
                None => Err(CallError::local(format!(
                    "a11y element not found (role={role:?}, name={name:?}); \
                     pass `fallback_coordinate` for a coordinate click"
                ))),
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
            return Err(CallError::local(format!(
                "a11y element ambiguous: {} matches for role={role:?}, name={name:?}; \
                 narrow `role`/`name`; candidates: {}",
                elements.len(),
                candidates.join(", ")
            )));
        }
        let element = &elements[0];
        let path = element
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CallError::local("malformed a11y.query element: missing path"))?;
        let bus = element.get("bus_name").and_then(Value::as_str);
        let action = self
            .rpc("a11y.action", json!({ "path": path, "bus": bus }))
            .await;
        match action {
            Ok(value) => Ok(value),
            Err(e) => match fallback {
                Some((x, y)) => self.click_at(x, y).await,
                None => Err(e),
            },
        }
    }

    /// 坐标兜底点击（`input.send` click，左键 + 绝对坐标）；结果原样返回。
    async fn click_at(&self, x: i64, y: i64) -> Result<Value, CallError> {
        self.rpc(
            "input.send",
            json!({ "kind": "click", "payload": { "button": "left", "at": [x, y] } }),
        )
        .await
    }

    /// screenshot：`target` 选择器决定实际捕获范围（全屏 / 窗口 / 区域）。
    ///
    /// 选择器与其它入参冲突时报错，不静默丢弃——否则 `target` 形同虚设。
    async fn execute_screenshot(&self, args: &Value) -> Result<Value, CallError> {
        let params = screenshot_params(args).map_err(CallError::local)?;
        self.rpc("screenshot.capture", params).await
    }

    /// focus_window: 先 windows.list 取列表，按 window_id/app_id/title/pid
    /// 解析 native_id，再 windows.op focus。
    ///
    /// 选择条件至少给一个：缺省即参数非法，本地失败、不触达服务（与
    /// `click_element` 的 `anyOf` + 本地 guard 同一做法）。
    async fn execute_focus_window(&self, args: &Value) -> Result<Value, CallError> {
        ensure_focus_selector(args)?;

        // 1. 取窗口列表
        let list_result = self
            .rpc("windows.list", json!({ "filter": Value::Null }))
            .await?;
        let windows: Vec<agent_shell_rpc::WindowEntry> =
            serde_json::from_value(list_result.get("windows").cloned().unwrap_or(Value::Null))
                .map_err(|e| {
                    CallError::transport(
                        RpcErrorCode::BackendError,
                        format!("malformed windows.list response: {e}"),
                    )
                })?;

        // 2. 按 window_id/app_id/title/pid 解析目标（多命中报歧义，见 resolve_target）
        let native_id = resolve_target(args, &windows)?;

        // 3. 发 windows.op focus
        let op_params = json!({ "op": "focus", "target": native_id });
        self.rpc("windows.op", op_params).await
    }

    /// wait_for_window: 轮询 windows.list + filter，直到匹配或超时。
    async fn execute_wait_for_window(&self, args: &Value) -> Result<Value, CallError> {
        let app_id = args
            .get("app_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CallError::local("missing required param: app_id"))?;
        let timeout_ms = Self::resolve_timeout_ms(args).map_err(CallError::local)?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

        loop {
            let list_result = self
                .rpc("windows.list", json!({ "filter": app_id }))
                .await?;

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
                    "error: {}",
                    e.render()
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

/// `focus_window` 的选择条件：至少给一个（schema `anyOf` 的本地对应判据）。
///
/// 全部缺省时 `resolve_target` 只能拼出空 label 的「未命中」文案，把参数问题伪装成
/// 目标不存在；这里先按参数非法显式失败，且先于 `windows.list`，无效 RPC。
fn ensure_focus_selector(args: &Value) -> Result<(), CallError> {
    const KEYS: [&str; 4] = ["window_id", "app_id", "title", "pid"];
    let present = KEYS
        .iter()
        .any(|key| args.get(*key).is_some_and(|value| !value.is_null()));
    if present {
        return Ok(());
    }
    Err(CallError::local(
        "focus_window requires one of window_id/app_id/title/pid",
    ))
}

/// 从 `windows.list` 结果中按 window_id/app_id/title/pid 解析目标窗口。
///
/// 选择条件优先级：window_id（原生标识精确，含花括号等价）→ app_id（精确）→
/// title（子串包含）→ pid（精确）。判定在
/// [`agent_shell_rpc::window_target`]，与命令面同源：
/// - 命中的那一档多命中即报歧义（匹配数 + 候选），不得静默取首个；
/// - 显式 `window_id` 失配即报未命中，绝不落回其余模糊条件——显式标识被静默忽略会
///   把写操作施加到另一扇窗口上。
fn resolve_target(
    args: &Value,
    windows: &[agent_shell_rpc::WindowEntry],
) -> Result<String, CallError> {
    use agent_shell_rpc::window_target;

    fn pick<'a>(
        windows: &'a [agent_shell_rpc::WindowEntry],
        label: String,
        matches: Vec<&'a agent_shell_rpc::WindowEntry>,
    ) -> Result<String, CallError> {
        window_target::expect_single(&label, matches, windows)
            .map(|window| window.native_id.clone())
            .map_err(CallError::local)
    }

    // 显式标识：精确匹配是终局判定，失配不进入下面的模糊档。
    if let Some(window_id) = args.get("window_id").and_then(Value::as_str) {
        let matches: Vec<&agent_shell_rpc::WindowEntry> = windows
            .iter()
            .filter(|window| window_target::native_id_matches(window_id, &window.native_id))
            .collect();
        return pick(windows, format!("window_id={window_id:?}"), matches);
    }

    if let Some(app_id) = args.get("app_id").and_then(Value::as_str) {
        let matches: Vec<&agent_shell_rpc::WindowEntry> = windows
            .iter()
            .filter(|window| window.app_id == app_id)
            .collect();
        if !matches.is_empty() {
            return pick(windows, format!("app_id={app_id:?}"), matches);
        }
    }

    if let Some(title) = args.get("title").and_then(Value::as_str) {
        let matches: Vec<&agent_shell_rpc::WindowEntry> = windows
            .iter()
            .filter(|window| window.title.contains(title))
            .collect();
        if !matches.is_empty() {
            return pick(windows, format!("title={title:?}"), matches);
        }
    }

    if let Some(pid) = args.get("pid").and_then(Value::as_u64) {
        let matches: Vec<&agent_shell_rpc::WindowEntry> = windows
            .iter()
            .filter(|window| u64::from(window.pid) == pid)
            .collect();
        if !matches.is_empty() {
            return pick(windows, format!("pid={pid}"), matches);
        }
    }

    // 全部条件零命中：与命令面同一文案，列出可见窗口供定位。
    let selectors: Vec<String> = ["window_id", "app_id", "title", "pid"]
        .into_iter()
        .filter_map(|key| {
            args.get(key)
                .filter(|value| !value.is_null())
                .map(|value| format!("{key}={value}"))
        })
        .collect();
    Err(CallError::local(format!(
        "window not found: {}; visible titles: {}",
        selectors.join(", "),
        agent_shell_rpc::window_target::candidates(windows)
    )))
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
        let windows = vec![
            window_entry("0x1", "Firefox", "firefox", 100),
            window_entry("0x2", "VS Code", "code", 200),
        ];
        let id = resolve_target(&json!({"app_id": "firefox"}), &windows).expect("app_id match");
        assert_eq!(id, "0x1");
    }

    #[test]
    fn test_resolve_target_by_title_substring() {
        let windows = vec![window_entry("0x1", "Firefox - Main", "firefox", 100)];
        let id = resolve_target(&json!({"title": "Main"}), &windows).expect("title substring");
        assert_eq!(id, "0x1");
    }

    #[test]
    fn test_resolve_target_by_pid() {
        let windows = vec![
            window_entry("0x1", "Firefox", "firefox", 100),
            window_entry("0x2", "Code", "code", 200),
        ];
        let id = resolve_target(&json!({"pid": 200}), &windows).expect("pid match");
        assert_eq!(id, "0x2");
    }

    #[test]
    fn test_resolve_target_by_window_id() {
        let windows = vec![window_entry("0x1", "Firefox", "firefox", 100)];
        let id = resolve_target(&json!({"window_id": "0x1"}), &windows).expect("native_id match");
        assert_eq!(id, "0x1");
    }

    #[test]
    fn test_resolve_target_no_match() {
        let windows = vec![window_entry("0x1", "Firefox", "firefox", 100)];
        let err = resolve_target(&json!({"app_id": "nonexistent"}), &windows)
            .expect_err("no window carries that app_id")
            .render();
        assert!(
            err.contains("window not found: app_id=\"nonexistent\""),
            "{err}"
        );
        assert!(
            err.contains("[0x1] Firefox"),
            "not-found must list visible titles: {err}"
        );
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

    fn window_entry(
        native_id: &str,
        title: &str,
        app_id: &str,
        pid: u32,
    ) -> agent_shell_rpc::WindowEntry {
        agent_shell_rpc::WindowEntry {
            native_id: native_id.into(),
            title: title.into(),
            app_id: app_id.into(),
            pid,
            x: None,
            y: None,
            width: None,
            height: None,
            workspace: None,
            stacking_order: 0,
            states: vec![],
            window_type: "Normal".into(),
            keep_above: false,
        }
    }

    /// 多命中必须报歧义：匹配数 + 候选都在错误里，不得静默取首个。
    #[test]
    fn resolve_target_rejects_multiple_matches() {
        let windows = vec![
            window_entry("w1", "Terminal", "kitty", 10),
            window_entry("w2", "Terminal", "kitty", 11),
            window_entry("w3", "Files", "nautilus", 12),
        ];
        let err = resolve_target(&json!({"app_id": "kitty"}), &windows)
            .expect_err("two windows with the same app_id must be ambiguous")
            .render();
        assert!(err.contains("2 windows match app_id=\"kitty\""), "{err}");
        assert!(err.contains("disambiguate with id:"), "{err}");
        assert!(
            err.contains("[w1] Terminal") && err.contains("[w2] Terminal"),
            "{err}"
        );
    }

    /// 显式 window_id 失配即未命中：不得落回同一次调用里的 app_id/title/pid。
    #[test]
    fn resolve_target_does_not_fall_back_after_window_id_mismatch() {
        let windows = vec![window_entry("w1", "Terminal", "kitty", 10)];
        let err = resolve_target(
            &json!({"window_id": "w-missing", "app_id": "kitty", "pid": 10}),
            &windows,
        )
        .expect_err("explicit window_id must not fall back to fuzzy selectors")
        .render();
        assert!(
            err.contains("window not found: window_id=\"w-missing\""),
            "{err}"
        );
        assert!(
            err.contains("[w1] Terminal"),
            "not-found must list visible titles: {err}"
        );
    }

    /// 显式标识与 KWin 的 `{uuid}` 原文等价；零命中错误带可见窗口。
    #[test]
    fn resolve_target_accepts_braced_window_id_and_reports_misses() {
        let windows = vec![
            window_entry("{e6f8-4a2c}", "Editor", "kate", 10),
            window_entry("w2", "Terminal", "konsole", 11),
        ];
        assert_eq!(
            resolve_target(&json!({"window_id": "e6f8-4a2c"}), &windows).expect("bare uuid"),
            "{e6f8-4a2c}"
        );
        let err = resolve_target(&json!({"app_id": "nope"}), &windows)
            .expect_err("missing app_id must not match")
            .render();
        assert!(err.contains("window not found: app_id=\"nope\""), "{err}");
        assert!(err.contains("[w2] Terminal"), "{err}");
    }

    /// 唯一命中按优先级返回：app_id 精确优先于标题子串。
    #[test]
    fn resolve_target_returns_unique_match() {
        let windows = vec![
            window_entry("w1", "Terminal — zsh", "kitty", 10),
            window_entry("w2", "Terminal — ssh", "konsole", 11),
        ];
        assert_eq!(
            resolve_target(&json!({"app_id": "konsole"}), &windows).expect("unique app_id"),
            "w2"
        );
        // 标题子串同命中两扇窗口 → 歧义，而不是取首个。
        assert!(resolve_target(&json!({"title": "Terminal"}), &windows).is_err());
    }

    /// 错误行只引用 stderr 末段：多行日志折叠为单行，且只保留最后几行。
    #[tokio::test]
    async fn stderr_text_keeps_only_the_tail() {
        let mut buf = std::collections::VecDeque::new();
        for i in 0..500 {
            buf.extend(format!("line {i}: noise\n").bytes());
        }
        let text = stderr_tail_text(&Some(Arc::new(Mutex::new(buf)))).await;
        assert!(
            !text.contains('\n'),
            "error line must stay single-line: {text}"
        );
        assert!(
            text.contains("line 499: noise"),
            "tail must keep the last line: {text}"
        );
        assert!(
            !text.contains("line 0: noise"),
            "old lines must be dropped: {text}"
        );
        assert_eq!(
            text.matches(" | ").count(),
            STDERR_ERROR_LINES - 1,
            "only {STDERR_ERROR_LINES} lines may survive: {text}"
        );
        assert!(
            text.len() <= STDERR_ERROR_BYTES,
            "tail must be bounded: {}",
            text.len()
        );
        assert!(stderr_tail_text(&None).await.is_empty());
    }

    /// schema 与本地 guard 同源：四个选择条件至少要给一个。
    #[test]
    fn focus_window_schema_requires_a_selector() {
        let tools = AgentShellMcpServer::build_tools();
        let tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "focus_window")
            .expect("focus_window tool registered");
        let any_of = tool
            .input_schema
            .get("anyOf")
            .and_then(|value| value.as_array())
            .expect("focus_window schema carries anyOf");
        assert_eq!(any_of.len(), 4, "one branch per selector: {any_of:?}");
        for key in ["window_id", "app_id", "title", "pid"] {
            assert!(
                any_of.iter().any(|branch| branch["required"]
                    .as_array()
                    .is_some_and(|required| required.iter().any(|v| v.as_str() == Some(key)))),
                "anyOf must require {key}: {any_of:?}"
            );
        }
    }

    /// 空选择条件是参数非法：给出可读文案，而不是空 label 的「未命中」。
    #[test]
    fn focus_window_rejects_empty_selectors() {
        let err = ensure_focus_selector(&json!({}))
            .expect_err("no selector must be rejected")
            .render();
        assert!(
            err.contains("focus_window requires one of window_id/app_id/title/pid"),
            "{err}"
        );
        assert!(ensure_focus_selector(&json!({"window_id": null, "app_id": null})).is_err());
        for args in [
            json!({"window_id": "w1"}),
            json!({"app_id": "kitty"}),
            json!({"title": "Terminal"}),
            json!({"pid": 42}),
        ] {
            assert!(
                ensure_focus_selector(&args).is_ok(),
                "{args} carries a selector"
            );
        }
    }
}
