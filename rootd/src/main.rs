//! agent-shell-rootd 入口：system bus 上的 D-Bus 服务（§23.2/§23.4.3）。
//!
//! systemd system unit（`agent-shell-rootd.service`）启动；daemon 经
//! system bus 调用白名单方法，polkit 按 §23.4.2 action 精确授权。
//!
//! 运行模式：
//! 1. **D-Bus 服务**（默认）：注册 `org.agentshell.Rootd` 于 system bus，
//!    方法经 polkit 校验后分派到 lib `dispatch`。
//! 2. **pkexec 子命令**（`agent-shell-rootd pkexec <cmd>...`）：
//!    §23.4.1 通道 3 兜底——polkit action `com.agentshell.pkexec.install-package`
//!    授权后执行一次性命令。

use tokio::signal::unix::{signal, SignalKind};

use std::process::ExitCode;
fn main() -> ExitCode {
    let version = env!("CARGO_PKG_VERSION");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agent_shell_rootd=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    // pkexec 子命令模式（§23.4.1 通道 3）
    if args.len() > 1 && args[1] == "pkexec" {
        return run_pkexec(&args[2..]);
    }

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        if let Err(e) = run_dbus_service().await {
            eprintln!("agent-shell-rootd {version}: D-Bus service error: {e}");
            std::process::exit(1);
        }
    });
    ExitCode::SUCCESS
}

/// D-Bus 服务模式：注册 org.agentshell.Rootd 于 system bus。
///
/// 错误类型用 `Box<dyn std::error::Error>`——快速实现足够；生产环境
/// 可用 `thiserror` 定义具体错误类型，但不影响功能。
async fn run_dbus_service() -> Result<(), Box<dyn std::error::Error>> {
    let version = env!("CARGO_PKG_VERSION");
    eprintln!("agent-shell-rootd {version} starting (system bus service)");

    // 连接 system bus
    let connection = zbus::Connection::system().await?;
    eprintln!("agent-shell-rootd: connected to system bus");

    // 注册 D-Bus 服务接口
    let iface = agent_shell_rootd::dbus::RootdInterface::new();
    let _ = connection
        .object_server()
        .at("/org/agentshell/Rootd", iface)
        .await?;

    // 请求 well-known name
    connection.request_name("org.agentshell.Rootd").await?;
    eprintln!("agent-shell-rootd: registered org.agentshell.Rootd on /org/agentshell/Rootd");

    // 获取 InterfaceRef 用于信号驱动
    let iface_ref = connection
        .object_server()
        .interface::<_, agent_shell_rootd::dbus::RootdInterface>("/org/agentshell/Rootd")
        .await?;

    // 信号驱动循环（JobProgress/JobDone）
    let connection_clone = connection.clone();
    tokio::spawn(async move {
        agent_shell_rootd::dbus::drive_signals(&connection_clone, iface_ref).await;
    });

    // 等待 SIGTERM/SIGINT
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => eprintln!("agent-shell-rootd: SIGTERM"),
        _ = int.recv() => eprintln!("agent-shell-rootd: SIGINT"),
    }
    eprintln!("agent-shell-rootd {version} shutting down");
    Ok(())
}

/// pkexec 子命令模式（§23.4.1 通道 3）。
///
/// polkit action `com.agentshell.pkexec.install-package` 已在前置授权
/// （pkexec 调用链）。为防止安全语义不匹配（action 名为 install-package
/// 却允许执行任意命令），此处只允许包管理器二进制通过白名单。
/// 非白名单命令拒绝执行——pkexec 通道仅用于软件包安装兜底。
fn run_pkexec(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: agent-shell-rootd pkexec <command> [args...]");
        return ExitCode::from(2);
    }
    // 命令白名单——仅允许包管理器二进制（§23.4.1 通道 3 语义：
    // pkexec action 名为 install-package，不允许执行任意命令）
    const ALLOWED: &[&str] = &["apt-get", "dnf", "pacman", "flatpak"];
    let cmd = &args[0];
    if !ALLOWED.contains(&cmd.as_str()) {
        eprintln!(
            "agent-shell-rootd: pkexec rejected non-package-manager command: {cmd} (allowed: {ALLOWED:?})"
        );
        return ExitCode::from(126); // EACCES-like: permission denied
    }
    eprintln!(
        "agent-shell-rootd: pkexec mode (polkit pre-authorized via com.agentshell.pkexec.install-package, cmd={cmd})"
    );
    let status = std::process::Command::new(cmd).args(&args[1..]).status();
    match status {
        Ok(s) => {
            if s.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(s.code().unwrap_or(1) as u8)
            }
        }
        Err(e) => {
            eprintln!("agent-shell-rootd: pkexec command failed: {e}");
            ExitCode::from(1)
        }
    }
}
