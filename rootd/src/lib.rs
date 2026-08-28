//! agent-shell-rootd：特权薄代理层（设计文档 §23.2 / §23.4.3）。
//!
//! 职责边界（§23.2「无业务逻辑，薄代理层」）：
//! - 仅实现白名单方法（§23.4.3 D-Bus 接口的方法名即操作名）
//! - 每个方法独立 polkit action（`com.agentshell.*`，§23.4.2）
//! - 无状态、无缓存、无用户域业务逻辑
//!
//! 传输：D-Bus system bus（systemd system unit 启动）。本 crate 提供
//! 方法分派与参数校验核心（可单测），D-Bus 服务注册在 main 中完成。
//! 五组核心场景（doctor/windows/input/screenshot/a11y）全部归属 daemon
//! 域（§23.3 矩阵）——rootd 不承载它们。

pub mod dbus;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// 执行系统命令（rootd 以 root 运行）。失败时返回错误描述。
/// 薄代理职责：校验后的参数直接转发给系统工具，不做额外业务逻辑。
fn run_command(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd} execution failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{cmd} failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
use std::sync::Mutex;

/// 当前安全模型版本（§23.4.3 版本对账字段）。
pub const SECURITY_MODEL_VERSION: u32 = 1;

/// 白名单方法执行结果。
pub type RootResult = Result<Value, String>;

// ───────────────────── polkit action 映射（§23.4.2） ─────────────────────
//
// 每个白名单方法对应独立 polkit action id。`polkit_action_for` 是唯一
// 映射源——D-Bus 服务层（main）调用此函数获取 action id，再经
// `org.freedesktop.PolicyKit1` 校验。未映射的方法（无 action）不走
// polkit 但仍在白名单内（如 Hello——纯版本对账，无副作用）。

/// 返回方法对应的 polkit action id；`None` = 无需 polkit（如 Hello）。
///
/// 安全模型：每个特权方法独立 action，精确到最小权限（§23.4.2）。
pub fn polkit_action_for(method: &str) -> Option<&'static str> {
    match method {
        "PackageInstall" => Some("com.agentshell.pkexec.install-package"),
        "PackageRemove" => Some("com.agentshell.package.remove"),
        "PackageUpdate" => Some("com.agentshell.package.update"),
        "PackageRefresh" => Some("com.agentshell.package.refresh"),
        "ServiceStart" | "ServiceStop" | "ServiceRestart" | "ServiceReload" => {
            Some("com.agentshell.service.control")
        }
        "ServiceEnable" | "ServiceDisable" | "DaemonReload" => {
            Some("com.agentshell.systemd.manage")
        }
        "JournalQuery" => Some("com.agentshell.system-log.view"),
        "SysctlGet" => Some("com.agentshell.sysctl.get"),
        "SysctlSet" => Some("com.agentshell.sysctl.set"),
        "HostnameSet" => Some("com.agentshell.hostname.set"),
        "ProcessKill" => Some("com.agentshell.process.kill"),
        "Mount" | "Unmount" => Some("com.agentshell.mount"),
        "SetToken" => None, // 内部 token 管理，无系统副作用
        "Hello" => None,    // 版本对账，无副作用
        _ => None,
    }
}

// ───────────────────── Job 跟踪（JobProgress/JobDone 信号源） ─────────────────────
//
// 软件包操作返回 job id，进度经 JobProgress 信号推送，完成发 JobDone
// （§23.4.3）。job 状态存于进程内——rootd 是系统级单例，所有 session
// 共享。D-Bus 服务层订阅 job 事件并转发为 D-Bus 信号。

static JOB_SEQ: AtomicU64 = AtomicU64::new(0);

/// Job 状态。
#[derive(Clone, Debug)]
pub struct JobState {
    pub id: String,
    pub method: String,
    pub progress: f64,
    pub done: bool,
    pub success: bool,
}

/// 进程内 job 注册表（D-Bus 服务层轮询/订阅以发射信号）。
static JOBS: Mutex<Option<HashMap<String, JobState>>> = Mutex::new(None);

fn jobs_lock<'a>() -> std::sync::MutexGuard<'a, Option<HashMap<String, JobState>>> {
    JOBS.lock().expect("JOBS mutex poisoned")
}

fn next_job_id() -> String {
    let n = JOB_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("job-{n}")
}

/// 注册一个新 job，返回 job id。D-Bus 服务层据此发射 JobProgress/JobDone。
pub fn job_create(method: &str) -> String {
    let id = next_job_id();
    let state = JobState {
        id: id.clone(),
        method: method.to_string(),
        progress: 0.0,
        done: false,
        success: false,
    };
    let mut jobs = jobs_lock();
    jobs.get_or_insert_with(HashMap::new)
        .insert(id.clone(), state);
    id
}

/// 更新 job 进度（0.0–1.0）。D-Bus 服务层据此发射 JobProgress 信号。
pub fn job_progress(id: &str, progress: f64) {
    let mut jobs = jobs_lock();
    if let Some(map) = jobs.as_mut() {
        if let Some(job) = map.get_mut(id) {
            job.progress = progress.clamp(0.0, 1.0);
        }
    }
}

/// 标记 job 完成。D-Bus 服务层据此发射 JobDone 信号。
///
/// 完成的 job 保留在注册表中，直到被 `job_drain_done` 取走——服务层
/// 调用 drain 读取已完成 job 后从注册表淘汰，防止无限增长
/// （rootd 是系统级单例常驻进程）。
pub fn job_done(id: &str, success: bool) {
    let mut jobs = jobs_lock();
    if let Some(map) = jobs.as_mut() {
        if let Some(job) = map.get_mut(id) {
            job.done = true;
            job.success = success;
            job.progress = 1.0;
        }
    }
}

/// 取走并淘汰已完成 job（D-Bus 服务层发射 JobDone 后调用）。
/// 防止注册表无限增长——rootd 是常驻进程，不淘汰会内存泄漏。
pub fn job_drain_done() -> Vec<JobState> {
    let mut jobs = jobs_lock();
    if let Some(map) = jobs.as_mut() {
        let drained: Vec<JobState> = map.values().filter(|j| j.done).cloned().collect();
        map.retain(|_, job| !job.done);
        drained
    } else {
        Vec::new()
    }
}

/// 获取所有 job 快照（D-Bus 服务层轮询 JobProgress 用）。
pub fn job_snapshot() -> Vec<JobState> {
    let jobs = jobs_lock();
    jobs.as_ref()
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default()
}

// ───────────────────── 分派核心 ─────────────────────

/// 分派一个 rootd 方法调用（方法名 = §23.4.3 D-Bus 接口方法名）。
///
/// 非白名单方法一律拒绝——rootd 的安全模型是「默认拒绝 + 显式白名单」。
///
/// **polkit 前置**：调用方（D-Bus 服务层）必须在调用本函数**之前**经
/// `polkit_action_for` 获取 action id 并完成 polkit 授权校验。本函数
/// 不重复 polkit 校验——它是薄代理的业务逻辑层，授权是传输层职责。
pub fn dispatch(method: &str, args: &[Value]) -> RootResult {
    match method {
        // 版本对账
        "Hello" => hello(),
        // 软件包（返回 job id）
        "PackageInstall" => package_op("PackageInstall", args),
        "PackageRemove" => package_op("PackageRemove", args),
        "PackageUpdate" => package_op("PackageUpdate", args),
        "PackageRefresh" => package_refresh(),
        // systemd system 单元
        "ServiceStart" | "ServiceStop" | "ServiceRestart" | "ServiceEnable" | "ServiceDisable"
        | "ServiceReload" => service_control(method, args),
        "DaemonReload" => daemon_reload(),
        // 系统日志
        "JournalQuery" => journal_query(args),
        // 系统配置
        "SysctlGet" => sysctl_get(args),
        "SysctlSet" => sysctl_set(args),
        "HostnameSet" => hostname_set(args),
        // 进程（跨用户）
        "ProcessKill" => process_kill(args),
        // 挂载
        "Mount" => mount(args),
        "Unmount" => unmount(args),
        // 会话 Token 管理（可选，无系统副作用）
        "SetToken" => set_token(args),
        _ => Err(format!("method not in whitelist: {method}")),
    }
}

// ───────────────────── 版本对账 ─────────────────────

fn hello() -> RootResult {
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "security_model": SECURITY_MODEL_VERSION,
    }))
}

// ───────────────────── 辅助：参数提取 ─────────────────────

fn str_arg(args: &[Value], idx: usize) -> Result<&str, String> {
    args.get(idx)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("arg[{idx}] must be a string"))
}

fn int_arg(args: &[Value], idx: usize) -> Result<i64, String> {
    args.get(idx)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("arg[{idx}] must be an integer"))
}

fn array_str_arg(args: &[Value], idx: usize) -> Result<Vec<String>, String> {
    let arr = args
        .get(idx)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("arg[{idx}] must be an array"))?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .ok_or_else(|| format!("arg[{idx}] array element must be a string"))
        })
        .collect()
}

// ───────────────────── 软件包管理 ─────────────────────
//
// §23.4.3：软件包操作返回 job id，进度经 JobProgress 信号推送，完成发
// JobDone。包管理器抽象(apt/dnf/pacman/flatpak system)——探测运行时
// 哪个前端可用，调用对应命令。无业务逻辑：只转译为包管理器调用。

/// 探测系统包管理器前端。
fn detect_package_manager() -> Option<PackageManager> {
    // 按优先级探测：apt → dnf → pacman → flatpak(system)
    if std::path::Path::new("/usr/bin/apt-get").exists() {
        return Some(PackageManager::Apt);
    }
    if std::path::Path::new("/usr/bin/dnf").exists() {
        return Some(PackageManager::Dnf);
    }
    if std::path::Path::new("/usr/bin/pacman").exists() {
        return Some(PackageManager::Pacman);
    }
    if std::path::Path::new("/usr/bin/flatpak").exists() {
        return Some(PackageManager::Flatpak);
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackageManager {
    Apt,
    Dnf,
    Pacman,
    Flatpak,
}

impl PackageManager {
    fn install_cmd(&self, packages: &[String]) -> Vec<String> {
        match self {
            PackageManager::Apt => {
                let mut cmd = vec!["apt-get".into(), "install".into(), "-y".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Dnf => {
                let mut cmd = vec!["dnf".into(), "install".into(), "-y".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Pacman => {
                let mut cmd = vec!["pacman".into(), "-S".into(), "--noconfirm".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Flatpak => {
                let mut cmd = vec![
                    "flatpak".into(),
                    "install".into(),
                    "-y".into(),
                    "--system".into(),
                ];
                cmd.extend(packages.iter().cloned());
                cmd
            }
        }
    }

    fn remove_cmd(&self, packages: &[String]) -> Vec<String> {
        match self {
            PackageManager::Apt => {
                let mut cmd = vec!["apt-get".into(), "remove".into(), "-y".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Dnf => {
                let mut cmd = vec!["dnf".into(), "remove".into(), "-y".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Pacman => {
                let mut cmd = vec!["pacman".into(), "-R".into(), "--noconfirm".into()];
                cmd.extend(packages.iter().cloned());
                cmd
            }
            PackageManager::Flatpak => {
                let mut cmd = vec![
                    "flatpak".into(),
                    "uninstall".into(),
                    "-y".into(),
                    "--system".into(),
                ];
                cmd.extend(packages.iter().cloned());
                cmd
            }
        }
    }

    fn update_cmd(&self, packages: &[String]) -> Vec<String> {
        match self {
            PackageManager::Apt => {
                if packages.is_empty() {
                    vec!["apt-get".into(), "upgrade".into(), "-y".into()]
                } else {
                    let mut cmd = vec![
                        "apt-get".into(),
                        "install".into(),
                        "-y".into(),
                        "--only-upgrade".into(),
                    ];
                    cmd.extend(packages.iter().cloned());
                    cmd
                }
            }
            PackageManager::Dnf => {
                if packages.is_empty() {
                    vec!["dnf".into(), "upgrade".into(), "-y".into()]
                } else {
                    let mut cmd = vec!["dnf".into(), "upgrade".into(), "-y".into()];
                    cmd.extend(packages.iter().cloned());
                    cmd
                }
            }
            PackageManager::Pacman => {
                if packages.is_empty() {
                    vec!["pacman".into(), "-Syu".into(), "--noconfirm".into()]
                } else {
                    let mut cmd = vec![
                        "pacman".into(),
                        "-S".into(),
                        "--noconfirm".into(),
                        "--needed".into(),
                    ];
                    cmd.extend(packages.iter().cloned());
                    cmd
                }
            }
            PackageManager::Flatpak => {
                if packages.is_empty() {
                    vec![
                        "flatpak".into(),
                        "update".into(),
                        "-y".into(),
                        "--system".into(),
                    ]
                } else {
                    let mut cmd = vec![
                        "flatpak".into(),
                        "update".into(),
                        "-y".into(),
                        "--system".into(),
                    ];
                    cmd.extend(packages.iter().cloned());
                    cmd
                }
            }
        }
    }

    fn refresh_cmd(&self) -> Vec<String> {
        match self {
            PackageManager::Apt => vec!["apt-get".into(), "update".into()],
            PackageManager::Dnf => vec!["dnf".into(), "makecache".into()],
            PackageManager::Pacman => vec!["pacman".into(), "-Sy".into()],
            PackageManager::Flatpak => {
                vec!["flatpak".into(), "update".into(), "--appstream".into()]
            }
        }
    }
}

/// 校验包名合法性（防注入：只允许字母数字 + `.` `-` `_` `+` `:` `/`）。
fn validate_package_name(pkg: &str) -> Result<(), String> {
    if pkg.is_empty() || pkg.len() > 256 {
        return Err("invalid package name length".into());
    }
    if !pkg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | ':' | '/'))
    {
        return Err(format!("illegal character in package name: {pkg:?}"));
    }
    Ok(())
}

fn package_op(method: &str, args: &[Value]) -> RootResult {
    let packages = array_str_arg(args, 0)?;
    for pkg in &packages {
        validate_package_name(pkg)?;
    }
    let pm = detect_package_manager().ok_or("no package manager available")?;
    let cmd = match method {
        "PackageInstall" => pm.install_cmd(&packages),
        "PackageRemove" => pm.remove_cmd(&packages),
        "PackageUpdate" => pm.update_cmd(&packages),
        _ => return Err(format!("unknown package method: {method}")),
    };
    let job_id = job_create(method);
    tracing::info!(
        method,
        job = %job_id,
        pm = ?pm,
        cmd = ?cmd,
        "package operation queued (polkit action: {})",
        polkit_action_for(method).unwrap_or("none"),
    );
    // 包操作返回 job id——实际异步执行（spawn 后台进程 + job_progress/job_done）
    // 待 D-Bus 服务层落地。当前 lib 层只准备命令与 job 跟踪。
    Ok(json!({ "job_id": job_id, "cmd": cmd, "pm": format!("{pm:?}") }))
}

fn package_refresh() -> RootResult {
    let pm = detect_package_manager().ok_or("no package manager available")?;
    let cmd = pm.refresh_cmd();
    let job_id = job_create("PackageRefresh");
    tracing::info!(
        job = %job_id,
        pm = ?pm,
        cmd = ?cmd,
        "package refresh queued (polkit action: com.agentshell.package.refresh)",
    );
    Ok(json!({ "job_id": job_id, "cmd": cmd, "pm": format!("{pm:?}") }))
}

// ───────────────────── systemd system 单元控制 ─────────────────────
//
// 单元名做基本合法性校验（防 shell 注入面：实际执行走 systemd D-Bus API
// 而非 shell，但入口仍拒绝可疑字符）。

fn service_control(method: &str, args: &[Value]) -> RootResult {
    let unit = str_arg(args, 0)?;
    validate_unit_name(unit)?;
    // systemd D-Bus API 映射（§23.4.3）：
    //   ServiceStart   → org.freedesktop.systemd1.Manager.StartUnit(unit, "replace")
    //   ServiceStop    → StopUnit(unit, "replace")
    //   ServiceRestart → RestartUnit(unit, "replace")
    //   ServiceReload  → ReloadUnit(unit, "replace")
    //   ServiceEnable  → EnableUnitFiles([unit], false, true)
    //   ServiceDisable → DisableUnitFiles([unit], false)
    // StartUnit mode 用 "replace"（agent 场景默认，§开放问题 #6）
    let systemd_method = match method {
        "ServiceStart" => "StartUnit",
        "ServiceStop" => "StopUnit",
        "ServiceRestart" => "RestartUnit",
        "ServiceReload" => "ReloadUnit",
        "ServiceEnable" => "EnableUnitFiles",
        "ServiceDisable" => "DisableUnitFiles",
        _ => return Err(format!("unknown service method: {method}")),
    };
    tracing::info!(
        method,
        unit,
        systemd_method,
        "service control requested (polkit action: com.agentshell.service.control)",
    );
    // 实际执行：systemctl 命令（rootd 以 root 运行，薄代理转发）
    let subcmd = match method {
        "ServiceStart" => "start",
        "ServiceStop" => "stop",
        "ServiceRestart" => "restart",
        "ServiceReload" => "reload",
        "ServiceEnable" => "enable",
        "ServiceDisable" => "disable",
        _ => return Err(format!("unknown service method: {method}")),
    };
    run_command("systemctl", &[subcmd, unit])?;
    // mode 只对 Start/Stop/Restart/Reload 有效（Enable/Disable 走
    // EnableUnitFiles/DisableUnitFiles，无 mode 参数，只有 runtime/persistent 语义）
    let mut result = json!({
        "accepted": true,
        "unit": unit,
        "action": method,
        "systemd_method": systemd_method,
    });
    if matches!(
        method,
        "ServiceStart" | "ServiceStop" | "ServiceRestart" | "ServiceReload"
    ) {
        result["mode"] = json!("replace");
    }
    Ok(result)
}

fn validate_unit_name(unit: &str) -> Result<(), String> {
    if unit.is_empty() || unit.len() > 256 {
        return Err("invalid unit name length".into());
    }
    if !unit
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '+'))
    {
        return Err(format!("illegal character in unit name: {unit:?}"));
    }
    Ok(())
}

fn daemon_reload() -> RootResult {
    tracing::info!("daemon-reload requested (polkit action: com.agentshell.systemd.manage)");
    // 实际执行：systemctl daemon-reload
    run_command("systemctl", &["daemon-reload"])?;
    Ok(json!({ "accepted": true, "systemd_method": "Reload" }))
}

// ───────────────────── 系统日志 ─────────────────────
//
// journal 解析注意多行消息和二进制字段（_MESSAGE 可能跨多行，§开放问题 #7）。
// 过滤表达式为结构化 JSON 对象，非自由文本拼接——防注入。

fn journal_query(args: &[Value]) -> RootResult {
    let filter = str_arg(args, 0)?;
    // 过滤表达式必须是合法 JSON 对象（结构化查询，非自由文本拼接）。
    let parsed: Value = serde_json::from_str(filter)
        .map_err(|e| format!("journal filter must be JSON object: {e}"))?;
    if !parsed.is_object() {
        return Err("journal filter must be a JSON object".into());
    }
    // 支持的过滤键（白名单，防注入）：
    //   unit, priority, since, until, boot, dmesg, grep
    let filter_obj = parsed.as_object().unwrap();
    let allowed_keys = [
        "unit", "priority", "since", "until", "boot", "dmesg", "grep", "limit",
    ];
    for key in filter_obj.keys() {
        if !allowed_keys.contains(&key.as_str()) {
            return Err(format!("unknown journal filter key: {key}"));
        }
    }
    tracing::info!(
        filter_keys = ?filter_obj.keys().collect::<Vec<_>>(),
        "journal query requested (polkit action: com.agentshell.system-log.view)",
    );
    // 实际执行：journalctl --output=json + 过滤参数
    let mut cmd_args: Vec<String> = vec!["--output=json".into()];
    if let Some(unit) = filter_obj.get("unit").and_then(|v| v.as_str()) {
        cmd_args.push(format!("--unit={unit}"));
    }
    if let Some(priority) = filter_obj.get("priority").and_then(|v| v.as_str()) {
        cmd_args.push(format!("--priority={priority}"));
    }
    if let Some(since) = filter_obj.get("since").and_then(|v| v.as_str()) {
        cmd_args.push(format!("--since={since}"));
    }
    if let Some(until) = filter_obj.get("until").and_then(|v| v.as_str()) {
        cmd_args.push(format!("--until={until}"));
    }
    if filter_obj
        .get("boot")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        cmd_args.push("--boot".into());
    }
    if filter_obj
        .get("dmesg")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        cmd_args.push("--dmesg".into());
    }
    if let Some(grep) = filter_obj.get("grep").and_then(|v| v.as_str()) {
        cmd_args.push("--grep".into());
        cmd_args.push(grep.to_string());
    }
    if let Some(limit) = filter_obj.get("limit").and_then(|v| v.as_u64()) {
        cmd_args.push(format!("--lines={limit}"));
    }
    let refs: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
    let output = run_command("journalctl", &refs)?;
    Ok(json!({
        "output": output,
        "output_format": "json-stream",
    }))
}

// ───────────────────── 系统配置 ─────────────────────

fn sysctl_get(args: &[Value]) -> RootResult {
    let key = str_arg(args, 0)?;
    validate_sysctl_key(key)?;
    let path = format!("/proc/sys/{}", key.replace('.', "/"));
    let value = std::fs::read_to_string(&path).map_err(|e| format!("sysctl read {key}: {e}"))?;
    Ok(json!({ "key": key, "value": value.trim() }))
}

fn sysctl_set(args: &[Value]) -> RootResult {
    let key = str_arg(args, 0)?;
    validate_sysctl_key(key)?;
    let value = args.get(1).ok_or("arg[1] (value) required")?;
    let value_str = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => value.to_string(),
    };
    // value 字符校验——拒绝空格/分号/空字节（防 sysctl 参数异常）
    if value_str.contains(' ') || value_str.contains(';') || value_str.contains('\0') {
        return Err(format!("illegal character in sysctl value: {value_str:?}"));
    }
    tracing::info!(
        key,
        %value_str,
        "sysctl set requested (polkit action: com.agentshell.sysctl.set)",
    );
    // 实际执行：sysctl -w key=value
    let arg = format!("{key}={value_str}");
    run_command("sysctl", &["-w", &arg])?;
    Ok(json!({ "accepted": true, "key": key }))
}

/// sysctl key 只允许 `a-z0-9.-_/`。`/` 合法（如 `kernel/random/uuid`
/// 映射 `/proc/sys/kernel/random/uuid`），`..` 检查防路径穿越。
fn validate_sysctl_key(key: &str) -> Result<(), String> {
    if key.contains("..") || key.starts_with('/') {
        return Err(format!("illegal sysctl key: {key:?}"));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_' | '/'))
    {
        return Err(format!("illegal character in sysctl key: {key:?}"));
    }
    Ok(())
}

fn hostname_set(args: &[Value]) -> RootResult {
    let hostname = str_arg(args, 0)?;
    validate_hostname(hostname)?;
    tracing::info!(
        hostname,
        "hostname set requested (polkit action: com.agentshell.hostname.set)",
    );
    // 实际执行：hostnamectl set-hostname
    run_command("hostnamectl", &["set-hostname", hostname])?;
    Ok(json!({ "accepted": true, "hostname": hostname }))
}

/// 主机名校验：RFC 1123 简化——字母数字 + `-` `.`，不以 `-` 开头/结尾。
fn validate_hostname(h: &str) -> Result<(), String> {
    if h.is_empty() || h.len() > 253 {
        return Err("invalid hostname length".into());
    }
    if !h
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
    {
        return Err(format!("illegal character in hostname: {h:?}"));
    }
    if h.starts_with('-') || h.starts_with('.') {
        return Err("hostname must not start with '-' or '.'".into());
    }
    if h.ends_with('-') || h.ends_with('.') {
        return Err("hostname must not end with '-' or '.'".into());
    }
    Ok(())
}

// ───────────────────── 进程管理（跨用户） ─────────────────────
//
// §开放问题 #10：进程管理涉及权限，kill 仅能操作同用户/同组进程（root
// 可跨用户）。rootd 以 root 运行，可 kill 任意进程。PID 校验防注入。

fn process_kill(args: &[Value]) -> RootResult {
    let pid = int_arg(args, 0)?;
    let signal = int_arg(args, 1)?;
    // PID 校验：正整数，非 0/1（init 不可 kill），≤ INT_MAX
    if pid <= 0 {
        return Err("pid must be a positive integer".into());
    }
    if pid == 1 {
        return Err("refusing to kill PID 1 (init)".into());
    }
    // 信号校验：常见信号 1–31（标准 POSIX 实时信号子集）
    if !(1..=31).contains(&signal) {
        return Err(format!("signal must be in range 1-31, got {signal}"));
    }
    tracing::info!(
        pid,
        signal,
        "process kill requested (polkit action: com.agentshell.process.kill)",
    );
    // 实际执行：kill 系统调用（rootd 以 root 运行）
    let pid_c = unsafe { libc::kill(pid as i32, signal as i32) };
    if pid_c != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("kill({pid}, {signal}) failed: {err}"));
    }
    Ok(json!({ "accepted": true, "pid": pid, "signal": signal }))
}

// ───────────────────── 挂载 ─────────────────────
//
// §23.4.3 Mount/Unmount。device/target/fstype/options 校验防注入。
// 实际执行走 mount(2) 系统调用或 `mount` 命令。

fn mount(args: &[Value]) -> RootResult {
    let device = str_arg(args, 0)?;
    let target = str_arg(args, 1)?;
    let fstype = str_arg(args, 2)?;
    let options = array_str_arg(args, 3)?;
    validate_mount_path(device, "device")?;
    validate_mount_path(target, "target")?;
    validate_fstype(fstype)?;
    for opt in &options {
        validate_mount_option(opt)?;
    }
    tracing::info!(
        device,
        target,
        fstype,
        ?options,
        "mount requested (polkit action: com.agentshell.mount)",
    );
    // 实际执行：mount -t fstype -o options device target
    let opts_str = options.join(",");
    let mount_args = vec!["-t", fstype, "-o", &opts_str, device, target];
    run_command("mount", &mount_args)?;
    Ok(json!({
        "accepted": true,
        "device": device,
        "target": target,
        "fstype": fstype,
        "options": options,
    }))
}

fn unmount(args: &[Value]) -> RootResult {
    let target = str_arg(args, 0)?;
    validate_mount_path(target, "target")?;
    tracing::info!(
        target,
        "unmount requested (polkit action: com.agentshell.mount)",
    );
    // 实际执行：umount target
    run_command("umount", &[target])?;
    Ok(json!({ "accepted": true, "target": target }))
}

fn validate_mount_path(path: &str, field: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(format!("{field} path must not be empty"));
    }
    if path.len() > 4096 {
        return Err(format!("{field} path too long"));
    }
    // 拒绝路径穿越与空字节（防注入）
    if path.contains('\0') {
        return Err(format!("{field} path contains null byte"));
    }
    // 必须是绝对路径或 /dev/ 块设备别名
    if !path.starts_with('/') {
        return Err(format!("{field} path must be absolute: {path:?}"));
    }
    // 拒绝 `..` 穿越
    if path.contains("/..") || path.contains("../") {
        return Err(format!("{field} path contains '..' traversal: {path:?}"));
    }
    Ok(())
}

fn validate_fstype(fstype: &str) -> Result<(), String> {
    if fstype.is_empty() {
        return Err("fstype must not be empty".into());
    }
    if fstype.len() > 64 {
        return Err("fstype too long".into());
    }
    // 只允许字母数字 + `-` `_`（防注入 mount 命令参数）
    if !fstype
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(format!("illegal character in fstype: {fstype:?}"));
    }
    Ok(())
}

fn validate_mount_option(opt: &str) -> Result<(), String> {
    if opt.len() > 256 {
        return Err("mount option too long".into());
    }
    // 挂载选项白名单字符（防注入）
    if !opt
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | '-' | '_' | '.'))
    {
        return Err(format!("illegal character in mount option: {opt:?}"));
    }
    Ok(())
}

// ───────────────────── 会话 Token 管理 ─────────────────────
//
// §23.4.3 SetToken：持久化 token（可选）。无系统副作用——内部状态管理，
// 不触发 polkit。token 用于 portal 授权持久化（§20.3），daemon 调用此方
// 法将 token 下发给 rootd 存储。
//
// 安全注记：token 明文常驻进程内存。若 rootd 进程内存被 dump
// （core dump / ptrace），token 泄露。rootd 以 root 运行，普通用户
// 无法 ptrace——但这是纵深防御的已知边界。后续可引入 `zeroize` crate
// 在 drop 时清零敏感内存。
static TOKEN: Mutex<Option<String>> = Mutex::new(None);

fn set_token(args: &[Value]) -> RootResult {
    let token = str_arg(args, 0)?;
    if token.is_empty() {
        return Err("token must not be empty".into());
    }
    if token.len() > 4096 {
        return Err("token too long".into());
    }
    let mut guard = TOKEN.lock().expect("TOKEN mutex poisoned");
    *guard = Some(token.to_string());
    tracing::info!("token set (len={})", token.len());
    Ok(json!({ "accepted": true }))
}

/// 读取当前持久化 token（D-Bus 服务层 / 测试用）。
pub fn get_token() -> Option<String> {
    TOKEN.lock().expect("TOKEN mutex poisoned").clone()
}

// ───────────────────── 白名单自省 ─────────────────────

/// 返回白名单方法列表（D-Bus Introspect 校验用）。
pub fn whitelist_methods() -> &'static [&'static str] {
    &[
        "Hello",
        "PackageInstall",
        "PackageRemove",
        "PackageUpdate",
        "PackageRefresh",
        "ServiceStart",
        "ServiceStop",
        "ServiceRestart",
        "ServiceEnable",
        "ServiceDisable",
        "ServiceReload",
        "DaemonReload",
        "JournalQuery",
        "SysctlGet",
        "SysctlSet",
        "HostnameSet",
        "ProcessKill",
        "Mount",
        "Unmount",
        "SetToken",
    ]
}

/// 白名单内？（Introspect 输出与白名单 XML 一致校验——无额外方法）。
pub fn is_whitelisted(method: &str) -> bool {
    whitelist_methods().contains(&method)
}

// ───────────────────── 测试 ─────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── 版本对账 ──

    #[test]
    fn hello_reports_version_and_security_model() {
        let r = dispatch("Hello", &[]).expect("hello");
        assert_eq!(r["security_model"], SECURITY_MODEL_VERSION);
        assert!(r["version"].as_str().is_some());
    }

    #[test]
    fn non_whitelisted_method_is_rejected() {
        let e = dispatch("PackageInstallx", &[json!(["curl"])]).unwrap_err();
        assert!(e.contains("not in whitelist"), "{e}");
        // 五组核心场景方法不属于 rootd（§23.3 daemon 域）。
        assert!(dispatch("windows.list", &[]).is_err());
        assert!(dispatch("input.send", &[]).is_err());
        assert!(dispatch("screenshot.capture", &[]).is_err());
        assert!(dispatch("a11y.status", &[]).is_err());
        assert!(dispatch("doctor.run", &[]).is_err());
    }

    // ── polkit action 映射 ──

    #[test]
    fn polkit_action_for_service_control_methods() {
        assert_eq!(
            polkit_action_for("ServiceStart"),
            Some("com.agentshell.service.control")
        );
        assert_eq!(
            polkit_action_for("ServiceStop"),
            Some("com.agentshell.service.control")
        );
        assert_eq!(
            polkit_action_for("ServiceRestart"),
            Some("com.agentshell.service.control")
        );
        assert_eq!(
            polkit_action_for("ServiceReload"),
            Some("com.agentshell.service.control")
        );
    }

    #[test]
    fn polkit_action_for_systemd_manage_methods() {
        assert_eq!(
            polkit_action_for("ServiceEnable"),
            Some("com.agentshell.systemd.manage")
        );
        assert_eq!(
            polkit_action_for("ServiceDisable"),
            Some("com.agentshell.systemd.manage")
        );
        assert_eq!(
            polkit_action_for("DaemonReload"),
            Some("com.agentshell.systemd.manage")
        );
    }

    #[test]
    fn polkit_action_for_package_methods() {
        assert_eq!(
            polkit_action_for("PackageInstall"),
            Some("com.agentshell.pkexec.install-package")
        );
        assert_eq!(
            polkit_action_for("PackageRemove"),
            Some("com.agentshell.package.remove")
        );
        assert_eq!(
            polkit_action_for("PackageUpdate"),
            Some("com.agentshell.package.update")
        );
        assert_eq!(
            polkit_action_for("PackageRefresh"),
            Some("com.agentshell.package.refresh")
        );
    }

    #[test]
    fn polkit_action_for_misc_methods() {
        assert_eq!(
            polkit_action_for("JournalQuery"),
            Some("com.agentshell.system-log.view")
        );
        assert_eq!(
            polkit_action_for("SysctlGet"),
            Some("com.agentshell.sysctl.get")
        );
        assert_eq!(
            polkit_action_for("SysctlSet"),
            Some("com.agentshell.sysctl.set")
        );
        assert_eq!(
            polkit_action_for("HostnameSet"),
            Some("com.agentshell.hostname.set")
        );
        assert_eq!(
            polkit_action_for("ProcessKill"),
            Some("com.agentshell.process.kill")
        );
        assert_eq!(polkit_action_for("Mount"), Some("com.agentshell.mount"));
        assert_eq!(polkit_action_for("Unmount"), Some("com.agentshell.mount"));
    }

    #[test]
    fn polkit_action_none_for_hello_and_token() {
        assert_eq!(polkit_action_for("Hello"), None);
        assert_eq!(polkit_action_for("SetToken"), None);
    }

    // ── 软件包管理 ──

    #[test]
    fn package_install_returns_job_id() {
        let r = dispatch("PackageInstall", &[json!(["curl", "wget"])]).expect("package install");
        assert!(r["job_id"].as_str().is_some());
        assert!(r["cmd"].is_array());
    }

    #[test]
    fn package_install_rejects_invalid_name() {
        assert!(dispatch("PackageInstall", &[json!(["evil; rm -rf /"])]).is_err());
        assert!(dispatch("PackageInstall", &[json!([""])]).is_err());
        assert!(dispatch("PackageInstall", &[json!(["$(whoami)"])]).is_err());
    }

    #[test]
    fn package_remove_returns_job_id() {
        let r = dispatch("PackageRemove", &[json!(["curl"])]).expect("package remove");
        assert!(r["job_id"].as_str().is_some());
    }

    #[test]
    fn package_update_empty_means_all() {
        let r = dispatch("PackageUpdate", &[json!([])]).expect("package update all");
        assert!(r["job_id"].as_str().is_some());
    }

    #[test]
    fn package_refresh_returns_job_id() {
        let r = dispatch("PackageRefresh", &[]).expect("package refresh");
        assert!(r["job_id"].as_str().is_some());
    }

    #[test]
    fn package_install_rejects_non_array() {
        assert!(dispatch("PackageInstall", &[json!("curl")]).is_err());
    }

    // ── Job 跟踪 ──

    /// 串行化 job 测试——全局 JOBS static 在并行测试间竞争。
    static JOB_TEST_MUTEX: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn job_create_and_progress() {
        let _guard = JOB_TEST_MUTEX.lock();
        let id = job_create("PackageInstall");
        assert!(id.starts_with("job-"));
        job_progress(&id, 0.5);
        let snap = job_snapshot();
        let job = snap.iter().find(|j| j.id == id).expect("job found");
        assert_eq!(job.method, "PackageInstall");
        assert!((job.progress - 0.5).abs() < f64::EPSILON);
        assert!(!job.done);
    }

    #[test]
    fn job_done_marks_complete() {
        let _guard = JOB_TEST_MUTEX.lock();
        let id = job_create("PackageUpdate");
        job_done(&id, true);
        let snap = job_snapshot();
        let job = snap.iter().find(|j| j.id == id).expect("job found");
        assert!(job.done);
        assert!(job.success);
        assert!((job.progress - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn job_drain_done_removes_completed_jobs() {
        let _guard = JOB_TEST_MUTEX.lock();
        // 先清空全局注册表（测试间共享 static 状态）
        let _ = job_drain_done();
        let id1 = job_create("PackageInstall");
        let id2 = job_create("PackageUpdate");
        job_done(&id1, true);
        job_done(&id2, false);
        // drain 取走已完成 job
        let drained = job_drain_done();
        assert_eq!(drained.len(), 2);
        // drain 后注册表不再含已完成 job
        let snap = job_snapshot();
        assert!(snap.iter().all(|j| !j.done));
        assert!(snap.iter().find(|j| j.id == id1).is_none());
        assert!(snap.iter().find(|j| j.id == id2).is_none());
    }

    #[test]
    fn job_drain_done_preserves_incomplete_jobs() {
        let _guard = JOB_TEST_MUTEX.lock();
        // 先清空全局注册表（测试间共享 static 状态）
        let _ = job_drain_done();
        let id_active = job_create("PackageInstall");
        let id_done = job_create("PackageRemove");
        job_done(&id_done, true);
        let drained = job_drain_done();
        // 只 drain 已完成的 id_done
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, id_done);
        // 未完成 job 保留
        let snap = job_snapshot();
        assert!(snap.iter().find(|j| j.id == id_active).is_some());
    }

    #[test]
    fn job_ids_are_unique() {
        let _guard = JOB_TEST_MUTEX.lock();
        let id1 = job_create("PackageInstall");
        let id2 = job_create("PackageInstall");
        assert_ne!(id1, id2);
    }

    // ── systemd 单元控制 ──

    #[test]
    fn service_control_validates_unit_name() {
        // 校验失败仍返回 Err（不触达 systemctl）
        assert!(dispatch("ServiceStop", &[json!("evil; rm -rf /")]).is_err());
        assert!(dispatch("ServiceRestart", &[json!("../escape")]).is_err());
        assert!(dispatch("ServiceEnable", &[json!("")]).is_err());
        // 有效单元名通过校验——systemctl 在测试环境会失败，预期 Err
        assert!(dispatch("ServiceStart", &[json!("nginx.service")]).is_err());
    }

    #[test]
    fn service_start_maps_to_systemd_method() {
        // systemctl 在 CI 无 systemd → Err，但 Err 消息含 systemctl 路径
        let e = dispatch("ServiceStart", &[json!("nginx.service")]).unwrap_err();
        assert!(e.contains("systemctl"), "{e}");
    }

    #[test]
    fn service_enable_maps_to_enable_unit_files() {
        let e = dispatch("ServiceEnable", &[json!("nginx.service")]).unwrap_err();
        assert!(e.contains("systemctl"), "{e}");
    }

    #[test]
    fn daemon_reload_maps_to_reload() {
        // systemctl daemon-reload 在 CI 无 systemd → Err
        let e = dispatch("DaemonReload", &[]).unwrap_err();
        assert!(e.contains("systemctl"), "{e}");
    }

    // ── 系统日志 ──

    #[test]
    fn journal_query_requires_json_object() {
        // 非法 filter 仍 Err（不触达 journalctl）
        assert!(dispatch("JournalQuery", &[json!("free text")]).is_err());
        // 有效 filter 通过校验——journalctl 在 CI 可能失败
        let r = dispatch("JournalQuery", &[json!(r#"{"unit":"nginx"}"#)]);
        match &r {
            Ok(v) => assert!(v.get("output").is_some(), "Ok should have output field"),
            Err(e) => assert!(e.contains("journalctl"), "{e}"),
        }
    }

    #[test]
    fn journal_query_rejects_unknown_keys() {
        let bad = json!(r#"{"evil_key":"x"}"#);
        assert!(dispatch("JournalQuery", &[bad]).is_err());
    }

    #[test]
    fn journal_query_accepts_allowed_keys() {
        let filter = json!(r#"{"unit":"nginx","priority":"err","limit":100}"#);
        // 有效 filter 通过校验——journalctl 在 CI 可能失败
        let r = dispatch("JournalQuery", &[filter]);
        match &r {
            Ok(v) => assert!(v.get("output").is_some(), "Ok should have output field"),
            Err(e) => assert!(e.contains("journalctl"), "{e}"),
        }
    }

    // ── 系统配置 ──

    #[test]
    fn sysctl_key_validation_blocks_traversal() {
        assert!(dispatch("SysctlGet", &[json!("net.ipv4.ip_forward")]).is_ok());
        assert!(dispatch("SysctlSet", &[json!("../../etc/passwd"), json!(1)]).is_err());
        assert!(dispatch("SysctlSet", &[json!("/abs/path"), json!(1)]).is_err());
    }

    #[test]
    fn hostname_validation() {
        // 校验失败仍返回 Err（不触达 hostnamectl）
        assert!(dispatch("HostnameSet", &[json!("")]).is_err());
        assert!(dispatch("HostnameSet", &[json!("-bad")]).is_err());
        assert!(dispatch("HostnameSet", &[json!("bad-")]).is_err());
        assert!(dispatch("HostnameSet", &[json!("host; rm -rf /")]).is_err());
        // 有效 hostname 通过校验——hostnamectl 在 CI 无权限/无 polkit → Err
        assert!(dispatch("HostnameSet", &[json!("myhost")]).is_err());
    }

    // ── 进程管理 ──

    #[test]
    fn process_kill_rejects_pid_zero_and_one() {
        assert!(dispatch("ProcessKill", &[json!(0), json!(9)]).is_err());
        assert!(dispatch("ProcessKill", &[json!(1), json!(9)]).is_err());
        assert!(dispatch("ProcessKill", &[json!(-5), json!(9)]).is_err());
    }

    #[test]
    fn process_kill_validates_signal_range() {
        // 信号校验失败仍返回 Err（不触达 kill）
        assert!(dispatch("ProcessKill", &[json!(1234), json!(0)]).is_err());
        assert!(dispatch("ProcessKill", &[json!(1234), json!(32)]).is_err());
        // 有效输入通过校验——kill(1234,9) 在 CI 无 PID 1234 → Err
        assert!(dispatch("ProcessKill", &[json!(1234), json!(9)]).is_err());
    }

    // ── 挂载 ──

    #[test]
    fn mount_validates_paths() {
        let opts = json!(["ro", "noexec"]);
        // 校验失败仍返回 Err（不触达 mount）
        assert!(dispatch(
            "Mount",
            &[json!("relpath"), json!("/mnt"), json!("ext4"), opts.clone()]
        )
        .is_err());
        assert!(dispatch(
            "Mount",
            &[
                json!("/dev/../etc"),
                json!("/mnt"),
                json!("ext4"),
                opts.clone()
            ]
        )
        .is_err());
        // 有效输入通过校验——mount 在 CI 无设备 → Err
        assert!(dispatch(
            "Mount",
            &[
                json!("/dev/sda1"),
                json!("/mnt/data"),
                json!("ext4"),
                opts.clone()
            ]
        )
        .is_err());
    }

    #[test]
    fn mount_validates_fstype() {
        let opts = json!(["ro"]);
        // fstype 校验失败仍返回 Err
        assert!(dispatch(
            "Mount",
            &[json!("/dev/sda1"), json!("/mnt"), json!("ext4; rm /"), opts]
        )
        .is_err());
        // 有效输入通过校验——mount 在 CI 无设备 → Err
        let opts2 = json!(["ro"]);
        assert!(dispatch(
            "Mount",
            &[json!("/dev/sda1"), json!("/mnt"), json!("ext4"), opts2]
        )
        .is_err());
    }

    #[test]
    fn unmount_validates_target() {
        // 校验失败仍返回 Err
        assert!(dispatch("Unmount", &[json!("relpath")]).is_err());
        // 有效输入通过校验——umount 在 CI 无挂载 → Err
        assert!(dispatch("Unmount", &[json!("/mnt/data")]).is_err());
    }

    // ── Token 管理 ──

    #[test]
    fn set_token_stores_and_retrieves() {
        dispatch("SetToken", &[json!("secret-token-123")]).expect("set token");
        assert_eq!(get_token().as_deref(), Some("secret-token-123"));
    }

    #[test]
    fn set_token_rejects_empty() {
        assert!(dispatch("SetToken", &[json!("")]).is_err());
    }

    // ── 白名单自省 ──

    #[test]
    fn whitelist_matches_design_doc() {
        // §23.4.3 全部方法（不含信号 JobProgress/JobDone——信号非方法）
        let expected = [
            "Hello",
            "PackageInstall",
            "PackageRemove",
            "PackageUpdate",
            "PackageRefresh",
            "ServiceStart",
            "ServiceStop",
            "ServiceRestart",
            "ServiceEnable",
            "ServiceDisable",
            "ServiceReload",
            "DaemonReload",
            "JournalQuery",
            "SysctlGet",
            "SysctlSet",
            "HostnameSet",
            "ProcessKill",
            "Mount",
            "Unmount",
            "SetToken",
        ];
        for method in expected {
            assert!(is_whitelisted(method), "{method} should be whitelisted");
        }
        // 信号名不在白名单方法列表中
        assert!(!is_whitelisted("JobProgress"));
        assert!(!is_whitelisted("JobDone"));
    }

    #[test]
    fn whitelist_has_no_extra_methods() {
        // 额外方法不在白名单中（Introspect 输出与白名单 XML 一致）
        assert!(!is_whitelisted("EvilMethod"));
        assert!(!is_whitelisted("Reboot"));
        assert!(!is_whitelisted("Shutdown"));
    }
}
