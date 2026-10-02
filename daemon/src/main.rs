//! agent-shell-daemon 入口（设计文档 §22.2 D1 / §23.2）。
//!
//! 两种接入形态，共用同一请求服务循环（见 [`serve`]）：
//! - 默认 stdio：从 inherited stdin 逐行读 JSON-RPC、逐行写响应，stdin EOF 或
//!   空闲超时（默认 30min，`--idle-timeout-secs` 可配置）即退出。CLI/MCP 经
//!   fork/exec 本二进制并以 stdio 管道承载协议——瞬态子进程随父进程退出
//!   （stdin 挂断）自行终止，不残留孤儿：挂断由 [`spawn_client_watchdog`] 全程
//!   观察，与请求循环所处阶段无关（socket 形态的 stdin 不是协议通道，不安装）。
//! - `--socket <path>`：显式 Unix socket 端点，常驻服务多条连接，同样按空闲
//!   超时退出，正常退出时回收绑定路径（见 [`serve::BoundSocket`]）。供无图形
//!   会话（QA/调试）用 `agent-shell --socket <path>` 直连。
//!
//! 单实例锁默认走 `$XDG_RUNTIME_DIR/agent-shell.lock`；`--lock-path` 与
//! `AGENT_SHELL_LOCK` 提供隔离命名空间，`--no-lock` 完全跳过（并行测试实例）。
//!
//! 手动调试 stdio 形态时用管道保持 stdin 打开即可维持运行
//! （`tail -f /dev/null | agent-shell-daemon`）；`/dev/null`、重定向或后台等
//! 非交互 stdin 的 EOF 由内核直接给出（读返回 0 / `POLLHUP`），但 stdin 服务
//! 循环排在单实例锁之后，EOF 要等锁判定完才被看见：已有实例持锁时本进程先等满
//! `LOCK_WAIT_TIMEOUT`（30s）再以 exit 1 报「daemon already running」，锁空闲
//! 时才随即因 EOF 退出（exit 0）。

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
        // stderr 在 stdio 形态下是调用方（CLI/MCP）持有的管道，客户端一死写日志
        // 必然失败。tracing-subscriber 默认的 `log_internal_errors` 会用
        // `eprintln!` 回退报告该失败——同样打在断裂/写满的 stderr 上，
        // `eprintln!` 直接 panic，把调用线程炸掉（退出路径因此执行不到
        // `std::process::exit`）。
        //
        // 这是**全局**开关，关掉的是 tracing 自身的「写不出去 / 格式化失败」内部
        // 上报（非业务日志），代价是全生命周期都不再上报：对 stderr 随客户端生灭
        // 的 daemon，写入失败是预期形态，升级成 panic 才是错误。退出路径已改用非
        // 阻塞的 [`log_exit`]，不依赖本开关。
        .log_internal_errors(false)
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(run(args));
    // 退出路径不得等待仍挂起的 stdin 阻塞读：`Runtime` 析构会 join 阻塞任务，
    // stdio 形态下客户端仍连着（空闲超时到达、订阅方还在等事件）时进程卡在析构
    // 里永不退出并一直持锁——其后每条 CLI 命令都要等满锁超时才报
    // `daemon already running`。
    runtime.shutdown_background();
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
    // stdio 形态是 CLI/MCP fork/exec 的瞬态子进程：stdin 写端只由调用方持有，
    // 其挂断即客户端消亡。看门狗先于单实例锁与后端装配启动——客户端在锁等待、
    // 装配或长请求期间消失时也必须结束进程，否则进程带着单实例锁残留。
    if args.socket.is_none() {
        spawn_client_watchdog();
    }
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

/// 客户端存活看门狗（仅 stdio 形态）：daemon 的 stdin 写端只由启动方（CLI/MCP）
/// 持有，其关闭即客户端消亡。请求循环只在间隙读 stdin——daemon 卡在锁等待、后端
/// 装配或长请求里时 EOF 无人观察，进程会带着单实例锁残留（其后每条 CLI 命令等满
/// 锁超时后报 `daemon already running`）。本线程不消费任何字节，与请求循环并行。
///
/// 线程本身不参与退出同步：正常退出路径由 [`main`] 显式结束进程，无需唤醒它。
/// 退出诊断走非阻塞的 [`log_exit`]——stderr 读端仍在但不排空（管道写满）时阻塞写
/// 会把 `std::process::exit` 拖住，退出路径不得依赖 stderr 的读取方。
fn spawn_client_watchdog() {
    let spawned = std::thread::Builder::new()
        .name("client-watchdog".into())
        .spawn(|| loop {
            // `events = 0`：不请求可读性，仅挂断/错误能让 `poll` 返回——管道上
            // 有未读数据也不忙轮询（未请求的可读位不上报）。POLLHUP 由内核在
            // 写端全部关闭时置位，与是否还有未读数据无关，故客户端死亡必命中。
            let mut pfd = libc::pollfd {
                fd: 0,
                events: 0,
                revents: 0,
            };
            // SAFETY: fd 0 是本进程 stdin（启动方以管道承载协议）；`poll` 只观察
            // 该 fd 状态，不读走字节、不涉及内存。
            let n = unsafe { libc::poll(&mut pfd, 1, -1) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                // EINTR 重试；其余错误无法继续观察挂断——返回，让请求循环按原
                // 语义处理 EOF（本线程缺席不影响进程退出）。
                if err.kind() != std::io::ErrorKind::Interrupted {
                    tracing::warn!("client watchdog stopped: poll failed: {err}");
                    return;
                }
                continue;
            }
            log_exit("stdin hangup; exiting");
            std::process::exit(0);
        });
    if let Err(e) = spawned {
        tracing::warn!("client watchdog thread failed to start: {e}");
    }
}

/// 退出路径的日志：非阻塞尽力写 stderr（`serve` 的瞬态连接与看门狗共用）。
///
/// 退出路径不能走 tracing（阻塞写）：stderr 读端仍在但不排空时写满管道会让调用
/// 线程无限阻塞，`std::process::exit` 执行不到，进程带着单实例锁残留——正是本
/// 模块要防的故障形态。这里 dup fd 2、置 `O_NONBLOCK` 后写一次：管道满（EAGAIN）
/// 或无读者（EPIPE；Rust 运行时已忽略 SIGPIPE）都立即返回，写不出去即丢弃。
/// 消息保持纯文本、单行——只求不阻塞、不 panic。写入时自动补行尾 `\n`：多轮瞬态
/// daemon 的退出消息各自成行，不会在 stderr 上粘成一行、与 shell 提示符相接。
pub(crate) fn log_exit(msg: &str) {
    // SAFETY: 只操作本进程的 stderr；dup 出的 fd 在本次调用内关闭，不与其它线程
    // 共享；写入缓冲区是本函数的有效切片，长度与指针同源。
    unsafe {
        let fd = libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 3);
        if fd < 0 {
            return;
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        libc::write(fd, msg.as_ptr().cast(), msg.len());
        libc::write(fd, b"\n".as_ptr().cast(), 1);
        libc::close(fd);
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
