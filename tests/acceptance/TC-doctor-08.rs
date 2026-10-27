//! TC-doctor-08（AC-110，P1）：各桌面环境后端组件槽填充矩阵与健康/执行一致性。
//!
//! - 场景：应填的槽非空、标为不提供的槽报明确不可用、健康报告与命令执行路径
//!   对同一能力的判定一致
//! - 判据：槽集合与装配契约一致；结构性缺能力明确不可用；就绪能力不返回后端
//!   不可用，不可用能力不静默成功
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-doctor-08`
//!
//! 无图形 CI 只能建立 TTY 与「夹具合成器」两种装配形态：TTY 的缺能力是结构性
//! 的（C-033），夹具合成器提供窗口/工作区/事件槽。五种目标组合的真实后端装配
//! 需要各自桌面会话，由真机验收覆盖；本用例断言可判定的槽矩阵与两路一致性。

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, FakeCompositor, Fixture, PrivateBus, RecordingInput, ScreenshotMock};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, DoctorResult, InfoResult, RpcErrorCode};
use serde_json::json;

fn info(value: serde_json::Value) -> InfoResult {
    serde_json::from_value(value).expect("InfoResult")
}

fn capability<'a>(info: &'a InfoResult, name: &str) -> &'a agent_shell_rpc::CapabilityStatus {
    info.capabilities
        .iter()
        .find(|(row, _)| row == name)
        .map(|(_, status)| status)
        .unwrap_or_else(|| panic!("missing capability row {name}"))
}

#[tokio::test]
async fn component_slot_matrix_matches_execution_paths() {
    // TTY：窗口/工作区/捕获/输入/无障碍全部结构性不可用；执行路径同判。
    let mut tty = Fixture::start().await;
    let caps = info(tty.call_ok(method::INFO, None).await);
    for name in [
        "window_management",
        "workspace_management",
        "window_events",
        "workspace_events",
        "monitor_layout",
        "native_input",
        "native_capture",
    ] {
        assert_eq!(
            capability(&caps, name),
            &agent_shell_rpc::CapabilityStatus::Disabled,
            "TTY slot {name} must be explicitly disabled"
        );
    }
    for (name, call) in [
        ("windows", method::WINDOWS_LIST),
        ("workspaces", method::WORKSPACES_LIST),
    ] {
        let error = tty.call_err(call, None).await;
        assert_eq!(
            error.code,
            RpcErrorCode::BackendUnavailable as i32,
            "{name} must fail with backend unavailable in TTY, not silently succeed"
        );
    }
    let error = tty
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(json!({"output_path": "/tmp/never.png"})),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
    let error = tty
        .call_err(method::A11Y_QUERY, Some(json!({"role": "push button"})))
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);

    let value = tty.call_ok(method::DOCTOR, None).await;
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    let compositor = report
        .lines
        .iter()
        .find(|line| !line.starts_with(' ') && line.contains("合成器"))
        .expect("compositor row");
    assert!(
        compositor.starts_with("✗") && compositor.contains("unavailable in this session"),
        "structural capability absence must be explicit: {compositor}"
    );
    assert!(
        report.lines.iter().all(|line| !line.contains("Shell 扩展")),
        "non-GNOME sessions must not render the GNOME extension slot"
    );

    // 夹具合成器 + portal + 输入链：应填的槽非空，执行路径不再报后端不可用。
    let bus = PrivateBus::start();
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let _portal = serve_screenshot_portal(&bus, mock).await;
    let mut filled =
        Fixture::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string())]).await;
    let compositor = FakeCompositor::new();
    filled.install_compositor(&compositor, DesktopEnvironment::KDE);
    filled
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![Box::new(RecordingInput::new("record"))],
        )
        .await;

    let caps = info(filled.call_ok(method::INFO, None).await);
    for name in [
        "window_management",
        "workspace_management",
        "virtual_desktops",
        "native_capture",
    ] {
        assert_eq!(
            capability(&caps, name),
            &agent_shell_rpc::CapabilityStatus::Enabled,
            "filled slot {name} must report enabled"
        );
    }
    for name in ["window_events", "workspace_events"] {
        assert_eq!(
            capability(&caps, name),
            &agent_shell_rpc::CapabilityStatus::Enabled,
            "enabled event slot {name} must report enabled (lazy only applies before activation)"
        );
    }
    assert_eq!(
        capability(&caps, "monitor_layout"),
        &agent_shell_rpc::CapabilityStatus::Disabled,
        "monitor layout is structurally not provided (C-034 ⑥)"
    );
    assert_eq!(
        capability(&caps, "native_input"),
        &agent_shell_rpc::CapabilityStatus::Disabled,
        "fixture compositor does not provide native input; the input chain is a separate slot"
    );
    let windows = filled.call_ok(method::WINDOWS_LIST, None).await;
    assert!(windows.get("windows").is_some());
    let workspaces = filled.call_ok(method::WORKSPACES_LIST, None).await;
    assert!(workspaces.get("workspaces").is_some());
    // 输入槽由独立组件填充：info 的 native_input 位不参与，但执行路径可用。
    filled
        .call_ok(
            method::INPUT_SEND,
            Some(json!({"kind": "key", "payload": {"combo": "a"}})),
        )
        .await;
    let out_dir = tempfile::tempdir().expect("out");
    let shot = filled
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(json!({"output_path": out_dir.path().join("x.ppm").display().to_string()})),
        )
        .await;
    assert!(shot.get("width").is_some());
    // 懒启动槽在首次订阅后建立：订阅成功即该能力被激活，不静默失败。
    filled.call_ok(method::EVENTS_SUBSCRIBE, None).await;

    // GNOME 会话独有的 Shell 扩展槽：显式状态，不静默省略（此时夹具目录尚未
    // 安装扩展，只断言槽存在且带明确状态；安装后的真值断言见下方 AC-110 段）。
    let _desktop = EnvGuard::set("XDG_CURRENT_DESKTOP", "GNOME");
    let value = filled.call_ok(method::DOCTOR, None).await;
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    let extension = report
        .lines
        .iter()
        .find(|line| !line.starts_with(' ') && line.contains("Shell 扩展"))
        .expect("GNOME session must render the extension slot");
    assert!(
        ["✓", "⚠", "✗"].iter().any(|p| extension.starts_with(p)),
        "extension slot must carry an explicit status: {extension}"
    );

    // 扩展的版本白名单边界（AC-110：GNOME 组合的合成器槽必须装配成功）。
    // metadata.json 的 shell-version 是 GNOME 的装载白名单，不在列即拒绝加载——
    // 槽随之变空、窗口命令报 1002。两侧都要可判定，且版本不符的提示本身保留。
    let versions = embedded_shell_versions();
    let running = running_shell_major().map(|m| m.to_string());
    assert!(
        versions.iter().any(|v| v == SUPPORTED_SHELL_MAJOR),
        "shell-version {versions:?} must cover the supported GNOME {SUPPORTED_SHELL_MAJOR}"
    );
    if let Some(running) = running.as_deref() {
        assert!(
            versions
                .iter()
                .any(|v| v.split('.').next() == Some(running)),
            "shell-version {versions:?} does not cover the running GNOME Shell {running}"
        );
    }

    // 生产装载判据（install 落盘 + status 提示）：覆盖的版本可加载且不报 mismatch；
    // 白名单之外的版本仍如实报 mismatch（提示保留，不静默成功）。
    let data = tempfile::tempdir().expect("extension data home");
    let _data_home = EnvGuard::set("XDG_DATA_HOME", data.path());
    let _shell = EnvGuard::set("GNOME_SHELL_VERSION", format!("{SUPPORTED_SHELL_MAJOR}.1"));
    filled.call_ok(method::EXTENSION_INSTALL, None).await;
    let status = extension_status(&mut filled).await;
    assert!(
        status.installed,
        "GNOME {SUPPORTED_SHELL_MAJOR} must load the installed extension: {status:?}"
    );
    assert!(
        !status
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("shell-version mismatch"),
        "a covered shell version must not report mismatch: {:?}",
        status.note
    );

    // AC-110 的就绪判据：安装生效后医生行必须读安装真值，不得再报「未安装」
    // （就绪能力不返回后端不可用）。⚠ installed but not enabled 与 ✓ 都接受——
    // 取决于 dconf 真值，两者都证明槽读的是安装结果而非空装配。
    //
    // 执行路径（windows.list 不返回 1002）要真实 GNOME 扩展通道才有判定面：夹具
    // 合成器是 KDE 假合成器，扩展通道不在本夹具可达范围；该面由 extension.js 的
    // 分派用例（四条授权分支）与真机验收覆盖。
    let value = filled.call_ok(method::DOCTOR, None).await;
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    let extension = report
        .lines
        .iter()
        .find(|line| !line.starts_with(' ') && line.contains("Shell 扩展"))
        .expect("GNOME session must render the extension slot");
    assert!(
        !extension.contains("not installed"),
        "installed extension must not be reported as missing: {extension}"
    );
    assert!(
        extension.starts_with('⚠') || extension.starts_with('✓'),
        "extension slot must report the installed state: {extension}"
    );

    let undeclared = format!("{}.1", highest_shell_major(&versions) + 1);
    let _undeclared = EnvGuard::set("GNOME_SHELL_VERSION", &undeclared);
    let status = extension_status(&mut filled).await;
    assert!(
        !status.installed,
        "an undeclared shell major must not be reported loadable: {status:?}"
    );
    assert!(
        status
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("shell-version mismatch"),
        "version mismatch hint must be kept for undeclared majors: {:?}",
        status.note
    );
}

/// 环境变量守卫：Drop 时恢复原值——断言 panic 时同样生效，不把指向已删除
/// tempdir 的值留在进程环境里。
struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// 当前受支持的最大 GNOME Shell 主版本：与 metadata.json 的 shell-version 同步上移。
const SUPPORTED_SHELL_MAJOR: &str = "51";

/// 内嵌 metadata.json 的 shell-version 白名单（安装与打包的唯一来源）。
fn embedded_shell_versions() -> Vec<String> {
    let meta: serde_json::Value =
        serde_json::from_str(agent_shell_compositor_mutter::EXTENSION_METADATA)
            .expect("extension metadata must be valid JSON");
    meta["shell-version"]
        .as_array()
        .expect("shell-version array")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// 白名单里的最高主版本（边界外的判据取它加一）。
fn highest_shell_major(versions: &[String]) -> u32 {
    versions
        .iter()
        .filter_map(|v| v.split('.').next()?.parse::<u32>().ok())
        .max()
        .expect("shell-version must name at least one major")
}

/// 本机安装的 GNOME Shell 主版本（与生产装载判据同源命令）；非 GNOME 环境为 None。
fn running_shell_major() -> Option<u32> {
    let out = std::process::Command::new("gnome-shell")
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    text.split_whitespace()
        .next_back()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

async fn extension_status(fixture: &mut Fixture) -> agent_shell_rpc::ExtensionStatus {
    serde_json::from_value(fixture.call_ok(method::EXTENSION_STATUS, None).await)
        .expect("ExtensionStatus")
}
