//! agent-shell-daemon 入口（设计文档 §22.2 D1 / §23.2）。
//!
//! 三种运行模式：
//! 1. **systemd --user 常驻**（默认）：stdin/stdout 为 socket-activated
//!    连接（`Accept=no` + `StandardInput=socket` 由 unit 层接线）；本实现
//!    以「每连接一协程，行分隔 JSON-RPC」服务。
//! 2. **空闲超时退出**（默认 30min，`--idle-timeout-secs` 可配置）。
//! 3. **前台调试**：`--foreground` 直接在当前终端 stdio 上服务。

mod a11y;
mod capture;
mod dispatch;
mod input;
mod state;

use agent_shell_rpc::{Request, Response};
use state::Daemon;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn main() {
    let mut idle_secs: u64 = 30 * 60;
    let mut foreground = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--idle-timeout-secs" => {
                idle_secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(1800)
            }
            "--foreground" => foreground = true,
            other => {
                eprintln!("unknown arg: {other} (usage: agent-shell-daemon [--foreground] [--idle-timeout-secs N])");
                std::process::exit(2);
            }
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agent_shell_daemon=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(run(Duration::from_secs(idle_secs), foreground));
}

async fn run(idle_timeout: Duration, _foreground: bool) {
    // 连接来源说明（§22.2 激活策略）：
    // - systemd --user 常驻形态：CLI 经 fork/exec `--foreground` 子进程建立
    //   stdio 管道连接；unit 常驻实例的 stdin=null，不承载协议。
    // - 手动管道/测试：stdin/stdout 即协议通道。
    // LISTEN_FDs socket-activation 留待 daemon D-Bus 服务注册任务接线；
    // 当前统一从 inherited stdio 读取，检测到 LISTEN_FDs 时如实记录。
    if std::env::var("LISTEN_FDS")
        .map(|v| v != "0")
        .unwrap_or(false)
    {
        tracing::info!("LISTEN_FDs present but unused (socket activation wiring pending); serving on inherited stdio");
    }

    let daemon = Daemon::connect(idle_timeout).await;
    let idle = daemon.idle_timeout;
    serve_connection(daemon, idle).await;
}

/// 单连接服务循环：逐行读请求 → dispatch → 写响应。EOF 或空闲超时退出。
async fn serve_connection(mut daemon: Daemon, idle_timeout: Duration) {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin);
    loop {
        // 空闲超时退出（§22.2 激活策略）：连接上无请求达 idle_timeout 即退，
        // systemd `Restart=on-failure` 语义下正常退出不重启。
        let mut line = String::new();
        let n = match tokio::time::timeout(idle_timeout, lines.read_line(&mut line)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                tracing::warn!("read error: {e}; exiting");
                break;
            }
            Err(_elapsed) => {
                tracing::info!("idle timeout reached; exiting");
                break;
            }
        };
        if n == 0 {
            tracing::info!("stdin closed; exiting");
            break;
        }

        let req = match Request::from_line(&line) {
            Ok(r) => r,
            Err(e) => {
                // 解析失败：id 不可知，回 id=0 的 ParseError（规范允许）。
                let resp = Response::err(0, agent_shell_rpc::RpcErrorCode::ParseError, e);
                write_response(resp).await;
                continue;
            }
        };
        let resp = dispatch::dispatch(&mut daemon, &req).await;
        write_response(resp).await;
    }

    // 退出前关闭 portal ScreenCast 会话（D-Bus Close，审查项 #6）。
    if let Some(capture) = daemon.capture.as_ref() {
        capture.shutdown().await;
    }
}

async fn write_response(resp: Response) {
    let mut stdout = tokio::io::stdout();
    let _ = stdout.write_all(resp.to_line().as_bytes()).await;
    let _ = stdout.flush().await;
}
