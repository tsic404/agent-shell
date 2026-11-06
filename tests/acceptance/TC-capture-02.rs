//! TC-capture-02（AC-41、AC-45，P1）：窗口截图（仅 X11）与窗口标识换算。
//!
//! - 场景：`screenshot --window <id>` 的标识解析、原生 X11 会话行为与
//!   Wayland 会话拒绝
//! - 判据：图像仅含目标窗口；正确标识成功、错误标识报错；非 X11 明确不支持
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-02`
//!
//! 无图形 CI 没有 X server：窗口直捕的成功路径（图像仅含目标窗口）属环境
//! 受限，由 X11 会话的真机验收覆盖。本用例判定可执行面：原生 X11 会话的
//! 十进制标识四种等价写法都进入 X11 直捕并得到明确的「无 X server」不支持
//! 错误；Wayland 会话（`WAYLAND_DISPLAY` 存在）的窗口标识——包括
//! `windows list` 给出的十进制 native id——归为「无效窗口标识」，不装配 X11
//! 直捕也不回落到 portal；非十进制（Wayland uuid/花括号不配对/负数/十六进制/
//! 溢出）在参数层明确拒绝，绝不截错窗口也不回落到 portal。

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, window, FakeCompositor, Fixture, PrivateBus, ScreenshotMock};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

fn window_params(target: &str, output: &std::path::Path) -> serde_json::Value {
    json!({
        "window": target,
        "area": null,
        "output_path": output.display().to_string(),
    })
}

#[tokio::test]
async fn window_capture_parses_decimal_ids_and_rejects_non_x11_ids() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let mut fixture =
        Fixture::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string())]).await;

    // 合法十进制标识的等价写法：都进入 X11 直捕；无 X server → 明确不支持。
    for (spec, expected_id) in [("42", 42u32), ("{42}", 42), ("id:42", 42), ("id:{42}", 42)] {
        let calls_before = mock.calls();
        let output = out_dir.path().join(format!("win-{expected_id}.ppm"));
        let error = fixture
            .call_err(
                method::SCREENSHOT_CAPTURE,
                Some(window_params(spec, &output)),
            )
            .await;
        assert_eq!(
            error.code,
            RpcErrorCode::BackendUnavailable as i32,
            "{spec:?} must reach the X11 window path"
        );
        assert!(
            error.message.contains("x11 capture unavailable")
                && error.message.contains("no X server reachable"),
            "non-X11 session must report explicit unsupported: {spec} -> {}",
            error.message
        );
        assert!(!output.exists());
        assert_eq!(
            mock.calls(),
            calls_before,
            "window capture must not fall back to the portal"
        );
    }

    // 非十进制标识：参数层明确拒绝（X11-only 说明），不截错窗口。
    for spec in [
        "{593a-uuid}",
        "{42",
        "42}",
        "-7",
        "0x2a",
        "99999999999999999999",
    ] {
        let output = out_dir.path().join("bad.ppm");
        let error = fixture
            .call_err(
                method::SCREENSHOT_CAPTURE,
                Some(window_params(spec, &output)),
            )
            .await;
        assert_eq!(
            error.code,
            RpcErrorCode::InvalidParams as i32,
            "{spec:?} must be InvalidParams"
        );
        assert!(
            error.message.contains("X11-only"),
            "rejection must explain the X11-only window id rule: {spec} -> {}",
            error.message
        );
        assert!(!output.exists());
    }

    // Wayland 会话：窗口标识没有 X11 语义。取 `windows list` 给出的十进制
    // native id，必须以「无效窗口标识」拒绝；不装配 X11 直捕（失效 DISPLAY
    // 下若落到 X11 路径，错误会是 X11 不可达），也不回落到 portal。
    let wayland_portal_dir = tempfile::tempdir().expect("portal temp");
    let wayland_bus = PrivateBus::start();
    let wayland_mock = ScreenshotMock::new(wayland_portal_dir.path().to_path_buf(), sample_png());
    let _wayland_portal = serve_screenshot_portal(&wayland_bus, wayland_mock.clone()).await;
    let mut wayland = Fixture::start_with_env(&[
        (
            "DBUS_SESSION_BUS_ADDRESS",
            wayland_bus.address().to_string(),
        ),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
        ("DISPLAY", ":99".to_string()),
    ])
    .await;
    assert!(
        wayland.daemon().capture.is_some(),
        "fixture must assemble the capture chain to exercise the Wayland window gate"
    );
    let wayland_compositor = FakeCompositor::with_windows(vec![window(
        "1211664006",
        "Text Editor",
        "org.gnome.TextEditor",
        0,
        0,
        800,
        600,
    )]);
    wayland.install_compositor(&wayland_compositor, DesktopEnvironment::GNOME);
    let listed = wayland.call_ok(method::WINDOWS_LIST, None).await;
    let entry = listed["windows"]
        .as_array()
        .expect("windows array")
        .iter()
        .find(|w| w["native_id"] == json!("1211664006"))
        .expect("window listed by windows list");
    let native_id = entry["native_id"]
        .as_str()
        .expect("native id string")
        .to_string();
    let output = out_dir.path().join("wayland-window.ppm");
    let calls_before = wayland_mock.calls();
    let error = wayland
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(window_params(&native_id, &output)),
        )
        .await;
    assert_eq!(
        error.code,
        RpcErrorCode::InvalidParams as i32,
        "Wayland window ids must be rejected as invalid, got {}: {}",
        error.code,
        error.message
    );
    assert!(
        error.message.contains("invalid window id") && error.message.contains("Wayland"),
        "Wayland must report the identifier itself as invalid: {}",
        error.message
    );
    assert!(
        !error.message.contains("x11 capture unavailable"),
        "Wayland must not route the request to X11 capture: {}",
        error.message
    );
    assert!(!output.exists());
    assert_eq!(
        wayland_mock.calls(),
        calls_before,
        "Wayland window capture must not fall back to the portal"
    );
}
