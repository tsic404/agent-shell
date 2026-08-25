//! agent-shell CLI 入口（设计文档 §17.2 命令面 + §22.2 D1 架构）。
//!
//! CLI 是**瞬态无状态 JSON-RPC 客户端**：不直连任何系统服务（D-Bus/
//! Wayland/X11/portal/AT-SPI），所有操作经 daemon 执行。组件编排、
//! 持久化连接与状态缓存全部在 daemon（`agent-shell-daemon`）。

mod cli;
mod client;
mod format;

use agent_shell_rpc::WindowOpKind;
use clap::Parser;
use cli::{Cli, Command, OutputFormat};
use client::DaemonClient;
use serde_json::json;

fn main() {
    let args = Cli::parse();
    let code = tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(run(args));
    std::process::exit(code);
}

async fn run(args: Cli) -> i32 {
    match dispatch(args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

type CmdResult = Result<i32, String>;

async fn dispatch(args: Cli) -> CmdResult {
    // CLI 无状态：每条命令建一次连接（自动拉起 daemon，§22.2 激活策略）。
    let mut c = DaemonClient::connect().await?;
    match args.command {
        Command::Doctor => doctor(&mut c).await,
        Command::Info => info(&mut c).await,
        Command::Windows(cmd) => windows(args.output_format, &mut c, cmd).await,
        Command::Workspaces(cmd) => workspaces(&mut c, cmd).await,
        Command::Input(cmd) => input(&mut c, cmd).await,
        Command::Screenshot(cmd) => screenshot(&mut c, cmd).await,
        Command::A11y(cmd) => a11y(&mut c, cmd).await,
    }
}

// ───────────────────────── doctor / info ─────────────────────────

async fn doctor(c: &mut DaemonClient) -> CmdResult {
    let report = c.doctor().await?;
    for line in &report.lines {
        println!("{line}");
    }
    Ok(if report.healthy { 0 } else { 2 })
}

async fn info(c: &mut DaemonClient) -> CmdResult {
    let r = c.info().await?;
    println!("{}", r.detection);
    println!("backend           : kwin-compositor (via daemon)");
    for (name, enabled) in &r.capabilities {
        println!("{:<24} {}", name, if *enabled { "✓" } else { "✗" });
    }
    Ok(0)
}

// ───────────────────────── windows / workspaces ─────────────────────────

async fn windows(out: OutputFormat, c: &mut DaemonClient, cmd: cli::WindowsCommand) -> CmdResult {
    use cli::WindowsCommand as W;
    match cmd {
        W::List { filter } => {
            let (wins, from_cache) = c.windows_list(filter).await?;
            emit_windows_json_or_table(out, &wins);
            eprintln!("# from_cache={from_cache}");
            Ok(0)
        }
        W::Info { target } => {
            let wins = c.windows_list(None).await?.0;
            let w = cli::resolve_target_entry(&target, &wins)?;
            if out == OutputFormat::Json {
                let v = c.window_info(&w.native_id).await?;
                println!("{}", serde_json::to_string_pretty(&v).expect("json"));
            } else {
                println!(
                    "id={} title={:?} app={} pid={}",
                    w.native_id, w.title, w.app_id, w.pid
                );
            }
            Ok(0)
        }
        W::Focus { target } => window_op(c, out, target, WindowOpKind::Focus, [0; 4]).await,
        W::Move { target, x, y } => {
            window_op(c, out, target, WindowOpKind::Move, [x, y, 0, 0]).await
        }
        W::Resize {
            target,
            width,
            height,
        } => window_op(c, out, target, WindowOpKind::Resize, [0, 0, width, height]).await,
        W::Minimize { target } => window_op(c, out, target, WindowOpKind::Minimize, [0; 4]).await,
        W::Close { target } => window_op(c, out, target, WindowOpKind::Close, [0; 4]).await,
    }
}

/// 定位目标并经 RPC 执行窗口写操作。ID 空间一致性由架构保证——
/// 列表与操作均走 daemon 的同一 KWin 通道，无 X11/KWin 混用可能。
async fn window_op(
    c: &mut DaemonClient,
    _out: OutputFormat,
    target: String,
    op: WindowOpKind,
    geo: [i32; 4],
) -> CmdResult {
    let wins = c.windows_list(None).await?.0;
    let w = cli::resolve_target_entry(&target, &wins)?;
    c.window_op(op, &w.native_id.clone(), geo).await?;
    Ok(0)
}

fn emit_windows_json_or_table(out: OutputFormat, wins: &[agent_shell_rpc::WindowEntry]) {
    match out {
        OutputFormat::Json => {
            println!("{}", format::windows_entries_json(wins));
        }
        OutputFormat::Table => print!("{}", format::windows_entries_table(wins)),
    }
}

async fn workspaces(c: &mut DaemonClient, cmd: cli::WorkspacesCommand) -> CmdResult {
    match cmd {
        cli::WorkspacesCommand::List => {
            for line in c.workspaces_list().await? {
                println!("{line}");
            }
        }
        cli::WorkspacesCommand::Switch { id } => c.workspace_switch(&id).await?,
    }
    Ok(0)
}

// ───────────────────────── input ─────────────────────────

async fn input(c: &mut DaemonClient, cmd: cli::InputCommand) -> CmdResult {
    use agent_shell_rpc::InputKind::*;
    match cmd {
        cli::InputCommand::Key { combo } => {
            cli::parse_key_combo(&combo)?; // 语法校验在 CLI（快速失败）
            c.input(Key, json!({ "combo": combo })).await?
        }
        cli::InputCommand::Type { text } => c.input(TypeText, json!({ "text": text })).await?,
        cli::InputCommand::Click { button, at } => {
            let at_v = at.as_deref().map(cli::parse_xy_json).transpose()?;
            c.input(Click, json!({ "button": button, "at": at_v }))
                .await?
        }
        cli::InputCommand::Scroll { dx, dy } => {
            c.input(Scroll, json!({ "dx": dx, "dy": dy })).await?
        }
    }
    Ok(0)
}

// ───────────────────────── screenshot ─────────────────────────

async fn screenshot(c: &mut DaemonClient, cmd: cli::ScreenshotCommand) -> CmdResult {
    // --area 接线：X,Y,W,H 直传 daemon 裁剪。
    let area = cmd.area.map(|v| [v[0], v[1], v[2], v[3]]);
    let r = c.screenshot(cmd.window, area, &cmd.output).await?;
    println!("saved {} ({}x{})", r.path, r.width, r.height);
    Ok(0)
}

// ───────────────────────── a11y ─────────────────────────

async fn a11y(c: &mut DaemonClient, cmd: cli::A11yCommand) -> CmdResult {
    match cmd {
        cli::A11yCommand::Status => {
            let r = c.a11y_status().await?;
            println!("{}", r.detail);
            Ok(if r.available { 0 } else { 2 })
        }
        cli::A11yCommand::Query { .. } => Err(
            "a11y query requires components/a11y (AT-SPI tree, TSI-2317); only bus status is available".into(),
        ),
    }
}
