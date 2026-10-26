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
//!   滤器，`security_check.operation` 落到服务端判定（省略则返回策略真值），
//!   `query_a11y_elements` 的 `all` 放开全树门槛）；
//! - 映射方法真实实现工具语义（`click_element` 经 `a11y.query` 定位后走
//!   `a11y.action`，定位失败才用 `fallback_coordinate` 坐标兜底）；
//! - 错误保留项目错误码且不降级为成功；占位回执原样透传；成功结果的结构化
//!   部分与文本内容逐字段一致（结果不裁剪、不改名）。
//! - 常驻实例在场时入口接入 `AGENT_SHELL_SOCKET` 指定的实例：订阅在同一实例
//!   可数，桌面依赖方法返回项目错误码而不是传输超时。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acceptance::workspace_root;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
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

/// 判定面用到的三个真实二进制。
struct Bins {
    mcp: PathBuf,
    daemon: PathBuf,
    cli: PathBuf,
}

fn build_binaries() -> Bins {
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
            "-p",
            "agent-shell-cli",
            "--bin",
            "agent-shell",
            // 与 CI 的 no-pipewire 腿同形：MCP 测试不依赖 portal-screencast，
            // 无 PipeWire dev 头的环境也能构建可用的 daemon。
            "--no-default-features",
        ])
        .output()
        .expect("build MCP, daemon and CLI binaries");
    assert!(
        output.status.success(),
        "building MCP/daemon/CLI must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let dir = target_dir(&root).join(profile);
    let bins = Bins {
        mcp: dir.join("agent-shell-mcp"),
        daemon: dir.join("agent-shell-daemon"),
        cli: dir.join("agent-shell"),
    };
    for binary in [&bins.mcp, &bins.daemon, &bins.cli] {
        assert!(binary.is_file(), "missing {}", binary.display());
    }
    bins
}

/// 沙箱化的端点/配置/状态目录：常驻实例与瞬态 daemon 共用同一 `XDG_RUNTIME_DIR`，
/// 端点与默认会话锁都落在沙箱内，不触碰宿主会话。
fn sandbox_env(command: &mut tokio::process::Command, sandbox: &Path) {
    command
        .env("HOME", sandbox.join("home"))
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_RUNTIME_DIR", sandbox.join("run"))
        .env("XDG_SESSION_TYPE", "tty")
        // PATH 置空：隐去宿主 ydotool/xdotool，让「输入链为空」是前提而非宿主巧合。
        .env("PATH", "")
        .env_remove("AGENT_SHELL_LOCK")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("DESKTOP_SESSION")
        .env_remove("XDG_CURRENT_DESKTOP")
        .env_remove("AGENT_SHELL_AGENT_ID");
}

/// 以显式套接字形态拉起常驻 daemon，且**不换锁命名空间**——默认会话锁被它持有，
/// 正是「MCP 另起瞬态 daemon」会卡住的部署形态。
async fn start_resident(bins: &Bins, sandbox: &Path, socket: &Path) -> Child {
    let mut command = tokio::process::Command::new(&bins.daemon);
    command
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    sandbox_env(&mut command, sandbox);
    let mut child = command.spawn().expect("spawn resident daemon");
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            assert!(
                sandbox.join("run/agent-shell.lock").is_file(),
                "resident must hold the default session lock for this scenario to bite: {}",
                socket.display()
            );
            return child;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = child.start_kill();
    panic!("resident daemon did not bind {}", socket.display());
}

/// 以同一端点跑一次 CLI，读数取自常驻实例自身。
async fn cli_json(
    bins: &Bins,
    sandbox: &Path,
    socket: &Path,
    args: &[&str],
) -> (i32, Value, String) {
    let mut command = tokio::process::Command::new(&bins.cli);
    command.arg("--socket").arg(socket).args(args);
    sandbox_env(&mut command, sandbox);
    let output = command.output().await.expect("run agent-shell");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    (output.status.code().unwrap_or(-1), value, stderr)
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    async fn start(binary: &Path, sandbox: &Path, socket: Option<&Path>) -> Self {
        let mut command = tokio::process::Command::new(binary);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        sandbox_env(&mut command, sandbox);
        match socket {
            Some(socket) => command.env("AGENT_SHELL_SOCKET", socket),
            None => command.env_remove("AGENT_SHELL_SOCKET"),
        };
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
    let bins = tokio::task::spawn_blocking(build_binaries)
        .await
        .expect("join build");
    let mut mcp = Mcp::start(&bins.mcp, sandbox.path(), None).await;

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
    // fallback_coordinate：无障碍不可达时改走坐标点击——必须落到输入后端：
    // 报输入侧的 1002 与输入链失败文案，错误不来自 a11y（a11y 不可达同为 1002）。
    let result = mcp
        .call_tool(
            "click_element",
            json!({"role": "button", "name": "OK", "fallback_coordinate": [10, 20]}),
        )
        .await;
    let text = error_text(&result);
    // 输入链的两种宿主态都必须被认作输入侧失败：空链（无候选）与有候选但
    // 缺绝对坐标能力（如宿主 /dev/uinput 可写、沙箱无桌面几何）。
    let input_failure = text.contains("input unavailable in this session")
        || text.contains("input: no backend in this session can perform");
    assert!(
        text.contains("1002") && input_failure,
        "fallback_coordinate must reach the input backend and keep its error code: {text}"
    );
    assert!(
        !text.to_lowercase().contains("a11y") && !text.contains("AT-SPI"),
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

    // 省略 operation 返回策略真值（反证：只认键缺失时，投射层物化的 null 会报 -32602）。
    let result = mcp.call_tool("security_check", json!({})).await;
    assert!(
        result.get("isError") != Some(&json!(true)),
        "security_check without operation must return the policy truth: {result}"
    );
    let structured = result.get("structuredContent").expect("structuredContent");
    assert!(
        structured.get("check").is_none(),
        "policy truth must not carry a decision: {structured}"
    );
    assert_eq!(structured["default_confirm_level"], json!("L3"));
    assert_eq!(structured["deny"]["process.kill"], json!(true));
    assert!(
        structured["allow"].is_object() && structured["audit_path"].is_string(),
        "policy truth must carry allow and audit_path: {structured}"
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

/// 常驻实例在场时的投影：端点由 `AGENT_SHELL_SOCKET` 指定时，MCP 入口必须接入
/// 该实例，方法结果与项目错误码照常呈现。
///
/// 复现形态取自 QA 部署：常驻 daemon 以显式套接字形态启动，且不换锁命名空间——
/// 默认会话锁被它持有。入口若忽略端点另起瞬态 daemon，派生进程会卡在单实例锁
/// 上，`tools/call` 只会拿到传输失败（旧实现即固定 10 秒后的 `error: timeout`），
/// 方法结果与项目错误码一概不可见。
#[tokio::test]
async fn mcp_reuses_resident_endpoint_for_method_results() {
    let sandbox = tempfile::tempdir().expect("mcp sandbox");
    for dir in ["home", "config/agent-shell", "state", "run"] {
        std::fs::create_dir_all(sandbox.path().join(dir)).expect("sandbox dir");
    }
    // 安全策略夹具：让 security_check 有可观察的策略真值。
    std::fs::write(
        sandbox.path().join("config/agent-shell/config.toml"),
        "[permissions.deny]\n\"process.kill\" = true\n",
    )
    .expect("write security config");

    let bins = tokio::task::spawn_blocking(build_binaries)
        .await
        .expect("join build");
    let socket = sandbox.path().join("run/agent-shell.sock");
    let mut resident = start_resident(&bins, sandbox.path(), &socket).await;
    let mut mcp = Mcp::start(&bins.mcp, sandbox.path(), Some(&socket)).await;

    // 握手与列举：接入常驻实例后工具面照旧成立。
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
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("tools"))
            .is_some(),
        "initialize must declare capabilities.tools: {result}"
    );
    mcp.send_notification("notifications/initialized").await;
    let id = mcp.send("tools/list", json!({})).await;
    let list = mcp.response(id).await;
    let tools = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .clone();
    assert_eq!(tools.len(), TOOLS.len(), "18 tools must be registered");

    // 非桌面方法返回结构化结果：daemon_status 的结构化部分即方法结果。
    let result = mcp.call_tool("daemon_status", json!({})).await;
    assert!(
        result.get("isError") != Some(&json!(true)),
        "daemon_status must succeed over the resident endpoint: {result}"
    );
    let structured = result
        .get("structuredContent")
        .expect("daemon_status structuredContent");
    assert_eq!(structured["running"], json!(true));
    let text = result["content"][0]["text"].as_str().expect("text content");
    let reparsed: Value = serde_json::from_str(text).expect("text is JSON");
    assert_eq!(
        reparsed, *structured,
        "text content must be the structured result verbatim"
    );

    // 同一实例：MCP 侧订阅在常驻实例自身可数——两边是两个实例时计数只会落在
    // 瞬态 daemon 上，命令行侧读到的永远是 0。
    let result = mcp
        .call_tool("subscribe_events", json!({"events": ["WindowOpened"]}))
        .await;
    assert!(
        result["structuredContent"]["subscriber_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "subscription must return a handle: {result}"
    );
    let (code, status, stderr) =
        cli_json(&bins, sandbox.path(), &socket, &["daemon", "status"]).await;
    assert_eq!(code, 0, "CLI daemon status must succeed; stderr: {stderr}");
    assert_eq!(
        status["subscribers"],
        json!(1),
        "the resident instance must see the MCP subscription (one instance, not two): {status}"
    );

    // 桌面依赖方法：返回项目错误码而不是传输超时。
    let result = mcp.call_tool("list_windows", json!({})).await;
    let text = error_text(&result);
    assert!(
        text.contains("1002") && text.contains("compositor"),
        "desktop-dependent method must keep the project error code: {text}"
    );
    assert!(
        !text.contains("timeout"),
        "desktop-dependent method must not degrade to a transport timeout: {text}"
    );
    // 命令行侧同类方法对照：同一方法面在同一实例上给出同一项目错误码。
    let (code, _, stderr) = cli_json(&bins, sandbox.path(), &socket, &["windows", "list"]).await;
    assert_eq!(
        code, 1,
        "CLI windows list must fail as a remote error: {stderr}"
    );
    assert!(
        stderr.contains("1002") && stderr.contains("compositor"),
        "CLI must keep the same project error code: {stderr}"
    );

    // 占位回执原样透传（AC-105 清单方法），呈现为成功结果。
    let result = mcp.call_tool("file_picker", json!({})).await;
    assert!(
        result.get("isError") != Some(&json!(true)),
        "file_picker must be a success receipt: {result}"
    );
    assert_eq!(
        result["structuredContent"],
        json!({"status": "not_implemented", "service": "file.pick"}),
        "placeholder receipt must pass through verbatim: {result}"
    );

    // 端点不可达：不静默回退派生实例，按 daemon 不可用（1001）显式失败。
    let missing = sandbox.path().join("run/missing.sock");
    let mut orphan = Mcp::start(&bins.mcp, sandbox.path(), Some(&missing)).await;
    let id = orphan
        .send(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "acceptance", "version": "1"}}),
        )
        .await;
    orphan.response(id).await;
    let result = orphan.call_tool("daemon_status", json!({})).await;
    let text = error_text(&result);
    assert!(
        text.contains("1001") && text.contains("cannot connect"),
        "unreachable endpoint must fail with daemon-unavailable, not fall back: {text}"
    );

    let _ = resident.start_kill();
}

/// 脚本化 daemon：只回答本用例需要的方法，并记录收到的请求。
///
/// 无头环境里真实 daemon 没有窗口夹具（`windows.list` 直接报 1002 compositor），
/// 而本用例判定的是 MCP 入口自身的目标解析语义。因此用协议对端顶替 daemon，
/// 判定面仍是真实 `agent-shell-mcp` 二进制发出的 JSON-RPC 与它返回的内容。
struct ScriptedDaemon {
    socket: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl ScriptedDaemon {
    async fn start(socket: &Path, windows: Vec<Value>) -> Self {
        let listener = UnixListener::bind(socket).expect("bind scripted daemon");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                let windows = windows.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Ok(request) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let id = request.get("id").cloned().unwrap_or(Value::Null);
                        let method = request
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        recorded.lock().unwrap().push(request.clone());
                        let result = match method.as_str() {
                            "windows.list" => json!({"windows": windows, "from_cache": false}),
                            "windows.op" => json!({
                                "op": request["params"]["op"],
                                "target": request["params"]["target"],
                            }),
                            _ => json!({}),
                        };
                        let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
                        let mut text = response.to_string();
                        text.push('\n');
                        if write.write_all(text.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = write.flush().await;
                    }
                });
            }
        });
        Self {
            socket: socket.to_path_buf(),
            requests,
            task,
        }
    }

    /// 已收到的方法请求数（用于断言「本地失败、未触达服务」）。
    fn request_count(&self, method: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == json!(method))
            .count()
    }

    /// 已发出的 `windows.op` 请求的目标标识（未发出为空）。
    fn windows_op_targets(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == json!("windows.op"))
            .map(|request| request["params"]["target"].clone())
            .collect()
    }
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// 目标解析与命令面同源：多命中报歧义（匹配数 + 候选）、零命中列可见窗口、
/// 显式 `window_id` 失配不落回模糊条件，且失败路径一次窗口写操作都不发出。
#[tokio::test]
async fn mcp_target_resolution_rejects_ambiguous_and_never_falls_back() {
    let sandbox = tempfile::tempdir().expect("mcp sandbox");
    for dir in ["home", "config/agent-shell", "state", "run"] {
        std::fs::create_dir_all(sandbox.path().join(dir)).expect("sandbox dir");
    }
    let bins = tokio::task::spawn_blocking(build_binaries)
        .await
        .expect("join build");
    let socket = sandbox.path().join("run/scripted.sock");
    let daemon = ScriptedDaemon::start(
        &socket,
        vec![
            json!({"native_id": "w1", "title": "Terminal — zsh", "app_id": "kitty", "pid": 10}),
            json!({"native_id": "w2", "title": "Terminal — ssh", "app_id": "kitty", "pid": 11}),
            json!({"native_id": "w3", "title": "Files", "app_id": "nautilus", "pid": 12}),
        ],
    )
    .await;
    let mut mcp = Mcp::start(&bins.mcp, sandbox.path(), Some(&socket)).await;
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
    mcp.response(id).await;
    mcp.send_notification("notifications/initialized").await;

    // 空选择条件：参数非法，本地失败（不触达服务，不发 windows.list）。
    let result = mcp.call_tool("focus_window", json!({})).await;
    let text = error_text(&result);
    assert!(
        text.contains("focus_window requires one of window_id/app_id/title/pid"),
        "missing selectors must be an invalid-params error: {text}"
    );
    assert_eq!(
        daemon.request_count("windows.list"),
        0,
        "invalid params must fail before any RPC"
    );

    // 多命中：同一 app_id 的两扇窗口 → 歧义错误，带匹配数与候选。
    let result = mcp
        .call_tool("focus_window", json!({"app_id": "kitty"}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("2 windows match app_id=\"kitty\"") && text.contains("disambiguate with id:"),
        "multiple matches must be an ambiguity error: {text}"
    );
    assert!(
        text.contains("[w1] Terminal — zsh") && text.contains("[w2] Terminal — ssh"),
        "ambiguity must list the matching candidates: {text}"
    );

    // 零命中：错误可读，列出可见窗口。
    let result = mcp
        .call_tool("focus_window", json!({"app_id": "nope"}))
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("window not found: app_id=\"nope\"") && text.contains("[w3] Files"),
        "a miss must name the selector and the visible windows: {text}"
    );

    // 显式标识失配：同一次调用还带了能命中的 app_id，也不得落回模糊匹配。
    let result = mcp
        .call_tool(
            "focus_window",
            json!({"window_id": "w-missing", "app_id": "kitty", "pid": 10}),
        )
        .await;
    let text = error_text(&result);
    assert!(
        text.contains("window not found: window_id=\"w-missing\""),
        "an explicit window_id miss must not fall back to fuzzy selectors: {text}"
    );
    assert!(
        daemon.windows_op_targets().is_empty(),
        "no window write may be issued by a failed resolution: {:?}",
        daemon.windows_op_targets()
    );

    // 唯一命中：照常下发 windows.op，目标即解析结果。
    let result = mcp
        .call_tool("focus_window", json!({"window_id": "w3"}))
        .await;
    assert!(
        result.get("isError") != Some(&json!(true)),
        "unique window_id must focus: {result}"
    );
    assert_eq!(
        result
            .get("structuredContent")
            .and_then(|value| value.get("target")),
        Some(&json!("w3")),
        "windows.op must carry the resolved native_id: {result}"
    );
    assert_eq!(daemon.windows_op_targets(), vec![json!("w3")]);
}
