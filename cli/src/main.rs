//! agent-shell CLI 入口（设计文档 §17.2 命令面 + §22.2 D1 架构）。
//!
//! CLI 是**瞬态无状态 JSON-RPC 客户端**：不直连任何系统服务（D-Bus/
//! Wayland/X11/portal/AT-SPI），所有操作经 daemon 执行。组件编排、
//! 持久化连接与状态缓存全部在 daemon（`agent-shell-daemon`）。

mod cli;
mod client;
mod format;
mod repl;

use agent_shell_rpc::{method, WindowOpKind};
use clap::Parser;
use cli::{Cli, Command, OutputFormat};
use client::DaemonClient;
use serde_json::{json, Value};

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
    match args.command {
        Some(command) => dispatch_command(command, args.output_format).await,
        None => repl::run_repl(args.output_format).await,
    }
}

/// 分派具体子命令（REPL 与单次执行共用）。
async fn dispatch_command(command: Command, out: OutputFormat) -> CmdResult {
    let mut c = DaemonClient::connect().await?;
    match command {
        Command::Doctor => doctor(&mut c).await,
        Command::Info => info(&mut c).await,
        Command::Windows(cmd) => windows(out, &mut c, cmd).await,
        Command::Workspaces(cmd) => workspaces(&mut c, cmd).await,
        Command::Input(cmd) => input(&mut c, cmd).await,
        Command::Screenshot(cmd) => screenshot(&mut c, cmd).await,
        Command::A11y(cmd) => a11y(&mut c, cmd).await,
        Command::Events(cmd) => events(&mut c, cmd).await,
        Command::Daemon(cmd) => daemon_cmd(&mut c, cmd).await,
        Command::Ime(cmd) => ime(&mut c, cmd).await,
        Command::Brightness(cmd) => brightness(&mut c, cmd).await,
        Command::File(cmd) => file_cmd(&mut c, cmd).await,
        Command::Mime(cmd) => mime_cmd(&mut c, cmd).await,
        Command::Bluetooth(cmd) => bluetooth(&mut c, cmd).await,
        Command::Software(cmd) => software(&mut c, cmd).await,
        Command::Touchpad(cmd) => touchpad(&mut c, cmd).await,
        Command::Kbd(cmd) => kbd(&mut c, cmd).await,
        Command::Secret(cmd) => secret(&mut c, cmd).await,
        Command::Shortcut(cmd) => shortcut(&mut c, cmd).await,
        Command::Timer(cmd) => timer(&mut c, cmd).await,
        Command::Service(cmd) => service_cmd(&mut c, cmd).await,
        Command::Log(cmd) => log_cmd(&mut c, cmd).await,
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

// ───────────────────────── events ─────────────────────────

async fn events(c: &mut DaemonClient, cmd: cli::EventsCommand) -> CmdResult {
    use cli::EventsCommand as E;
    match cmd {
        E::Subscribe { filter } => {
            let params = filter
                .map(|f| json!({ "filter": f }))
                .unwrap_or(Value::Null);
            let r = c.call(method::EVENTS_SUBSCRIBE, params).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        E::Unsubscribe { id } => {
            c.call(method::EVENTS_UNSUBSCRIBE, json!({ "subscriber_id": id }))
                .await?;
        }
        E::Replay { filter } => {
            let params = filter
                .map(|f| json!({ "filter": f }))
                .unwrap_or(Value::Null);
            let r = c.call(method::EVENTS_REPLAY, params).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── daemon ─────────────────────────

async fn daemon_cmd(c: &mut DaemonClient, cmd: cli::DaemonCommand) -> CmdResult {
    match cmd {
        cli::DaemonCommand::Status => {
            let r = c.call0(method::DAEMON_STATUS).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::DaemonCommand::Sessions => {
            let r = c.call0(method::DAEMON_SESSIONS).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── ime ─────────────────────────

async fn ime(c: &mut DaemonClient, cmd: cli::ImeCommand) -> CmdResult {
    match cmd {
        cli::ImeCommand::Engine(cli::ImeEngineCommand::List) => {
            let r = c.call0(method::IME_ENGINE_LIST).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::ImeCommand::Engine(cli::ImeEngineCommand::Set { engine }) => {
            c.call(method::IME_ENGINE_SET, json!({ "engine": engine }))
                .await?;
        }
        cli::ImeCommand::Engine(cli::ImeEngineCommand::Current) => {
            let r = c.call0(method::IME_ENGINE_CURRENT).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::ImeCommand::Type { text } => {
            let r = c.call(method::IME_TYPE, json!({ "text": text })).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── brightness ─────────────────────────

async fn brightness(c: &mut DaemonClient, cmd: cli::BrightnessCommand) -> CmdResult {
    match cmd.value {
        Some(v) => {
            c.call(method::BRIGHTNESS_SET, json!({ "value": v }))
                .await?;
        }
        None => {
            let r = c.call0(method::BRIGHTNESS_GET).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── file ─────────────────────────

async fn file_cmd(c: &mut DaemonClient, cmd: cli::FileCommand) -> CmdResult {
    match cmd {
        cli::FileCommand::Pick => {
            let r = c.call0(method::FILE_PICK).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::FileCommand::Trash { path } => {
            c.call(method::FILE_TRASH, json!({ "path": path })).await?;
        }
        cli::FileCommand::OpenDirectory { path } => {
            c.call(method::FILE_OPEN_DIR, json!({ "path": path }))
                .await?;
        }
    }
    Ok(0)
}

// ───────────────────────── mime ─────────────────────────

async fn mime_cmd(c: &mut DaemonClient, cmd: cli::MimeCommand) -> CmdResult {
    match cmd {
        cli::MimeCommand::Get { mime } => {
            let r = c.call(method::MIME_GET, json!({ "mime": mime })).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::MimeCommand::Set { mime, app } => {
            c.call(method::MIME_SET, json!({ "mime": mime, "app": app }))
                .await?;
        }
        cli::MimeCommand::DefaultBrowser => {
            let r = c.call0(method::MIME_DEFAULT_BROWSER).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── bluetooth ─────────────────────────

async fn bluetooth(c: &mut DaemonClient, cmd: cli::BluetoothCommand) -> CmdResult {
    match cmd {
        cli::BluetoothCommand::Scan => {
            let r = c.call0(method::BLUETOOTH_SCAN).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::BluetoothCommand::Connect { address } => {
            c.call(method::BLUETOOTH_CONNECT, json!({ "address": address }))
                .await?;
        }
        cli::BluetoothCommand::Disconnect { address } => {
            c.call(method::BLUETOOTH_DISCONNECT, json!({ "address": address }))
                .await?;
        }
        cli::BluetoothCommand::List => {
            let r = c.call0(method::BLUETOOTH_LIST).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── software ─────────────────────────

async fn software(c: &mut DaemonClient, cmd: cli::SoftwareCommand) -> CmdResult {
    match cmd {
        cli::SoftwareCommand::Flatpak(cli::FlatpakCommand::List) => {
            let r = c.call0(method::FLATPAK_LIST).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::SoftwareCommand::Flatpak(cli::FlatpakCommand::Install { app_id }) => {
            let r = c
                .call(method::FLATPAK_INSTALL, json!({ "app_id": app_id }))
                .await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::SoftwareCommand::Updates => {
            let r = c.call0(method::SOFTWARE_UPDATES).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── touchpad ─────────────────────────

async fn touchpad(c: &mut DaemonClient, cmd: cli::TouchpadCommand) -> CmdResult {
    match cmd {
        cli::TouchpadCommand::Status => {
            let r = c.call0(method::TOUCHPAD_STATUS).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::TouchpadCommand::On => {
            c.call(method::TOUCHPAD_SET, json!({ "enabled": true }))
                .await?;
        }
        cli::TouchpadCommand::Off => {
            c.call(method::TOUCHPAD_SET, json!({ "enabled": false }))
                .await?;
        }
        cli::TouchpadCommand::NaturalScroll { state } => {
            let enabled = match state.as_str() {
                "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                _ => {
                    return Err(format!(
                        "invalid natural-scroll state: {state} (expected on/off)"
                    ))
                }
            };
            c.call(method::TOUCHPAD_SET, json!({ "natural_scroll": enabled }))
                .await?;
        }
    }
    Ok(0)
}

// ───────────────────────── kbd ─────────────────────────

async fn kbd(c: &mut DaemonClient, cmd: cli::KbdCommand) -> CmdResult {
    match cmd {
        cli::KbdCommand::Layout(cli::KbdLayoutCommand::List) => {
            let r = c.call0(method::KBD_LAYOUT_LIST).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::KbdCommand::Layout(cli::KbdLayoutCommand::Set { layout }) => {
            c.call(method::KBD_LAYOUT_SET, json!({ "layout": layout }))
                .await?;
        }
    }
    Ok(0)
}

// ───────────────────────── secret ─────────────────────────

async fn secret(c: &mut DaemonClient, cmd: cli::SecretCommand) -> CmdResult {
    match cmd {
        cli::SecretCommand::Set { key, value } => {
            c.call(method::SECRET_SET, json!({ "key": key, "value": value }))
                .await?;
        }
        cli::SecretCommand::Get { key } => {
            let r = c.call(method::SECRET_GET, json!({ "key": key })).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── shortcut ─────────────────────────

async fn shortcut(c: &mut DaemonClient, cmd: cli::ShortcutCommand) -> CmdResult {
    match cmd {
        cli::ShortcutCommand::Bind { combo, action } => {
            let r = c
                .call(
                    method::SHORTCUT_BIND,
                    json!({ "combo": combo, "action": action }),
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::ShortcutCommand::Trigger { combo } => {
            let r = c
                .call(method::SHORTCUT_TRIGGER, json!({ "combo": combo }))
                .await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── timer ─────────────────────────

async fn timer(c: &mut DaemonClient, cmd: cli::TimerCommand) -> CmdResult {
    match cmd {
        cli::TimerCommand::List => {
            let r = c.call0(method::TIMER_LIST).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        cli::TimerCommand::Next { name } => {
            let r = c.call(method::TIMER_NEXT, json!({ "name": name })).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
    }
    Ok(0)
}

// ───────────────────────── service / log（rootd 特权链路，§23.4） ─────────────────────────

async fn service_cmd(c: &mut DaemonClient, cmd: cli::ServiceCommand) -> CmdResult {
    let (action, unit) = match cmd {
        cli::ServiceCommand::Start { unit } => ("start", unit),
        cli::ServiceCommand::Stop { unit } => ("stop", unit),
        cli::ServiceCommand::Restart { unit } => ("restart", unit),
    };
    let _ = c
        .call(
            method::SERVICE_CONTROL,
            json!({ "action": action, "unit": unit }),
        )
        .await?;
    // service_control 的错误由 RPC error 层处理（call() 已返回 Err）。
    // 成功到达此处的 result 不含 error 字段——只检查 accepted。
    println!("service {action} {unit}: accepted");
    Ok(0)
}

/// `log` 查询的 CLI 侧超时（秒）：大于 daemon→rootd 链路最长 90s 的容错余量，
/// 到点主动失败并给明确提示（TSI-2493），而非静默挂起。
const LOG_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// `log` 查询超时后的用户可读错误信息。
fn log_query_timeout_error() -> String {
    format!(
        "journal query timed out after {}s (rootd may be scanning an oversized journal)",
        LOG_QUERY_TIMEOUT.as_secs()
    )
}

async fn log_cmd(c: &mut DaemonClient, cmd: cli::LogCommand) -> CmdResult {
    // --filter 是 clap 捕获的字符串，必须解析为 JSON 对象后再嵌入 RPC 参数。
    // 若直接传字符串，daemon 会二次序列化——rootd journal_query 收到
    // `"{\"unit\":\"sshd\"}"` 字符串字面量而非对象，`is_object()` 拒绝。
    let filter = parse_log_filter(&cmd.filter)?;
    let r = tokio::time::timeout(
        LOG_QUERY_TIMEOUT,
        c.call(method::SYSTEM_LOG_VIEW, json!({ "filter": filter })),
    )
    .await
    .map_err(|_| log_query_timeout_error())??;
    // system_log_view 的错误由 RPC error 层处理（call() 已返回 Err）。
    println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
    Ok(0)
}

/// 把 `--filter` 原始字符串解析为 JSON 对象（§23.4 JournalQuery 参数契约）。
fn parse_log_filter(raw: &str) -> Result<Value, String> {
    let v: Value =
        serde_json::from_str(raw).map_err(|e| format!("--filter must be a JSON object: {e}"))?;
    if !v.is_object() {
        return Err("--filter must be a JSON object".into());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::{log_query_timeout_error, parse_log_filter, LOG_QUERY_TIMEOUT};

    #[test]
    fn log_timeout_is_positive_and_explicit() {
        assert_eq!(LOG_QUERY_TIMEOUT.as_secs(), 90);
        let msg = log_query_timeout_error();
        assert!(msg.contains("timed out"), "{msg}");
        assert!(msg.contains("90"), "{msg}");
    }

    #[test]
    fn filter_object_parses() {
        let v = parse_log_filter(r#"{"unit":"sshd"}"#).expect("object filter");
        assert_eq!(v.get("unit").and_then(|u| u.as_str()), Some("sshd"));
    }

    #[test]
    fn filter_non_object_rejected() {
        assert!(parse_log_filter(r#""free text""#).is_err());
        assert!(parse_log_filter("not json").is_err());
    }
}
