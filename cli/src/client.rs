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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 一个 daemon 连接上的客户端会话。
pub struct DaemonClient {
    child: Option<tokio::process::Child>,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
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
    /// 建立到 daemon 的连接并完成握手探测。
    pub async fn connect() -> Result<Self, String> {
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
        })
    }

    /// 单次请求往返。
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
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

        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("daemon read: {e}"))?;
        if n == 0 {
            return Err("daemon closed connection before responding".into());
        }
        let resp = Response::from_line(&line)?;
        if resp.id != id {
            return Err(format!("response id mismatch: got {} want {}", resp.id, id));
        }
        match (resp.result, resp.error) {
            (Some(v), None) => Ok(v),
            (None, Some(err)) => Err(format!("rpc error {}: {}", err.code, err.message)),
            _ => Err("malformed response: neither result nor error".into()),
        }
    }

    /// 无参调用。
    pub async fn call0(&mut self, method_name: &str) -> Result<Value, String> {
        self.call(method_name, Value::Null).await
    }
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
    ) -> Result<(Vec<agent_shell_rpc::WindowEntry>, bool), String> {
        let params = filter
            .map(|f| json!({ "filter": f }))
            .unwrap_or(Value::Null);
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
}
