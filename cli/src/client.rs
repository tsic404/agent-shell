//! CLI → daemon JSON-RPC 客户端（§22.2 D1）。
//!
//! CLI 是瞬态无状态进程，不直连任何系统服务（D-Bus/Wayland/X11/portal/
//! AT-SPI）。本模块负责：
//! 1. daemon 连接获取——优先 systemd socket activation（LISTEN_FDS），
//!    否则 fork/exec `agent-shell-daemon --foreground` 子进程并经 stdio
//!    通信（自动激活语义，§22.2 激活策略）；
//! 2. 请求/响应往返（行分隔 JSON-RPC 2.0）；
//! 3. 错误码 → CLI 退出码映射。

use agent_shell_rpc::{method, Request, Response};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 结构化调用错误：RPC 错误携带 code，便于调用方按退出码分派
/// （如 mount 的 polkit 拒绝 → exit 2，其余 → exit 1）。
pub enum CallError {
    /// JSON-RPC 错误响应（code + message）。
    Rpc { code: i32, message: String },
    /// 传输/协议错误（无 RPC code）。
    Other(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Rpc { code, message } => write!(f, "rpc error {code}: {message}"),
            CallError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<String> for CallError {
    fn from(msg: String) -> Self {
        CallError::Other(msg)
    }
}

/// 连接提前关闭错误前缀——`read_line` 收到 EOF（daemon 在写出响应前退出）
/// 时返回。CLI 侧 `--retry` 重建连接重试，`subscribe` 据此判定流结束。
const CLOSED_EARLY: &str = "daemon closed connection before responding";

/// 连接重建后的短退避（毫秒）。portal/DBus 会话就绪竞态通常在百毫秒级
/// 恢复（§19 短退避）。
const RETRY_BACKOFF_MS: u64 = 150;

/// 一个 daemon 连接上的客户端会话。
pub struct DaemonClient {
    child: Option<tokio::process::Child>,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
    /// 「连接提前关闭」时的重建重试次数（`--retry N` 注入）。
    retries: u32,
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // CLI 瞬态退出即断开；daemon（若为拉起的调试实例）随 stdin EOF 退出，
            // 常驻形态由 systemd 管理，此处 kill 兜底防孤儿。
            let _ = c.start_kill();
        }
    }
}

impl DaemonClient {
    /// 建立到 daemon 的连接（不重试「连接提前关闭」）。
    pub async fn connect() -> Result<Self, String> {
        Self::connect_with_retries(0).await
    }

    /// 建立到 daemon 的连接并配置「连接提前关闭」时的重建重试次数。
    pub async fn connect_with_retries(retries: u32) -> Result<Self, String> {
        // 自动激活：spawn 前台 daemon 子进程（stdio 管道承载 JSON-RPC）。
        // D-Bus/systemd activation 形态由 unit 层提供同名二进制；CLI 统一
        // 走 spawn 路径保证行为一致（首次查询 ~100ms 启动延迟可接受）。
        let exe = find_daemon_binary()?;
        let mut child = tokio::process::Command::new(&exe)
            .arg("--foreground")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn {exe}: {e}"))?;
        let stdin = child.stdin.take().ok_or("daemon child has no stdin")?;
        let stdout = child.stdout.take().ok_or("daemon child has no stdout")?;
        Ok(Self {
            child: Some(child),
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
            retries,
        })
    }

    /// 读取一行原始消息（响应与通知共享同一 stdio 行流）。
    async fn read_line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("daemon read: {e}"))?;
        if n == 0 {
            return Err(format!(
                "{CLOSED_EARLY} (hint: daemon exited early — possible portal/DBus \
                 session-permission failure or concurrent daemon startup; use --retry N \
                 or ensure an active graphical login session)"
            ));
        }
        // 半截响应：完整 JSON-RPC 行必以 `\n` 结尾（`to_line` 追加），读到 EOF
        // 仍无换行说明 daemon 在写完一行前退出——归入传输层错误供 `--retry`
        // 重建连接重试（TSI-2877 审查项 #2）。
        if !line.ends_with('\n') {
            return Err("daemon read: truncated response (EOF before newline)".into());
        }
        Ok(line)
    }

    /// 单次请求往返，带「连接提前关闭」重建重试（§19 短退避）。
    ///
    /// daemon 可能在写出响应前退出（单实例锁竞争、portal GetSession 失败
    /// 提前退出等）——此时按 `--retry N` 重建连接重试，避免偶发误报为
    /// 「产品无响应」。仅传输层「提前关闭」触发重试；RPC 错误（有 code）
    /// 与参数错误不重试。
    pub async fn call_rpc(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let mut attempt: u32 = 0;
        loop {
            match self.call_rpc_once(method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(CallError::Other(msg))
                    if is_transport_error(&msg) && attempt < self.retries =>
                {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MS)).await;
                    *self = Self::connect_with_retries(self.retries).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 单次请求往返（不重试）。响应与 `events.notify` 通知共享行流，通知无
    /// `id`——读到通知即跳过，直到拿到匹配本请求 id 的响应。
    async fn call_rpc_once(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = if params.is_null() {
            Request::without_params(id, method)
        } else {
            Request::new(id, method, params)
        };
        self.stdin
            .write_all(req.to_line().as_bytes())
            .await
            .map_err(|e| format!("daemon write: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("daemon flush: {e}"))?;

        loop {
            let line = self.read_line().await?;
            let msg: Value =
                serde_json::from_str(&line).map_err(|e| format!("bad message: {e}"))?;
            if msg.get("id").is_none() {
                // 通知（events.notify）不归属本请求。
                continue;
            }
            let resp = Response::from_line(&line)?;
            if resp.id != id {
                return Err(CallError::Other(format!(
                    "response id mismatch: got {} want {}",
                    resp.id, id
                )));
            }
            match (resp.result, resp.error) {
                (Some(v), None) => return Ok(v),
                (None, Some(err)) => {
                    return Err(CallError::Rpc {
                        code: err.code,
                        message: err.message,
                    })
                }
                _ => {
                    return Err(CallError::Other(
                        "malformed response: neither result nor error".into(),
                    ))
                }
            }
        }
    }

    /// 单次请求往返（字符串错误，兼容既有调用方）。
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.call_rpc(method, params)
            .await
            .map_err(|e| e.to_string())
    }

    /// 无参调用。
    pub async fn call0(&mut self, method_name: &str) -> Result<Value, String> {
        self.call(method_name, Value::Null).await
    }

    /// 订阅事件流：发送 `events.subscribe` 后持续读取，直到连接关闭。
    ///
    /// `subscriber_id` 响应打印一次；此后每条 `events.notify` 通知以
    /// pretty JSON 打印到 stdout，与普通查询命令的单行输出互不干扰。
    ///
    /// 流结束（EOF，daemon 退出或空闲超时）显式提示并 exit 0；真实 I/O
    /// 错误返回 Err 以非零退出（审查建议 2）。
    pub async fn subscribe(&mut self, filter: Option<String>) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        let params = filter
            .map(|f| json!({ "filter": f }))
            .unwrap_or(Value::Null);
        let req = if params.is_null() {
            Request::without_params(id, method::EVENTS_SUBSCRIBE)
        } else {
            Request::new(id, method::EVENTS_SUBSCRIBE, params)
        };
        self.stdin
            .write_all(req.to_line().as_bytes())
            .await
            .map_err(|e| format!("daemon write: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("daemon flush: {e}"))?;

        loop {
            let line = match self.read_line().await {
                Ok(l) => l,
                Err(e) if is_closed_early(&e) => {
                    eprintln!("event stream ended");
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match msg.get("id") {
                Some(vid) => {
                    if vid.as_u64() == Some(id) {
                        let resp = Response::from_line(&line)?;
                        match (resp.result, resp.error) {
                            (Some(r), None) => {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&r).unwrap_or_default()
                                );
                            }
                            (None, Some(err)) => {
                                return Err(format!("rpc error {}: {}", err.code, err.message))
                            }
                            _ => return Err("malformed response: neither result nor error".into()),
                        }
                    }
                }
                None => {
                    // 事件通知：打印后继续等待下一条。
                    println!("{}", serde_json::to_string_pretty(&msg).unwrap_or_default());
                }
            }
        }
    }
}

/// 传输层「连接提前关闭（EOF）」判定——`read_line` 收到 EOF（daemon 干净退出）。
/// `subscribe` 据此判定事件流正常结束（exit 0）；其余传输错误（broken pipe /
/// 读 I/O 错误）走 [`is_transport_error`] 归入 `--retry` 重试判定。
fn is_closed_early(msg: &str) -> bool {
    msg.contains(CLOSED_EARLY)
}

/// 传输层错误判定——daemon 连接在请求往返期间断开，均可重建连接重试：
/// - 写侧/读侧 pipe 错误（`daemon write:` / `daemon flush:` / `daemon read:`）；
/// - 读侧 EOF（`is_closed_early`）。
///
/// 与 RPC 错误（有 code）及协议错误（解析失败 / 响应 id 不符 / malformed）
/// 区分——后者不重建连接重试。
fn is_transport_error(msg: &str) -> bool {
    msg.starts_with("daemon write:")
        || msg.starts_with("daemon flush:")
        || msg.starts_with("daemon read:")
        || is_closed_early(msg)
}
/// 定位 daemon 二进制（委托 agent-shell-rpc::daemon_bin）。
///
/// workspace target 目录用 `CARGO_MANIFEST_DIR` 拼绝对路径，不依赖 CWD
/// （TSI-2471 审查 #2）。
fn find_daemon_binary() -> Result<String, String> {
    // cli/ 上一级是 workspace 根，target/ 在根下。
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
// ───────────────────────── 高层操作封装 ─────────────────────────

impl DaemonClient {
    pub async fn doctor(&mut self) -> Result<agent_shell_rpc::DoctorResult, String> {
        let v = self.call0(method::DOCTOR).await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    pub async fn info(&mut self) -> Result<agent_shell_rpc::InfoResult, String> {
        let v = self.call0(method::INFO).await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    pub async fn windows_list(
        &mut self,
        filter: Option<String>,
        match_mode: Option<&str>,
    ) -> Result<(Vec<agent_shell_rpc::WindowEntry>, bool), String> {
        let mut params = serde_json::Map::new();
        if let Some(f) = filter {
            params.insert("filter".into(), json!(f));
        }
        if let Some(m) = match_mode {
            params.insert("match".into(), json!(m));
        }
        let params = if params.is_empty() {
            Value::Null
        } else {
            Value::Object(params)
        };
        let v = self.call(method::WINDOWS_LIST, params).await?;
        #[derive(serde::Deserialize)]
        struct W {
            windows: Vec<agent_shell_rpc::WindowEntry>,
            from_cache: bool,
        }
        let w: W = serde_json::from_value(v).map_err(|e| e.to_string())?;
        Ok((w.windows, w.from_cache))
    }

    pub async fn window_info(&mut self, target: &str) -> Result<Value, String> {
        self.call(method::WINDOW_INFO, json!({ "target": target }))
            .await
    }

    /// 窗口目标解析在 CLI 完成（纯文本逻辑），native_id 经 RPC 提交。
    ///
    /// 返回解析出的 native_id 列表供 resolve_target 匹配后选择。
    pub async fn window_op(
        &mut self,
        op: agent_shell_rpc::WindowOpKind,
        native_id: &str,
        geo: [i32; 4],
    ) -> Result<(), String> {
        self.call(
            method::WINDOW_OP,
            json!({
                "op": op,
                "target": native_id,
                "x": geo[0], "y": geo[1], "width": geo[2], "height": geo[3],
            }),
        )
        .await
        .map(|_| ())
    }

    pub async fn workspaces_list(&mut self) -> Result<Vec<String>, String> {
        let v = self.call0(method::WORKSPACES_LIST).await?;
        let arr = v
            .get("workspaces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(arr
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect())
    }

    pub async fn workspace_switch(&mut self, id: &str) -> Result<(), String> {
        self.call(method::WORKSPACE_SWITCH, json!({ "id": id }))
            .await
            .map(|_| ())
    }

    pub async fn input(
        &mut self,
        kind: agent_shell_rpc::InputKind,
        payload: Value,
    ) -> Result<(), String> {
        self.call(
            method::INPUT_SEND,
            json!({ "kind": kind, "payload": payload }),
        )
        .await
        .map(|_| ())
    }

    pub async fn screenshot(
        &mut self,
        window: Option<String>,
        area: Option<[i32; 4]>,
        output_path: &str,
    ) -> Result<agent_shell_rpc::CaptureResult, String> {
        let v = self
            .call(
                method::SCREENSHOT_CAPTURE,
                json!({ "window": window, "area": area, "output_path": output_path }),
            )
            .await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    pub async fn a11y_status(&mut self) -> Result<agent_shell_rpc::A11yStatusResult, String> {
        let v = self.call0(method::A11Y_STATUS).await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    pub async fn a11y_query(
        &mut self,
        role: Option<String>,
        name: Option<String>,
        all: bool,
    ) -> Result<agent_shell_rpc::A11yQueryResult, String> {
        let v = self
            .call(
                method::A11Y_QUERY,
                a11y_query_params(role.as_deref(), name.as_deref(), all),
            )
            .await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }
}

/// `a11y.query` 参数构造：`None` 条件省略该键，而非序列化为 JSON `null`；
/// `all=true` 显式插入 `"all": true`，`all=false` 保持省略键约定。
///
/// daemon 侧把缺省键与 `null` 值均视为「未提供」，但 CLI 省略键更忠实于
/// 「单条件查询」语义，且不依赖双方对 null 解释的一致性。
fn a11y_query_params(role: Option<&str>, name: Option<&str>, all: bool) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(r) = role {
        params.insert("role".into(), json!(r));
    }
    if let Some(n) = name {
        params.insert("name".into(), json!(n));
    }
    if all {
        params.insert("all".into(), json!(true));
    }
    Value::Object(params)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// CLI 注入的 target 目录用 CARGO_MANIFEST_DIR 拼绝对路径——CWD 无关
    /// （TSI-2471 审查 #2）。验证候选目录列表中包含基于
    /// CARGO_MANIFEST_DIR 的 target/debug 与 target/release 绝对路径。
    #[test]
    fn target_dirs_are_absolute_via_cargo_manifest_dir() {
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

    /// `a11y.query` 单条件调用：未提供的条件必须省略键，
    /// 而非序列化为 JSON `null`（TSI-2480 QA 回归锚定）。
    #[test]
    fn a11y_query_params_omits_none_keys() {
        let v = a11y_query_params(Some("pushButton"), None, false);
        let obj = v.as_object().expect("params must be object");
        assert_eq!(obj.get("role").and_then(|v| v.as_str()), Some("pushButton"));
        assert!(!obj.contains_key("name"), "None name must be omitted");

        let v = a11y_query_params(None, Some("foo"), false);
        let obj = v.as_object().expect("params must be object");
        assert_eq!(obj.get("name").and_then(|v| v.as_str()), Some("foo"));
        assert!(!obj.contains_key("role"), "None role must be omitted");
    }

    /// `all=true` 显式插入 `"all": true`；`all=false` 保持省略键约定。
    #[test]
    fn a11y_query_params_all_includes_flag() {
        let v = a11y_query_params(None, None, true);
        let obj = v.as_object().expect("params must be object");
        assert_eq!(obj.get("all").and_then(|v| v.as_bool()), Some(true));

        let v = a11y_query_params(None, None, false);
        let obj = v.as_object().expect("params must be object");
        assert!(!obj.contains_key("all"), "all=false must be omitted");
    }

    /// `is_closed_early` 只命中读侧 EOF（daemon 干净退出）；`subscribe` 据此
    /// 判流结束，不把 broken pipe 误判为流结束。
    #[test]
    fn is_closed_early_matches_only_eof() {
        assert!(is_closed_early(
            "daemon closed connection before responding (hint: ...)"
        ));
        assert!(!is_closed_early("daemon write: broken pipe"));
        assert!(!is_closed_early("rpc error 1002: backend unavailable"));
        assert!(!is_closed_early(""));
    }

    /// `is_transport_error` 必须命中写侧/读侧 pipe 传输错误与 EOF——daemon 在
    /// 请求期间退出时三者同属可重建连接重试的失败模式（TSI-2877 审查项 #1），
    /// 不得命中协议错误（响应 id 不符 / 解析失败）。
    #[test]
    fn is_transport_error_matches_pipe_failures_not_protocol() {
        assert!(is_transport_error(
            "daemon write: broken pipe (os error 32)"
        ));
        assert!(is_transport_error("daemon flush: broken pipe"));
        assert!(is_transport_error("daemon read: broken pipe"));
        // 半截响应：daemon 写出半行后退出（EOF 无换行）→ read_line 归一为
        // 传输层错误，`--retry` 必须命中（TSI-2877 审查项 #2）。
        assert!(is_transport_error(
            "daemon read: truncated response (EOF before newline)"
        ));
        assert!(is_transport_error(
            "daemon closed connection before responding (hint: ...)"
        ));
        assert!(!is_transport_error("response id mismatch: got 1 want 2"));
        assert!(!is_transport_error("bad message: expected value"));
        assert!(!is_transport_error("rpc error 1002: backend unavailable"));
    }
}
