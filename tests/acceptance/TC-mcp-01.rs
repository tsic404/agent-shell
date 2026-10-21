//! TC-mcp-01（AC-106，P1）：MCP 握手与工具列举、入参完整性、语义等价、错误
//! 投影、占位透传、结果投影。
//!
//! - 场景：MCP 协议面（NDJSON over stdio）的投影不变量
//! - 判据：握手声明工具能力；列举单页；入参逐一可观察生效；错误不降级；
//!   占位回执原样透传；结果逐字段原样
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-mcp-01`
//!
//! 判定面是真实 `agent-shell-mcp` 进程与它按生产路径派生的 daemon：隔离
//! HOME/XDG/会话变量后走 initialize → tools/list → tools/call。
//!
//! 本用例逐条判定 AC-106 的投影不变量：
//! - 握手声明 `capabilities.tools`；
//! - 工具列举单页、名称唯一、schema 自洽；
//! - 每个声明入参可观察生效（`list_windows.workspace` 到服务端工作区谓词，
//!   `screenshot.target` 选择器自洽校验，`subscribe_events.events` 落到订阅过
//!   滤器，`security_check.operation` 落到服务端判定，`query_a11y_elements`
//!   的 `all` 放开全树门槛）；
//! - 映射方法真实实现工具语义（`click_element` 经 `a11y.query` 定位后走
//!   `a11y.action`，定位失败才用 `fallback_coordinate` 坐标兜底）；
//! - 错误保留项目错误码且不降级为成功；占位回执原样透传；成功结果的结构化
//!   部分与文本内容逐字段一致（结果不裁剪、不改名）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use acceptance::workspace_root;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// 期望的 18 个工具名（与 `mcp/src/server.rs` 的注册表一致）。
const TOOLS: [&str; 18] = [
    "list_windows",
    "focus_window",
    "click_element",
    "screenshot",
    "wait_for_window",
    "query_a11y_elements",
    "subscribe_events",
    "security_check",
    "daemon_status",
    "ime_set_engine",
    "ime_type",
    "set_brightness",
    "file_picker",
    "file_trash",
    "default_app_get",
    "default_app_set",
    "bluetooth_scan",
    "list_timers",
];

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
}

fn build_binaries() -> PathBuf {
    let root = workspace_root();
    let output = Command::new(cargo())
        .current_dir(&root)
        .args([
            "build",
            "-q",
            "-p",
            "mcp",
            "--bin",
            "agent-shell-mcp",
            "-p",
            "agent-shell-daemon",
            "--bin",
            "agent-shell-daemon",
            // 与 CI 的 no-pipewire 腿同形：MCP 测试不依赖 portal-screencast，
            // 无 PipeWire dev 头的环境也能构建可用的 daemon。
            "--no-default-features",
        ])
        .output()
        .expect("build MCP and daemon binaries");
    assert!(
        output.status.success(),
        "building MCP/daemon must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let binary = target_dir(&root).join(profile).join("agent-shell-mcp");
    assert!(binary.is_file(), "missing {}", binary.display());
    binary
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    async fn start(binary: &Path, sandbox: &Path) -> Self {
        let mut command = tokio::process::Command::new(binary);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("HOME", sandbox.join("home"))
            .env("XDG_CONFIG_HOME", sandbox.join("config"))
            .env("XDG_STATE_HOME", sandbox.join("state"))
            .env("XDG_RUNTIME_DIR", sandbox.join("run"))
            .env("XDG_SESSION_TYPE", "tty")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("DESKTOP_SESSION")
            .env_remove("XDG_CURRENT_DESKTOP")
            .env_remove("AGENT_SHELL_AGENT_ID");
        let mut child = command.spawn().expect("spawn agent-shell-mcp");
        let stdin = child.stdin.take().expect("mcp stdin");
        let stdout = BufReader::new(child.stdout.take().expect("mcp stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut text = serde_json::to_string(&line).expect("encode request");
        text.push('\n');
        self.stdin
            .write_all(text.as_bytes())
            .await
            .expect("write MCP request");
        self.stdin.flush().await.expect("flush MCP request");
        id
    }

    /// 读一行并把通知（无 id）跳过，返回匹配 id 的响应。
    async fn response(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(15), self.stdout.read_line(&mut line))
                    .await
                    .expect("MCP response timeout")
                    .expect("read MCP response");
            assert!(read > 0, "MCP server closed stdout");
            let value: Value = serde_json::from_str(line.trim_end()).expect("MCP line JSON");
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    }

    async fn send_notification(&mut self, method: &str) {
        let line = json!({"jsonrpc": "2.0", "method": method});
        let mut text = serde_json::to_string(&line).expect("encode notification");
        text.push('\n');
        self.stdin
            .write_all(text.as_bytes())
            .await
            .expect("write MCP notification");
        self.stdin.flush().await.expect("flush MCP notification");
    }

    /// `tools/call` 一次往返，返回 JSON-RPC `result` 对象。
    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let id = self
            .send("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let response = self.response(id).await;
        response.get("result").cloned().expect("tools/call result")
    }
}

/// `tools/call` 失败时的文本内容（成功结果返回空串）。
fn error_text(result: &Value) -> String {
    if result.get("isError") != Some(&json!(true)) {
        return String::new();
    }
    result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[tokio::test]
async fn mcp_projection_preserves_tools_errors_and_receipts() {
    let sandbox = tempfile::tempdir().expect("mcp sandbox");
    std::fs::create_dir_all(sandbox.path().join("home")).expect("home");
    std::fs::create_dir_all(sandbox.path().join("config/agent-shell")).expect("config");
    std::fs::create_dir_all(sandbox.path().join("state")).expect("state");
    std::fs::create_dir_all(sandbox.path().join("run")).expect("run");
    // 安全策略夹具：黑名单命中让 `security_check.operation` 有 deny 可观察面。
    // daemon 在 MCP 进程启动时派生并读取该 XDG 配置目录。
    std::fs::write(
        sandbox.path().join("config/agent-shell/config.toml"),
        "[permissions.deny]\n\"process.kill\" = true\n",
    )
    .expect("write security config");
    let binary = tokio::task::spawn_blocking(build_binaries)
        .await
        .expect("join build");
    let mut mcp = Mcp::start(&binary, sandbox.path()).await;

    // 握手：协议版本、服务标识与指令字段可读。
    let id = mcp
        .send(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "acceptance", "version": "1"},
            }),
        )
        .await;
    let init = mcp.response(id).await;
    let result = init.get("result").expect("initialize result");
    assert!(result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .is_some());
    assert!(result.get("instructions").and_then(Value::as_str).is_some());
    assert!(result
        .get("serverInfo")
        .and_then(|v| v.get("name"))
        .is_some());
    // 握手必须声明工具能力：空 capabilities 会被读成「不提供工具」。
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("tools"))
            .is_some(),
        "initialize must declare capabilities.tools: {result}"
    );
    mcp.send_notification("notifications/initialized").await;

    // 工具列举：单页、名称唯一、schema 自洽（required ⊆ properties）。
    let id = mcp.send("tools/list", json!({})).await;
    let list = mcp.response(id).await;
    let result = list.get("result").expect("list result");
    assert!(
        result.get("nextCursor").is_none(),
        "tool listing must be a single page"
    );
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");
    assert_eq!(tools.len(), TOOLS.len(), "18 tools must be registered");
    let mut names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    names.sort_unstable();
    let mut expected = TOOLS.to_vec();
    expected.sort_unstable();
    assert_eq!(names, expected, "tool names must match the registry");
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        assert!(
            tool.get("description")
                .and_then(Value::as_str)
                .is_some_and(|d| !d.is_empty()),
            "{name} must carry a description"
        );
        let schema = tool.get("inputSchema").expect("inputSchema");
        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("object"),
            "{name} inputSchema must be an object"
        );
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required {
                let key = key.as_str().expect("required name");
                assert!(
                    properties.contains_key(key),
                    "{name}.required {key:?} must exist in properties"
                );
            }
        }
    }

    // 语义等价 + 结果投影：daemon_status 的结构化部分即方法结果，文本是其
    // pretty JSON，成功结果不带 isError。
    let id = mcp
        .send(
            "tools/call",
            json!({"name": "daemon_status", "arguments": {}}),
        )
        .await;
    let call = mcp.response(id).await;
    let result = call.get("result").expect("call result");
    assert!(
        result.get("isError") != Some(&json!(true)),
        "daemon_status must succeed: {result}"
    );
    let structured = result.get("structuredContent").expect("structuredContent");
    assert_eq!(structured.get("running"), Some(&json!(true)));
    assert!(structured
        .get("subscribers")
        .and_then(Value::as_u64)
        .is_some());
    assert_eq!(structured.get("ime_engine"), Some(&Value::Null));
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .expect("text content");
    assert_eq!(
        result["content"][0]["type"].as_str(),
        Some("text"),
        "content item must be text"
    );
    let reparsed: Value = serde_json::from_str(text).expect("text is JSON");
    assert_eq!(
        reparsed, *structured,
        "text content must be the structured result verbatim"
    );

    // ── 声明入参逐一可观察生效 ──────────────────────────────────────────
    // 判定面：headless 沙箱里后端不可用，但每个入参各自改变「走到哪一步」——
    // 参数问题报 -32602，参数放行后落到对应后端缺失的错误，两者可区分。

    // query_a11y_elements.all：缺省时 daemon 拒绝无过滤全树；all=true 放行。
    let result = mcp.call_tool("query_a11y_elements", json!({})).await;
    let text = error_text(&result);
    assert!(
        text.contains("-32602") && text.contains("requires `role` and/or `name`"),
        "full-tree query without role/name/all must be rejected: {text}"
    );
    let result = mcp
        .call_tool("query_a11y_elements", json!({"all": true}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("1002") && text.contains("AT-SPI unavailable"),
        "all=true must pass the gate and reach a11y.query: {text}"
    );

    // click_element：按 role+name 走 a11y.query 定位（不再是 a11y.status）；
    // 无障碍不可达时报后端不可用，而不是返回一份状态报告。
    let result = mcp
        .call_tool("click_element", json!({"role": "button", "name": "OK"}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("AT-SPI unavailable"),
        "click_element must locate through a11y.query: {text}"
    );
    // fallback_coordinate：无障碍不可达时改走坐标点击——错误来源不再是 a11y
    // （输入后端也可能不可用，故只断言不再报 a11y 错误）。
    let result = mcp
        .call_tool(
            "click_element",
            json!({"role": "button", "name": "OK", "fallback_coordinate": [10, 20]}),
        )
        .await;
    let text = error_text(&result);
    assert!(
        !text.contains("AT-SPI unavailable"),
        "fallback_coordinate must bypass the a11y failure: {text}"
    );

    // subscribe_events.events：词表值落到订阅过滤器；非法值被服务端拒绝并列出合法取值。
    let result = mcp
        .call_tool("subscribe_events", json!({"events": ["WindowOpened"]}))
        .await;
    let text = error_text(&result);
    assert!(
        text.is_empty(),
        "valid event filter must subscribe successfully: {text}"
    );
    assert!(
        result["structuredContent"]["subscriber_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "subscription must return a handle: {result}"
    );
    let result = mcp
        .call_tool("subscribe_events", json!({"events": ["bogus"]}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("-32602")
            && text.contains("unknown event filter: bogus")
            && text.contains("valid values:"),
        "invalid event filter must be rejected with the vocabulary: {text}"
    );

    // list_windows.workspace：类型错误由服务端参数校验拒绝；合法标识放行到窗口通道。
    let result = mcp.call_tool("list_windows", json!({"workspace": 3})).await;
    let text = error_text(&result);
    assert!(
        text.contains("-32602") && text.contains("params.workspace"),
        "workspace must reach the service-side workspace filter: {text}"
    );
    let result = mcp
        .call_tool("list_windows", json!({"workspace": "1"}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("1002") && text.contains("compositor"),
        "a valid workspace id must pass parameter validation: {text}"
    );

    // security_check.operation：判定与门禁同源（allow / confirm / deny），
    // 未登记操作名与坏类型被拒绝。
    let result = mcp
        .call_tool("security_check", json!({"operation": "windows.list"}))
        .await;
    assert!(
        result.get("isError") != Some(&json!(true)),
        "security_check must succeed: {result}"
    );
    let structured = result.get("structuredContent").expect("structuredContent");
    assert_eq!(structured["check"]["decision"], json!("allow"));
    assert_eq!(structured["check"]["level"], json!("L0"));
    let text = result["content"][0]["text"].as_str().expect("text content");
    let reparsed: Value = serde_json::from_str(text).expect("text is JSON");
    assert_eq!(
        reparsed, *structured,
        "security status projection must be verbatim"
    );
    let result = mcp
        .call_tool("security_check", json!({"operation": "mount.mount"}))
        .await;
    assert_eq!(
        result["structuredContent"]["check"]["decision"],
        json!("confirm"),
        "L4 operation must require confirmation: {result}"
    );
    let result = mcp
        .call_tool("security_check", json!({"operation": "process.kill"}))
        .await;
    assert_eq!(
        result["structuredContent"]["check"]["decision"],
        json!("deny"),
        "deny-listed operation must be denied: {result}"
    );
    let result = mcp
        .call_tool("security_check", json!({"operation": "nope"}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("-32602") && text.contains("unknown operation: nope"),
        "unknown operation must be rejected: {text}"
    );

    // screenshot.target：选择器与其它入参自洽校验；合法选择器放行到捕获通道。
    let result = mcp
        .call_tool("screenshot", json!({"target": "window"}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("requires `window`"),
        "target=window without window must be rejected: {text}"
    );
    let result = mcp
        .call_tool(
            "screenshot",
            json!({"target": "screen", "area": [0, 0, 10, 10]}),
        )
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("takes neither"),
        "target=screen with area must be rejected: {text}"
    );
    let result = mcp
        .call_tool("screenshot", json!({"target": "screen"}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("1002") && text.contains("capture unavailable"),
        "target=screen must reach screenshot.capture: {text}"
    );

    // 错误投影：项目错误码与消息保留，不降级为成功。
    let id = mcp
        .send(
            "tools/call",
            json!({"name": "set_brightness", "arguments": {"value": 200}}),
        )
        .await;
    let call = mcp.response(id).await;
    let result = call.get("result").expect("call result");
    assert_eq!(result.get("isError"), Some(&json!(true)));
    let text = result["content"][0]["text"].as_str().expect("error text");
    assert!(
        text.contains("RPC error -32602") && text.contains("brightness value out of range: 200"),
        "error projection must preserve project code and message: {text}"
    );

    // 占位回执原样透传：状态字段 + 服务名，呈现为成功结果。
    for (name, service) in [
        ("file_picker", "file.pick"),
        ("bluetooth_scan", "bluetooth.scan"),
        ("default_app_set", "mime.set"),
    ] {
        let arguments = match name {
            "default_app_set" => json!({"mime": "text/html", "app": "firefox.desktop"}),
            _ => json!({}),
        };
        let id = mcp
            .send("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let call = mcp.response(id).await;
        let result = call.get("result").expect("call result");
        assert!(
            result.get("isError") != Some(&json!(true)),
            "{name} must be a success receipt, got: {result}"
        );
        assert_eq!(
            result["structuredContent"],
            json!({"status": "not_implemented", "service": service}),
            "{name} placeholder receipt must pass through verbatim"
        );
    }

    // 未知工具：显式错误，不静默成功。
    let id = mcp
        .send("tools/call", json!({"name": "nope", "arguments": {}}))
        .await;
    let call = mcp.response(id).await;
    let result = call.get("result").expect("call result");
    assert_eq!(result.get("isError"), Some(&json!(true)));
    assert!(
        result["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("unknown tool: nope")),
        "unknown tool must be an explicit error"
    );
}
