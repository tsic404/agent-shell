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

    // GNOME 会话独有的 Shell 扩展槽：显式状态，不静默省略。
    std::env::set_var("XDG_CURRENT_DESKTOP", "GNOME");
    let value = filled.call_ok(method::DOCTOR, None).await;
    std::env::remove_var("XDG_CURRENT_DESKTOP");
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
}
