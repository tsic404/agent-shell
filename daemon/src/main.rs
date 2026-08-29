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
mod ime_session;
mod input;
mod portal_sessions;
mod rootd_client;
mod single_instance;
mod state;

use agent_shell_rpc::{Notification, Request, Response};
use state::Daemon;
use std::sync::Arc;
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
    // 单实例锁（§22.2 D1）——失败说明已有 daemon 运行。
    let lock = match single_instance::SingleInstanceLock::acquire() {
        Ok(l) => l,
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    };
    // 连接来源说明（§22.2 激活策略）：
    // - systemd --user 常驻形态：CLI 经 fork/exec `--foreground` 子进程建立
    //   stdio 管道连接；unit 常驻实例的 stdin=null，不承载协议。
    // - 手动管道/测试：stdin/stdout 即协议通道。
    // LISTEN_FDs (systemd socket activation) — Phase 3 待接线：当前仅记录检测
    // 到 LISTEN_FDs 的存在，但不使用 fd 接受连接，统一从 inherited stdio 读取。
    // 这是有意限制：socket-activated 监听需先完成 daemon D-Bus 服务注册
    // （见 §22.2 激活策略），否则单连接 stdio 服务无法与多连接 socket 模型共存。
    if std::env::var("LISTEN_FDS")
        .map(|v| v != "0")
        .unwrap_or(false)
    {
        tracing::info!("LISTEN_FDs present but unused (socket activation wiring pending); serving on inherited stdio");
    }

    let daemon = Daemon::connect(idle_timeout).await;
    let idle = daemon.idle_timeout;
    serve_connection(daemon, idle).await;

    // 释放单实例锁
    lock.release();
}

/// 单连接服务循环：逐行读请求 → dispatch → 写响应。EOF 或空闲超时退出。
async fn serve_connection(mut daemon: Daemon, idle_timeout: Duration) {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin);
    // 响应与通知共用同一 stdout 行流；订阅转发任务独立 spawn，必须共享
    // 一个互斥 writer，否则并发写入会交织半行。tokio Mutex 跨 await 持有。
    let out = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
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
                write_line(&out, resp.to_line()).await;
                continue;
            }
        };
        let resp = dispatch::dispatch(&mut daemon, &req).await;
        write_line(&out, resp.to_line()).await;

        // events.subscribe 的返回订阅句柄被 dispatch 暂存在 daemon；此处取走
        // 并 spawn 转发任务。订阅句柄 channel 关闭（unsubscribe）时任务退出。
        let subs = std::mem::take(&mut daemon.subscriptions);
        for mut sub in subs {
            let out = Arc::clone(&out);
            tokio::spawn(async move {
                while let Some(evt) = sub.recv().await {
                    let params = match serde_json::to_value(evt) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!("event serialize failed: {e}");
                            continue;
                        }
                    };
                    write_line(&out, Notification::event(params).to_line()).await;
                }
            });
        }
    }

    // 退出前关闭 portal ScreenCast 会话（D-Bus Close，审查项 #6）。
    if let Some(capture) = daemon.capture.as_ref() {
        capture.shutdown().await;
    }
}

async fn write_line(out: &Arc<tokio::sync::Mutex<tokio::io::Stdout>>, line: String) {
    let mut stdout = out.lock().await;
    let _ = stdout.write_all(line.as_bytes()).await;
    let _ = stdout.flush().await;
}
