//! agent-shell-rootd 入口：system bus 上的 D-Bus 服务。
//!
//! systemd system unit（`agent-shell-rootd.service`）启动；daemon 经
//! system bus 调用白名单方法，polkit 按 §23.4.2 action 精确授权。
//!
//! 常驻形态：本进程必须持续运行（unit Restart=on-failure 语义下退出
//! 即重启风暴）。当前 D-Bus 服务注册依赖 zbus system bus 连接接线
//! （后续任务），在接线落地前以**驻留等待**模式运行——进程保持存活、
//! 方法核心在 lib 层可单测，SIGTERM 时干净退出（systemctl stop 正常
//! 停止，不触发重启）。

use tokio::signal::unix::{signal, SignalKind};

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    eprintln!("agent-shell-rootd {version} (resident; D-Bus wiring pending)");

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            // SIGTERM = systemctl stop / 系统关机；SIGINT = 手动调试中断。
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => eprintln!("agent-shell-rootd: SIGTERM"),
                _ = int.recv() => eprintln!("agent-shell-rootd: SIGINT"),
            }
        });
    eprintln!("agent-shell-rootd {version} shutting down");
}
