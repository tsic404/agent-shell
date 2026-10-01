//! agent-shell-daemon 入口（设计文档 §22.2 D1 / §23.2）。
//!
//! 两种接入形态，共用同一请求服务循环（见 [`serve`]）：
//! - 默认 stdio：从 inherited stdin 逐行读 JSON-RPC、逐行写响应，stdin EOF 或
//!   空闲超时（默认 30min，`--idle-timeout-secs` 可配置）即退出。CLI/MCP 经
//!   fork/exec 本二进制并以 stdio 管道承载协议——瞬态子进程随父进程退出
//!   （stdin EOF）自行终止，不残留孤儿。
//! - `--socket <path>`：显式 Unix socket 端点，常驻服务多条连接，同样按空闲
//!   超时退出，正常退出时回收绑定路径（见 [`serve::BoundSocket`]）。供无图形
//!   会话（QA/调试）用 `agent-shell --socket <path>` 直连。
//!
//! 单实例锁默认走 `$XDG_RUNTIME_DIR/agent-shell.lock`；`--lock-path` 与
//! `AGENT_SHELL_LOCK` 提供隔离命名空间，`--no-lock` 完全跳过（并行测试实例）。
//!
//! 手动调试 stdio 形态时用管道保持 stdin 打开即可维持运行
//! （`tail -f /dev/null | agent-shell-daemon`）；`/dev/null`、重定向或后台等
//! 非交互 stdin 会立即 EOF 退出。

mod a11y;
mod capture;
mod dispatch;
mod ime_session;
mod input;
mod path_guard;
mod portal_sessions;
mod rootd_client;
mod serve;
mod single_instance;
mod state;

use state::Daemon;
use std::path::PathBuf;
use std::time::Duration;

/// 空闲超时默认值：30min（§22.2 激活策略）。
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 30 * 60;

const USAGE: &str = "usage: agent-shell-daemon [--idle-timeout-secs N] [--socket PATH] \
                     [--lock-path PATH] [--no-lock]";

/// daemon 命令行参数。
#[derive(Debug, PartialEq)]
struct Args {
    idle_timeout: Duration,
    /// Unix socket 端点；`None` = stdio 形态。
    socket: Option<PathBuf>,
    /// 单实例锁路径；`None` = `AGENT_SHELL_LOCK` env 或默认路径。
    lock_path: Option<PathBuf>,
    /// 跳过单实例锁（并行测试/显式隔离实例）。
    no_lock: bool,
}

fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}\n{USAGE}");
            std::process::exit(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agent_shell_daemon=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(run(args));
}

/// 解析命令行参数。
///
/// 未知参数、缺失值或不可解析的值统一走 `Err`，由调用方以 exit 2 报错——
/// 与 `unknown arg` 行为一致。不可解析值不得静默回退默认值。
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
        args.next()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("missing value for {flag}"))
    }

    let mut parsed = Args {
        idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        socket: None,
        lock_path: None,
        no_lock: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--idle-timeout-secs" => {
                let raw = value(&mut args, "--idle-timeout-secs")?;
                parsed.idle_timeout = Duration::from_secs(
                    raw.parse()
                        .map_err(|_| format!("invalid --idle-timeout-secs value: {raw}"))?,
                );
            }
            "--socket" => parsed.socket = Some(PathBuf::from(value(&mut args, "--socket")?)),
            "--lock-path" => {
                parsed.lock_path = Some(PathBuf::from(value(&mut args, "--lock-path")?))
            }
            "--no-lock" => parsed.no_lock = true,
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    Ok(parsed)
}

async fn run(args: Args) {
    // 单实例锁（§22.2 D1）：默认会话级锁；`--lock-path`/`AGENT_SHELL_LOCK`
    // 换到独立命名空间（同路径仍互斥），`--no-lock` 完全跳过供测试。
    let lock = if args.no_lock {
        tracing::warn!("single-instance lock disabled (--no-lock)");
        None
    } else {
        let path = single_instance::resolve_lock_path(args.lock_path.as_deref());
        match single_instance::SingleInstanceLock::acquire_at_path(&path) {
            Ok(lock) => Some(lock),
            Err(msg) => {
                eprintln!("error: {msg}");
                std::process::exit(1);
            }
        }
    };

    // 端点先绑定再装配：路径冲突（活跃端点、不可写目录）在建立桌面连接之前失败。
    let endpoint = match args.socket.as_deref() {
        Some(path) => match serve::bind_socket(path) {
            Ok(endpoint) => Some(endpoint),
            Err(msg) => {
                eprintln!("error: {msg}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    // 版本自报：`<crate-version> (<git-commit>)`（§20.7 QA 二进制版本约定）。
    tracing::info!(
        "agent-shell-daemon {} ({}) starting",
        env!("CARGO_PKG_VERSION"),
        env!("AGENT_SHELL_GIT_COMMIT")
    );
    if let Some(path) = args.socket.as_deref() {
        tracing::info!(socket = %path.display(), "serving on explicit unix socket endpoint");
    }
    // 连接来源说明（§22.2 激活策略）：CLI/MCP fork/exec 本二进制经 stdio 管道
    // 承载 JSON-RPC；LISTEN_FDs (systemd socket activation) — Phase 3 待接线：
    // 当前仅记录检测到 LISTEN_FDs 的存在，但不使用 fd 接受连接。这是有意限制：
    // socket-activated 监听需先完成 daemon D-Bus 服务注册（见 §22.2 激活策略），
    // 否则单连接 stdio 服务无法与多连接 socket 模型共存。
    if std::env::var("LISTEN_FDS")
        .map(|v| v != "0")
        .unwrap_or(false)
    {
        tracing::info!("LISTEN_FDs present but unused (socket activation wiring pending)");
    }

    let daemon = Daemon::connect(args.idle_timeout).await;
    // 空闲超时以 daemon 装配值为准（§22.2：无请求达超时即退，systemd
    // `Restart=on-failure` 语义下正常退出不重启）。
    let idle_timeout = daemon.idle_timeout;
    match endpoint {
        Some(endpoint) => serve::serve_socket(endpoint, daemon, idle_timeout).await,
        None => serve::serve_stdio(daemon, idle_timeout).await,
    }

    // 释放单实例锁
    if let Some(lock) = lock {
        lock.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_without_args() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.idle_timeout, Duration::from_secs(1800));
        assert_eq!(args.socket, None);
        assert_eq!(args.lock_path, None);
        assert!(!args.no_lock);
    }

    #[test]
    fn explicit_idle_value_wins() {
        assert_eq!(
            parse(&["--idle-timeout-secs", "60"]).unwrap().idle_timeout,
            Duration::from_secs(60)
        );
    }

    #[test]
    fn unparseable_idle_value_errors() {
        let err = parse(&["--idle-timeout-secs", "abc"]).unwrap_err();
        assert!(
            err.contains("abc"),
            "error should name the bad value: {err}"
        );
    }

    #[test]
    fn missing_idle_value_errors() {
        assert!(parse(&["--idle-timeout-secs"]).is_err());
    }

    #[test]
    fn negative_idle_value_rejected_as_unparseable() {
        assert!(parse(&["--idle-timeout-secs", "-1"]).is_err());
    }

    #[test]
    fn socket_flag_carries_explicit_path() {
        let args = parse(&["--socket", "/run/user/1000/agent-shell-qa.sock"]).unwrap();
        assert_eq!(
            args.socket,
            Some(PathBuf::from("/run/user/1000/agent-shell-qa.sock"))
        );
    }

    #[test]
    fn lock_path_and_no_lock_flags() {
        let args = parse(&["--lock-path", "/tmp/qa.lock", "--no-lock"]).unwrap();
        assert_eq!(args.lock_path, Some(PathBuf::from("/tmp/qa.lock")));
        assert!(args.no_lock);
    }

    #[test]
    fn flags_accept_any_order() {
        let args = parse(&[
            "--no-lock",
            "--socket",
            "s.sock",
            "--idle-timeout-secs",
            "5",
        ])
        .unwrap();
        assert_eq!(args.socket, Some(PathBuf::from("s.sock")));
        assert!(args.no_lock);
        assert_eq!(args.idle_timeout, Duration::from_secs(5));
    }

    #[test]
    fn missing_value_errors_name_the_flag() {
        for flag in ["--socket", "--lock-path"] {
            let err = parse(&[flag]).unwrap_err();
            assert!(err.contains(flag), "error should name the flag: {err}");
        }
    }

    #[test]
    fn empty_value_errors_name_the_flag() {
        let err = parse(&["--socket", ""]).unwrap_err();
        assert!(
            err.contains("--socket"),
            "empty value must not bind a path: {err}"
        );
    }

    #[test]
    fn unknown_arg_errors() {
        let err = parse(&["--bogus"]).unwrap_err();
        assert!(err.contains("unknown arg"));
    }
}
