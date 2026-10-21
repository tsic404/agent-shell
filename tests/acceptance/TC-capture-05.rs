//! TC-capture-05（AC-44，P1）：Wayland 链、X11 链与无可用后端。
//!
//! - 场景：各会话走对应通道；无可用后端时明确报错不静默失败
//! - 判据：Wayland 下拒绝 X11 兜底；原生 X11 走 X11；无后端时报错
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-05`
//!
//! 可判定分支：Wayland 授权被拒 → 权限错误并说明拒绝抓屏（不静默抓 XWayland
//! root）；无 portal 且无 X11 → 组件缺席的后端不可用；原生非 Wayland 且无 X11
//! 兜底 → 明确「无可用后端」。X11 链的抓帧需要真实 X server（无图形 CI 不可
//! 建立执行面），由原生 X11 会话的真机验收覆盖。

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_rpc::{method, DoctorResult, RpcErrorCode};
use serde_json::json;

fn params(output: &std::path::Path) -> serde_json::Value {
    json!({"window": null, "area": null, "output_path": output.display().to_string()})
}

#[tokio::test]
async fn capture_chain_selects_session_path_and_fails_loudly_without_backend() {
    let out_dir = tempfile::tempdir().expect("output temp");

    // 1) Wayland：portal 拒绝 → 权限错误，明确拒绝 X11 兜底。
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    mock.set_behaviour(PortalBehaviour::Cancel);
    mock.set_interactive_behaviour(PortalBehaviour::Cancel);
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let mut wayland = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;
    let output = out_dir.path().join("wayland.png");
    let error = wayland
        .call_err(method::SCREENSHOT_CAPTURE, Some(params(&output)))
        .await;
    assert_eq!(error.code, RpcErrorCode::Denied as i32);
    assert!(
        error.message.contains("Wayland")
            && error.message.contains("refusing to capture the screen"),
        "Wayland denial must refuse the X11 fallback explicitly: {}",
        error.message
    );
    assert!(!output.exists());

    // 2) 无任何后端：组件缺席 → 后端不可用；doctor 行明确不可用。
    let mut bare = Fixture::start().await;
    let error = bare
        .call_err(method::SCREENSHOT_CAPTURE, Some(params(&output)))
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
    assert!(
        error
            .message
            .contains("capture unavailable (no portal backend and no DISPLAY)"),
        "missing component must be explicit: {}",
        error.message
    );
    let report: DoctorResult =
        serde_json::from_value(bare.call_ok(method::DOCTOR, None).await).expect("DoctorResult");
    let row = report
        .lines
        .iter()
        .find(|line| !line.starts_with(' ') && line.contains("截图捕获"))
        .expect("capture row");
    assert!(
        row.starts_with("✗"),
        "doctor must report the missing backend: {row}"
    );

    // 3) 原生非 Wayland、portal 存在但拒绝、无 X11：明确无可用后端。
    let mut native =
        Fixture::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string())]).await;
    let output = out_dir.path().join("native.png");
    let error = native
        .call_err(method::SCREENSHOT_CAPTURE, Some(params(&output)))
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
    assert!(
        error.message.contains(
            "no capture backend available (portal unavailable/denied, no native X11 session)"
        ),
        "no-backend decision must be explicit: {}",
        error.message
    );
    assert!(!output.exists());

    // 4) X11 链：无 X server 时同样明确失败，不静默产出空图。
    //    真实 X11 抓帧由原生会话验收覆盖（环境受限，见文件头）。
    let mut x11_absent = Fixture::start_with_env(&[
        ("DISPLAY", ":99".to_string()),
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
    ])
    .await;
    let output = out_dir.path().join("x11.png");
    let error = x11_absent
        .call_err(method::SCREENSHOT_CAPTURE, Some(params(&output)))
        .await;
    assert!(
        error.code == RpcErrorCode::BackendUnavailable as i32
            || error.code == RpcErrorCode::BackendError as i32,
        "missing X server must fail loudly, got {}: {}",
        error.code,
        error.message
    );
    assert!(!output.exists());
}
