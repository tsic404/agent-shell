//! 混合版本跨进程 e2e 夹具：模拟 pre-21c7267 旧 daemon 的 `info.show`
//! 线格式（TSI-2873）。
//!
//! 旧 daemon 的 `InfoResult.capabilities` 是 `Vec<(String, bool)>`（布尔），
//! 新线格式是 snake_case 字符串。daemon 常驻用户会话、postinst 升级不重启，
//! 新 CLI 反序列化旧 daemon 的布尔时不得硬失败——本夹具以真实进程边界
//! 复现该旧线格式，供 `cli/tests/mixed_version_e2e.rs` 用真实 `agent-shell`
//! 二进制连接后验证。
//!
//! 夹具协议：从 stdin 逐行读 JSON-RPC 请求，对 `info.show` 回旧布尔格式
//! 响应，其余方法回 method-not-found；EOF 或读错误即退出。CLI 以
//! `--foreground` 拉起本夹具，参数被忽略。

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut lines = stdin.lock().lines();
    let mut out = stdout.lock();

    for line in lines.by_ref() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => break,
        };
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let resp = if method == "info.show" {
            legacy_info_response(id)
        } else {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not found" },
            })
        };
        // 写失败（父进程已退出）即终止，不 panic。
        if writeln!(out, "{resp}").is_err() || out.flush().is_err() {
            break;
        }
    }
}

/// 旧 daemon（pre-21c7267）的 `info.show` 响应：`capabilities` 为布尔值。
///
/// 能力名与顺序复刻旧 `daemon/src/dispatch.rs` 的 `info` 实现（9 行固定），
/// 值取布尔混合以覆盖 true→Enabled、false→Disabled 两条反序列化分支。
fn legacy_info_response(id: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "detection": "legacy daemon (pre-21c7267 bool capabilities)",
            "capabilities": [
                ["window_management", true],
                ["workspace_management", false],
                ["monitor_layout", false],
                ["window_events", false],
                ["workspace_events", false],
                ["native_input", true],
                ["native_capture", false],
                ["virtual_desktops", false],
                ["effects_control", false],
            ],
        },
    })
}
