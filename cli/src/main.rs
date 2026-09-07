//! agent-shell CLI 入口（设计文档 §17.2 命令面 + §22.2 D1 架构）。
//!
//! CLI 是**瞬态无状态 JSON-RPC 客户端**：不直连任何系统服务（D-Bus/
//! Wayland/X11/portal/AT-SPI），所有操作经 daemon 执行。组件编排、
//! 持久化连接与状态缓存全部在 daemon（`agent-shell-daemon`）。

mod cli;
mod client;
mod format;
mod repl;

use agent_shell_rpc::{method, RpcErrorCode, WindowOpKind, MOUNT_POLKIT_ACTION};
use clap::Parser;
use cli::{Cli, Command, OutputFormat};
use client::{CallError, DaemonClient};
use serde_json::{json, Value};

fn main() {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let code = match Cli::try_parse_from(argv.clone()) {
        Ok(args) => tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(run(args)),
        Err(e) => {
            let code = e.exit_code();
            let _ = e.print();
            if let Some(hint) = cli::at_syntax_hint(&argv, &e) {
                eprintln!("\nhint: {hint}");
            }
            code
        }
    };
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
        Command::Security(cmd) => security_cmd(&mut c, cmd).await,
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
        Command::Fs(cmd) => fs_cmd(&mut c, cmd).await,
        Command::Log(cmd) => log_cmd(&mut c, cmd).await,
        Command::Hostname(cmd) => hostname_cmd(&mut c, cmd).await,
        Command::Kill { pid, signal } => kill_cmd(&mut c, pid, signal).await,
        Command::Job(cmd) => job_cmd(&mut c, cmd).await,
        Command::Pkg(cmd) => pkg_cmd(&mut c, cmd).await,
        Command::Sysctl(cmd) => sysctl_cmd(&mut c, cmd).await,
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
        W::Wait { app_id, timeout_ms } => {
            let timeout_ms = timeout_ms.unwrap_or(15_000);
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            loop {
                let (wins, _) = c.windows_list(Some(app_id.clone())).await?;
                if let Some(w) = wins.first() {
                    emit_windows_json_or_table(out, std::slice::from_ref(w));
                    return Ok(0);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "window '{app_id}' did not appear within {timeout_ms}ms"
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
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
        cli::A11yCommand::Query {
            role,
            name,
            all,
            fail_on_empty,
        } => {
            let r = c.a11y_query(role, name, all).await?;
            println!("{}", serde_json::to_string_pretty(&r).expect("json"));
            Ok(a11y_query_exit_code(r.count, fail_on_empty))
        }
    }
}

/// `a11y query` 的退出码决策：零命中不再是错误——RPC 成功即 exit 0，
/// 空结果照常打印；仅当调用方显式传入 `--fail-on-empty` 时才把零命中
/// 视为失败（exit 2）。这样脚本可通过退出码区分「无匹配」与「命令出错」
/// （后者走 `run()` 的 `Err` 路径，exit 1）。
fn a11y_query_exit_code(count: usize, fail_on_empty: bool) -> i32 {
    if count > 0 {
        0
    } else if fail_on_empty {
        2
    } else {
        0
    }
}

// ───────────────────────── events ─────────────────────────

async fn events(c: &mut DaemonClient, cmd: cli::EventsCommand) -> CmdResult {
    use cli::EventsCommand as E;
    match cmd {
        E::Subscribe { filter } => {
            c.subscribe(filter).await?;
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

// ───────────────────────── security ─────────────────────────

/// `security.*` 策略面 RPC 透传（§22.7 D6）。
///
/// daemon 侧 status/audit 只读、免普通级别 gate；grant/revoke 是提权原语，
/// 按 caller_id 门禁（仅 `"*"` 本地调用方放行，见 daemon `dispatch.rs`
/// `check_management_permission`）。CLI 子进程未注入 `AGENT_SHELL_AGENT_ID`
/// 时即以 `"*"` 运行，具备管理面权限。
///
/// 方法名与参数形状由纯函数 [`security_request`] 产出，测试锚定其契约。
async fn security_cmd(c: &mut DaemonClient, cmd: cli::SecurityCommand) -> CmdResult {
    let (method_name, params) = security_request(&cmd);
    let r = if params.is_null() {
        c.call0(method_name).await?
    } else {
        c.call(method_name, params).await?
    };
    println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
    Ok(0)
}

/// `security.*` 分派表：子命令 → (RPC 方法名, 参数)。独立纯函数，
/// 供测试锚定方法名与参数形状；audit 省略键约定由 `security_audit_params`
/// 保证（与 `a11y.query` 同源约定）。
fn security_request(cmd: &cli::SecurityCommand) -> (&'static str, Value) {
    match cmd {
        cli::SecurityCommand::Status => (method::SECURITY_STATUS, Value::Null),
        cli::SecurityCommand::Grant { agent_id, level } => (
            method::SECURITY_GRANT,
            json!({ "agent_id": agent_id, "level": level }),
        ),
        cli::SecurityCommand::Revoke { agent_id } => {
            (method::SECURITY_REVOKE, json!({ "agent_id": agent_id }))
        }
        cli::SecurityCommand::Audit {
            agent_id,
            op,
            decision,
            result,
        } => (
            method::SECURITY_AUDIT,
            security_audit_params(
                agent_id.as_deref(),
                op.as_deref(),
                decision.as_deref(),
                *result,
            ),
        ),
    }
}

/// `security.audit` 参数构造：`None` 过滤条件省略键，而非序列化为 JSON
/// `null`（与 `a11y.query` 同约定；daemon 空串 / 缺键 = 不限制该维度）。
fn security_audit_params(
    agent_id: Option<&str>,
    op: Option<&str>,
    decision: Option<&str>,
    result: Option<bool>,
) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(v) = agent_id {
        params.insert("agent_id".into(), json!(v));
    }
    if let Some(v) = op {
        params.insert("op".into(), json!(v));
    }
    if let Some(v) = decision {
        params.insert("decision".into(), json!(v));
    }
    if let Some(v) = result {
        params.insert("result".into(), json!(v));
    }
    Value::Object(params)
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

/// `service.control` 扩展动作（enable/disable/reload）共用的错误映射：
/// rootd 未安装/校验失败/系统调用失败 → 干净 stderr + exit 1，polkit 拒绝 →
/// exit 2；其余错误保留 `rpc error N: …` 原始形态，由 `run` 统一打印。
async fn service_extended(c: &mut DaemonClient, action: &str, unit: &str) -> CmdResult {
    match c
        .call(
            method::SERVICE_CONTROL,
            json!({ "action": action, "unit": unit }),
        )
        .await
    {
        Ok(_) => {
            println!("service {action} {unit}: accepted");
            Ok(0)
        }
        Err(e) => match process_rootd_error(&e) {
            Some((code, msg)) => {
                eprintln!("error: {msg}");
                Ok(code)
            }
            None => Err(e),
        },
    }
}

async fn service_cmd(c: &mut DaemonClient, cmd: cli::ServiceCommand) -> CmdResult {
    match cmd {
        cli::ServiceCommand::Start { unit } => {
            let _ = c
                .call(
                    method::SERVICE_CONTROL,
                    json!({ "action": "start", "unit": unit }),
                )
                .await?;
            println!("service start {unit}: accepted");
            Ok(0)
        }
        cli::ServiceCommand::Stop { unit } => {
            let _ = c
                .call(
                    method::SERVICE_CONTROL,
                    json!({ "action": "stop", "unit": unit }),
                )
                .await?;
            println!("service stop {unit}: accepted");
            Ok(0)
        }
        cli::ServiceCommand::Restart { unit } => {
            let _ = c
                .call(
                    method::SERVICE_CONTROL,
                    json!({ "action": "restart", "unit": unit }),
                )
                .await?;
            println!("service restart {unit}: accepted");
            Ok(0)
        }
        cli::ServiceCommand::Enable { unit } => service_extended(c, "enable", &unit).await,
        cli::ServiceCommand::Disable { unit } => service_extended(c, "disable", &unit).await,
        cli::ServiceCommand::Reload { unit } => service_extended(c, "reload", &unit).await,
        cli::ServiceCommand::DaemonReload => match c.call0(method::DAEMON_RELOAD).await {
            Ok(_) => {
                println!("daemon-reload: accepted");
                Ok(0)
            }
            Err(e) => match process_rootd_error(&e) {
                Some((code, msg)) => {
                    eprintln!("error: {msg}");
                    Ok(code)
                }
                None => Err(e),
            },
        },
    }
}

/// `kill` 命令（rootd ProcessKill，§23.4）。
///
/// 错误分两类处理：
/// - rootd 直传的校验/系统调用错误（BackendError 1005）与 rootd 未安装
///   （BackendUnavailable 1002）——映射为 issue 错误表要求的干净信息；
/// - 其余错误（权限 gate ConfirmationRequired、daemon 连接失败等）保留
///   `rpc error N: …` 原始形态，由 `run` 统一 `error: {e}` 打印。
async fn kill_cmd(c: &mut DaemonClient, pid: i32, signal: i32) -> CmdResult {
    match c
        .call(
            method::PROCESS_KILL,
            json!({ "pid": pid, "signal": signal }),
        )
        .await
    {
        Ok(_) => {
            println!("kill {pid} (signal {signal}): accepted");
            Ok(0)
        }
        Err(e) => match process_rootd_error(&e) {
            Some((code, msg)) => {
                eprintln!("error: {msg}");
                Ok(code)
            }
            None => Err(e),
        },
    }
}

/// `pkg` 命令组（rootd PackageInstall/Remove/Update/Refresh，§23.4）。
///
/// rootd 返回 `{job_id, pm}`（pm = 检测到的包管理器类型）。无 `--wait`
/// 时打印排队信息即退出；`--wait` 时复用 [`wait_for_job`] 轮询至完成。
/// 错误分两类：rootd 直传错误走 [`process_rootd_error`] 干净映射，
/// 其余（权限 gate、daemon 连接失败等）保留原始 RPC 错误形态。
async fn pkg_cmd(c: &mut DaemonClient, cmd: cli::PkgCommand) -> CmdResult {
    let (m, packages, wait) = match cmd {
        cli::PkgCommand::Install { packages, wait } => (method::PACKAGE_INSTALL, packages, wait),
        cli::PkgCommand::Remove { packages, wait } => (method::PACKAGE_REMOVE, packages, wait),
        cli::PkgCommand::Update { packages, wait } => (method::PACKAGE_UPDATE, packages, wait),
        cli::PkgCommand::Refresh { wait } => (method::PACKAGE_REFRESH, Vec::new(), wait),
    };
    pkg_cmd_impl(c, m, packages, wait).await
}

/// `pkg_cmd` 的公共实现：调用 → 取 job_id/pm → 按需等待。
async fn pkg_cmd_impl(
    c: &mut DaemonClient,
    m: &str,
    packages: Vec<String>,
    wait: bool,
) -> CmdResult {
    let params = if m == method::PACKAGE_REFRESH {
        json!({})
    } else {
        json!({ "packages": packages })
    };
    let v = match c.call(m, params).await {
        Ok(v) => v,
        Err(e) => {
            return match process_rootd_error(&e) {
                Some((code, msg)) => {
                    eprintln!("error: {msg}");
                    Ok(code)
                }
                None => Err(e),
            }
        }
    };
    let job_id = v
        .get("job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("malformed response (missing job_id): {v}"))?;
    let pm = v.get("pm").and_then(Value::as_str).unwrap_or("unknown");
    if !wait {
        println!("job {job_id} queued (pm: {pm})");
        return Ok(0);
    }
    match wait_for_job(c, job_id).await {
        Ok(true) => {
            println!("job {job_id} completed successfully");
            Ok(0)
        }
        Ok(false) => {
            let st = c
                .job_status(job_id)
                .await
                .unwrap_or_else(|_| serde_json::json!({}));
            eprintln!("{}", pkg_failed_job_line(&st));
            Ok(1)
        }
        Err(e) if e.contains("did not finish within") => {
            eprintln!("error: job timed out");
            Ok(1)
        }
        Err(e) => Err(e),
    }
}

/// 构造 `--wait` 失败时的 job 摘要行。
fn pkg_failed_job_line(v: &Value) -> String {
    let code = v
        .get("exit_code")
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".into());
    let stderr = v.get("stderr").and_then(Value::as_str).unwrap_or("");
    format!("error: job failed (exit {code}): {stderr}")
}

/// `log` 查询的 CLI 侧超时（秒）：作为最外层兜底，防止 daemon 本身无响应
/// 时 CLI 进程永久挂起。必须严格大于 daemon→rootd 的 zbus `method_timeout`
/// （`daemon/src/rootd_client.rs`，90s）——rootd `JOURNAL_QUERY_TIMEOUT` 60s
/// 最先触发，其次 daemon zbus 90s 错误会先传播回 CLI，本常量仅覆盖 daemon
/// 进程自身卡死的罕见情况，不会抢在 daemon 的超时错误之前误报。
const LOG_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(91);

/// `log` 查询 CLI 侧超时后的用户可读错误信息。
fn log_query_timeout_error() -> String {
    format!(
        "call to daemon timed out after {}s (daemon may be unresponsive)",
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

/// 文件系统挂载/卸载（rootd Mount/Unmount，§23.4）。
async fn fs_cmd(c: &mut DaemonClient, cmd: cli::FsCommand) -> CmdResult {
    match cmd {
        cli::FsCommand::Mount {
            device,
            target,
            fstype,
            options,
        } => {
            let r = c
                .call_rpc(
                    method::MOUNT,
                    json!({
                        "device": device,
                        "target": target,
                        "fstype": fstype,
                        "options": options.unwrap_or_default(),
                    }),
                )
                .await;
            match r {
                Ok(_) => {
                    println!("mount {device} → {target} (fstype {fstype}): accepted");
                    Ok(0)
                }
                Err(e) => fs_rpc_error(e),
            }
        }
        cli::FsCommand::Unmount { target } => {
            let r = c
                .call_rpc(method::UNMOUNT, json!({ "target": target }))
                .await;
            match r {
                Ok(_) => {
                    println!("unmount {target}: accepted");
                    Ok(0)
                }
                Err(e) => fs_rpc_error(e),
            }
        }
    }
}

/// fs 命令 RPC 错误 → `CmdResult`：polkit 拒绝映射为 exit 2 与固定
/// stderr，其余沿用统一 `rpc error {code}: {message}` + exit 1。
fn fs_rpc_error(e: CallError) -> CmdResult {
    match &e {
        CallError::Rpc { code, message } if is_auth_required(*code, message) => {
            eprintln!("error: authentication required: {}", MOUNT_POLKIT_ACTION);
            Ok(2)
        }
        _ => Err(e.to_string()),
    }
}

/// 判定 RPC 错误是否为 mount/unmount 的授权拒绝。
///
/// daemon 把 rootd 的 polkit 拒绝、polkit 基础设施故障与挂载失败都折叠为
/// 1005，仅消息可区分：`polkit denied action:` 是明确拒绝，`polkit check:` /
/// `polkit proxy:` 是 polkitd 不可达等授权协商故障，均归入授权拒绝；其余
/// 1005（挂载失败、相对路径拒绝等）仍为普通后端错误。1004/1006 按语义直接
/// 视为授权拒绝。
fn is_auth_required(code: i32, message: &str) -> bool {
    matches!(code, 1004 | 1006)
        || (code == 1005
            && (message.contains(&format!("polkit denied action: {MOUNT_POLKIT_ACTION}"))
                || message.contains("polkit check:")
                || message.contains("polkit proxy:")))
}

// ───────────────────────── job（rootd 特权链路，§23.4） ─────────────────────────

/// `wait_for_job` / `job_cmd` 的 job 状态来源抽象——生产走
/// `DaemonClient.call(method::JOB_STATUS)`，测试以脚本化 mock 驱动各分支。
trait JobStatusSource {
    async fn job_status(&mut self, job_id: &str) -> Result<Value, String>;
}

impl JobStatusSource for DaemonClient {
    async fn job_status(&mut self, job_id: &str) -> Result<Value, String> {
        self.call(method::JOB_STATUS, json!({ "job_id": job_id }))
            .await
    }
}

/// `job status` 查询：输出 rootd 返回的完整 JSON（found/method/progress/
/// done/success/exit_code/stderr）。RPC 成功即退出码 0（无论 found 真假）。
async fn job_cmd(c: &mut DaemonClient, cmd: cli::JobCommand) -> CmdResult {
    let cli::JobCommand::Status { job_id } = cmd;
    let out = job_status_outcome(c.job_status(&job_id).await)?;
    println!("{out}");
    Ok(0)
}

/// `job status` 的展示与退出码决策：RPC 成功 → pretty JSON（退出 0，
/// 无论 found 真假）；RPC 失败 → 错误上抛（`run()` 打印并退出 1）。
fn job_status_outcome(rpc: Result<Value, String>) -> Result<String, String> {
    rpc.map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
}

/// `job status` 轮询的超时（秒）。
const JOB_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// `job status` 轮询间隔（毫秒）。
const JOB_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// 轮询 `job.status` 直到 `done=true`，返回 `success`。
///
/// 供 `pkg --wait`（TSI-2558）复用。超时 300s 返回错误。
pub async fn wait_for_job(client: &mut DaemonClient, job_id: &str) -> Result<bool, String> {
    wait_for_job_impl(client, job_id, JOB_POLL_TIMEOUT, JOB_POLL_INTERVAL).await
}

/// 轮询实现（间隔/超时可注入，供测试驱动）。
///
/// `found=false` 只可能是「从未成功观测到」或「观测到后被 rootd drain」。
/// 两种情况下本函数都无法给出确定的 `success` 值：rootd 已保留完成
/// job 的最终快照缓存（见 `rootd` `job_drain_done`），正常轮询到
/// `done=true` 时才返回成功；此处落在缓存兜底之外的 drain 窗口，只能
/// 诚实报错，绝不猜测一个布尔值（会把成功 job 误报为失败）。
async fn wait_for_job_impl<S: JobStatusSource>(
    src: &mut S,
    job_id: &str,
    timeout: std::time::Duration,
    interval: std::time::Duration,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let v = src.job_status(job_id).await?;
        match v.get("found").and_then(Value::as_bool) {
            Some(true) => {
                if v.get("done").and_then(Value::as_bool) == Some(true) {
                    return job_success(&v);
                }
            }
            Some(false) => {
                return Err("job drained before completion result observed".into());
            }
            None => {
                return Err(format!(
                    "job.status returned malformed response (missing `found`): {v}"
                ))
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "job {job_id} did not finish within {}s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(interval).await;
    }
}

/// 从响应中取 `success` 布尔（缺省即 malformed 响应）。
fn job_success(v: &Value) -> Result<bool, String> {
    v.get("success")
        .and_then(Value::as_bool)
        .ok_or_else(|| "job.status response missing `success`".to_string())
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

/// `hostname set` 命令（rootd HostnameSet，§23.4）。
///
/// 错误分两类处理：
/// - rootd 直传的校验/系统调用错误（BackendError 1005）与 rootd 未安装
///   （BackendUnavailable 1002）——映射为 issue 错误表要求的干净信息；
/// - 其余错误（权限 gate ConfirmationRequired、daemon 连接失败等）保留
///   `rpc error N: …` 原始形态，由 `run` 统一 `error: {e}` 打印。
async fn hostname_cmd(c: &mut DaemonClient, cmd: cli::HostnameCommand) -> CmdResult {
    let name = match cmd {
        cli::HostnameCommand::Set { name } => name,
    };
    match c
        .call(method::HOSTNAME_SET, json!({ "hostname": name }))
        .await
    {
        Ok(_) => {
            println!("hostname set {name}: accepted");
            Ok(0)
        }
        Err(e) => match process_rootd_error(&e) {
            Some((code, msg)) => {
                eprintln!("error: {msg}");
                Ok(code)
            }
            None => Err(e),
        },
    }
}

/// 把 rootd 链路的 daemon 层错误映射为 issue 错误表规定的 (退出码, stderr 信息)。
///
/// 传输形态（daemon → CLI）：
/// - `rpc error 1002: rootd not installed — privileged operation unavailable`
/// - `rpc error 1005: rootd: org.freedesktop.DBus.Error.Failed: <rootd detail>`
/// - `rpc error 1005: rootd: org.freedesktop.DBus.Error.AuthFailed: polkit denied action: com.agentshell.hostname.set`
///
/// 返回 `None` 表示非本链路错误，交通用 RPC 错误路径。
fn process_rootd_error(e: &str) -> Option<(i32, String)> {
    // 剥通用 RPC 错误前缀 `rpc error <code>: `；传输层错误无此前缀，原样保留。
    let body = e.split_once(": ").map(|(_, rest)| rest).unwrap_or(e);
    if body.contains("rootd not installed") {
        return Some((
            1,
            "rootd not installed — privileged operation unavailable".into(),
        ));
    }
    let zbus = body.strip_prefix("rootd: ")?;
    // 剥 zbus 方法错误名（`org.freedesktop.DBus.Error.*: `），只留 rootd 原文。
    let detail = zbus.split_once(": ").map(|(_, d)| d).unwrap_or(zbus);
    if let Some(action) = detail.strip_prefix("polkit denied action: ") {
        return Some((2, format!("authentication required: {action}")));
    }
    // hostnamectl 失败保留 `rootd: ` 前缀与 zbus 错误名（issue 错误表
    // `error: rootd: <zbus error>`）；校验类错误剥前缀后干净输出。
    if detail.starts_with("hostnamectl failed") {
        return Some((1, body.to_string()));
    }
    Some((1, detail.to_string()))
}

/// `sysctl set` 成功输出行（CLI 打印与回归单测共用）。
fn sysctl_set_accepted_line(key: &str, value: &str) -> String {
    format!("sysctl set {key}={value}: accepted")
}

/// `sysctl` 命令（rootd SysctlGet/Set，§23.4）。
///
/// daemon 侧已把 polkit 拒绝映射为专用认证错误码 1007，故 CLI 侧按码分派：
/// 1007 → exit 2；其余错误保留 `rpc error N: …` 形态交 `run` 统一打印（exit 1）。
async fn sysctl_cmd(c: &mut DaemonClient, cmd: cli::SysctlCommand) -> CmdResult {
    match cmd {
        cli::SysctlCommand::Get { key } => {
            let r = c.call_rpc(method::SYSCTL_GET, json!({ "key": key })).await;
            match r {
                Ok(v) => {
                    let s = v
                        .as_str()
                        .ok_or_else(|| "sysctl.get: malformed response".to_string())?;
                    println!("{s}");
                    Ok(0)
                }
                Err(e) => sysctl_rpc_error(e),
            }
        }
        cli::SysctlCommand::Set { key, value } => {
            let r = c
                .call_rpc(method::SYSCTL_SET, json!({ "key": key, "value": value }))
                .await;
            match r {
                Ok(_) => {
                    println!("{}", sysctl_set_accepted_line(&key, &value));
                    Ok(0)
                }
                Err(e) => sysctl_rpc_error(e),
            }
        }
    }
}

/// sysctl RPC 错误 → `CmdResult`：认证要求（1007）→ exit 2；其余交
/// `process_rootd_error` 剥 RPC/rootd 前缀后打印（exit 1）；非本链路错误
/// 保留 `rpc error N: …` 形态交 `run` 统一打印（exit 1）。
fn sysctl_rpc_error(e: CallError) -> CmdResult {
    if let CallError::Rpc { code, message } = &e {
        if *code == RpcErrorCode::AuthenticationRequired as i32 {
            eprintln!("error: {message}");
            return Ok(2);
        }
    }
    match process_rootd_error(&e.to_string()) {
        Some((code, msg)) => {
            eprintln!("error: {msg}");
            Ok(code)
        }
        None => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        a11y_query_exit_code, is_auth_required, job_status_outcome, log_query_timeout_error,
        parse_log_filter, pkg_failed_job_line, process_rootd_error, security_audit_params,
        security_request, sysctl_rpc_error, sysctl_set_accepted_line, wait_for_job_impl,
        JobStatusSource, LOG_QUERY_TIMEOUT,
    };
    use crate::client::CallError;
    use crate::RpcErrorCode;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::time::Duration;

    #[test]
    fn process_rootd_error_maps_all_five_branches() {
        // rootd 未安装 → exit 1（降级）。
        assert_eq!(
            process_rootd_error(
                "rpc error 1002: rootd not installed — privileged operation unavailable"
            ),
            Some((
                1,
                "rootd not installed — privileged operation unavailable".into()
            ))
        );
        // polkit 拒绝 → exit 2 + 前缀改写为 authentication required。
        assert_eq!(
            process_rootd_error(
                "rpc error 1005: rootd: org.freedesktop.DBus.Error.AuthFailed: polkit denied action: com.agentshell.hostname.set"
            ),
            Some((
                2,
                "authentication required: com.agentshell.hostname.set".into()
            ))
        );
        // 校验失败 → exit 1，剥 zbus 错误名，保留 rootd 原文。
        assert_eq!(
            process_rootd_error(
                "rpc error 1005: rootd: org.freedesktop.DBus.Error.Failed: illegal character in hostname: \"work_station\""
            ),
            Some((1, "illegal character in hostname: \"work_station\"".into()))
        );
        // hostnamectl 失败 → exit 1，保留 `rootd: ` 前缀与 zbus 错误名。
        assert_eq!(
            process_rootd_error(
                "rpc error 1005: rootd: org.freedesktop.DBus.Error.Failed: hostnamectl failed: some stderr"
            ),
            Some((
                1,
                "rootd: org.freedesktop.DBus.Error.Failed: hostnamectl failed: some stderr".into()
            ))
        );
        // 非本链路错误（权限 gate 等）→ None，保留 RPC 原始形态。
        assert_eq!(
            process_rootd_error("rpc error 1006: hostname.set (always)"),
            None
        );

        // kill：pid/signal 校验 → 剥 zbus 错误名前缀，保留 rootd 原文（exit 1）。
        assert_eq!(
            process_rootd_error(
                "rpc error 1005: rootd: org.freedesktop.DBus.Error.Failed: pid must be a positive integer"
            ),
            Some((1, "pid must be a positive integer".into()))
        );
        assert_eq!(
            process_rootd_error(
                "rpc error 1005: rootd: org.freedesktop.DBus.Error.Failed: signal must be in range 1-31, got 99"
            ),
            Some((1, "signal must be in range 1-31, got 99".into()))
        );
        // kill polkit 拒绝 → authentication required（exit 2）。
        assert_eq!(
            process_rootd_error(
                "rpc error 1005: rootd: org.freedesktop.DBus.Error.AuthFailed: polkit denied action: com.agentshell.process.kill"
            ),
            Some((2, "authentication required: com.agentshell.process.kill".into()))
        );
        // 非本链路错误（权限 gate 等）→ None，保留 RPC 原始形态。
        assert_eq!(
            process_rootd_error("rpc error 1006: process.kill (Always)"),
            None
        );
    }

    /// 脚本化 job 状态源：按队列返回，耗尽后可选地循环返回同一响应。
    struct ScriptedSource {
        queue: VecDeque<Value>,
        endless: Option<Value>,
    }

    impl ScriptedSource {
        fn queue(responses: Vec<Value>) -> Self {
            Self {
                queue: responses.into(),
                endless: None,
            }
        }
        fn endless(v: Value) -> Self {
            Self {
                queue: VecDeque::new(),
                endless: Some(v),
            }
        }
    }

    impl JobStatusSource for ScriptedSource {
        async fn job_status(&mut self, _job_id: &str) -> Result<Value, String> {
            if let Some(v) = self.queue.pop_front() {
                return Ok(v);
            }
            if let Some(v) = &self.endless {
                return Ok(v.clone());
            }
            panic!("ScriptedSource exhausted");
        }
    }

    fn found(v: bool, done: bool, success: bool) -> Value {
        json!({ "found": v, "done": done, "success": success })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_job_done_returns_success() {
        let mut s = ScriptedSource::queue(vec![found(true, true, true)]);
        assert_eq!(
            wait_for_job_impl(
                &mut s,
                "j",
                Duration::from_secs(1),
                Duration::from_millis(1)
            )
            .await,
            Ok(true)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_job_done_returns_failure() {
        let mut s = ScriptedSource::queue(vec![found(true, true, false)]);
        assert_eq!(
            wait_for_job_impl(
                &mut s,
                "j",
                Duration::from_secs(1),
                Duration::from_millis(1)
            )
            .await,
            Ok(false)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_job_not_found_errors() {
        // 从未观测到 found=true 的 found=false：无法区分「真的不存在」
        // 与「已完成但最终结果未观测到」，统一诚实报错。
        let mut s = ScriptedSource::queue(vec![found(false, false, false)]);
        let e = wait_for_job_impl(
            &mut s,
            "j",
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(
            e.contains("job drained before completion result observed"),
            "{e}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_job_drained_before_done_errors() {
        // 真实模型：rootd 在 done=true 之前 success 恒 false；job 在
        // 两次轮询间完成并被 drain 后，下一次查询 found=false。此时
        // 不可猜测 success，必须报错而非误报成功/失败。
        let mut s =
            ScriptedSource::queue(vec![found(true, false, false), found(false, false, false)]);
        let e = wait_for_job_impl(
            &mut s,
            "j",
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(
            e.contains("job drained before completion result observed"),
            "{e}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_job_times_out() {
        let mut s = ScriptedSource::endless(found(true, false, false));
        let e = wait_for_job_impl(
            &mut s,
            "j",
            Duration::from_millis(5),
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(e.contains("did not finish within"), "{e}");
    }

    #[test]
    fn job_cmd_outcome_found_true_exits_zero_equivalent() {
        let out = job_status_outcome(Ok(found(true, true, true))).expect("ok");
        assert!(out.contains("\"found\": true"), "{out}");
        assert!(out.contains("\"success\": true"), "{out}");
    }

    #[test]
    fn job_cmd_outcome_found_false_exits_zero_equivalent() {
        let out = job_status_outcome(Ok(found(false, false, false))).expect("ok");
        assert!(out.contains("\"found\": false"), "{out}");
    }

    #[test]
    fn job_cmd_outcome_rpc_error_propagates() {
        assert_eq!(job_status_outcome(Err("boom".into())), Err("boom".into()));
    }

    #[test]
    fn log_timeout_is_positive_and_explicit() {
        assert_eq!(LOG_QUERY_TIMEOUT.as_secs(), 91);
        let msg = log_query_timeout_error();
        assert!(msg.contains("timed out"), "{msg}");
        assert!(msg.contains("91"), "{msg}");
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

    #[test]
    fn auth_required_matches_confirmation_and_denied() {
        assert!(is_auth_required(1006, "mount.mount (Always)"));
        assert!(is_auth_required(1004, "operation denied"));
    }

    #[test]
    fn auth_required_matches_polkit_message_only() {
        let denied =
            "rootd: org.freedesktop.DBus.Error.AuthFailed: polkit denied action: com.agentshell.mount";
        assert!(is_auth_required(1005, denied));

        let mount_failure =
            "rootd: org.freedesktop.DBus.Error.Failed: mount failed: mount: /mnt: bad superblock";
        assert!(!is_auth_required(1005, mount_failure));
        assert!(!is_auth_required(1003, "not found"));
    }

    #[test]
    fn auth_required_matches_polkit_infra_failure() {
        // polkitd 不可达（CheckAuthorization 调用失败）→ 授权协商故障，exit 2。
        let check =
            "rootd: org.freedesktop.DBus.Error.AuthFailed: polkit check: org.freedesktop.DBus.Error.ServiceUnknown: The name org.freedesktop.PolicyKit1 was not provided by any .service files";
        assert!(is_auth_required(1005, check));

        // polkit 代理构建失败 → 同样归入授权拒绝。
        let proxy = "rootd: org.freedesktop.DBus.Error.AuthFailed: polkit proxy: connection closed";
        assert!(is_auth_required(1005, proxy));
    }

    #[test]
    fn auth_required_rejects_non_auth_1005() {
        // 相对路径 / 非法 fstype 等 rootd 校验错误折叠进 1005，仍 exit 1。
        let rel_device =
            "rootd: org.freedesktop.DBus.Error.Failed: device must be an absolute path";
        assert!(!is_auth_required(1005, rel_device));

        let bad_fstype = "rootd: org.freedesktop.DBus.Error.Failed: invalid fstype: \"ex;t4\"";
        assert!(!is_auth_required(1005, bad_fstype));
    }

    #[test]
    fn pkg_failed_job_line_maps_exit_code_and_unknown() {
        // exit_code 存在 → 内联数值；缺省 → unknown。
        assert_eq!(
            pkg_failed_job_line(&json!({ "exit_code": 1, "stderr": "boom" })),
            "error: job failed (exit 1): boom"
        );
        assert_eq!(
            pkg_failed_job_line(&json!({})),
            "error: job failed (exit unknown): "
        );
    }

    /// `security.audit` 过滤条件与 `a11y.query` 同约定：`None` 条件省略键，
    /// 而非序列化为 JSON `null`（daemon 空串 / 缺键 = 不限制该维度）。
    #[test]
    fn security_audit_params_omits_none_keys() {
        let v = security_audit_params(Some("alice"), None, Some("deny"), None);
        let obj = v.as_object().expect("params must be object");
        assert_eq!(obj.get("agent_id").and_then(|v| v.as_str()), Some("alice"));
        assert_eq!(obj.get("decision").and_then(|v| v.as_str()), Some("deny"));
        assert!(!obj.contains_key("op"), "None op must be omitted");
        assert!(!obj.contains_key("result"), "None result must be omitted");

        let v = security_audit_params(None, Some("windows.list"), None, Some(true));
        let obj = v.as_object().expect("params must be object");
        assert_eq!(obj.get("op").and_then(|v| v.as_str()), Some("windows.list"));
        assert_eq!(obj.get("result").and_then(|v| v.as_bool()), Some(true));
        assert!(
            !obj.contains_key("agent_id"),
            "None agent_id must be omitted"
        );
        assert!(
            !obj.contains_key("decision"),
            "None decision must be omitted"
        );

        let v = security_audit_params(None, None, None, None);
        assert!(
            v.as_object().expect("params must be object").is_empty(),
            "all-None params must be empty object"
        );
    }

    /// `security.*` 分派锚定：方法名与参数形状必须与 daemon RPC 契约一致。
    /// status 走无参形态（`Value::Null`），grant/revoke/audit 各带精确键。
    #[test]
    fn security_request_matches_rpc_contract() {
        use crate::cli::SecurityCommand as S;

        let (m, p) = security_request(&S::Status);
        assert_eq!(m, "security.status");
        assert!(p.is_null(), "status params must be null");

        let (m, p) = security_request(&S::Grant {
            agent_id: "alice".into(),
            level: "L2".into(),
        });
        assert_eq!(m, "security.grant");
        assert_eq!(p, json!({ "agent_id": "alice", "level": "L2" }));

        let (m, p) = security_request(&S::Revoke {
            agent_id: "alice".into(),
        });
        assert_eq!(m, "security.revoke");
        assert_eq!(p, json!({ "agent_id": "alice" }));

        let (m, p) = security_request(&S::Audit {
            agent_id: Some("alice".into()),
            op: None,
            decision: Some("deny".into()),
            result: Some(false),
        });
        assert_eq!(m, "security.audit");
        assert_eq!(
            p,
            json!({ "agent_id": "alice", "decision": "deny", "result": false })
        );
    }

    #[test]
    fn sysctl_rpc_error_maps_auth_required_to_two() {
        let e = CallError::Rpc {
            code: RpcErrorCode::AuthenticationRequired as i32,
            message: "authentication required: com.agentshell.sysctl.get".into(),
        };
        assert_eq!(sysctl_rpc_error(e), Ok(2));
    }

    #[test]
    fn sysctl_set_accepted_line_includes_value() {
        assert_eq!(
            sysctl_set_accepted_line("net.ipv4.ip_forward", "1"),
            "sysctl set net.ipv4.ip_forward=1: accepted"
        );
    }

    #[test]
    fn sysctl_rpc_error_maps_rootd_backend_to_one() {
        let e = CallError::Rpc {
            code: RpcErrorCode::BackendError as i32,
            message: "rootd: boom".into(),
        };
        assert_eq!(sysctl_rpc_error(e), Ok(1));
    }

    #[test]
    fn a11y_query_hit_exits_zero() {
        assert_eq!(a11y_query_exit_code(1, false), 0);
        assert_eq!(a11y_query_exit_code(1, true), 0);
    }

    #[test]
    fn a11y_query_zero_hit_exits_zero_by_default() {
        assert_eq!(a11y_query_exit_code(0, false), 0);
    }

    #[test]
    fn a11y_query_zero_hit_with_fail_on_empty_exits_two() {
        assert_eq!(a11y_query_exit_code(0, true), 2);
    }
}
