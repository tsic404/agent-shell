//! TC-capture-02（AC-41、AC-45，P1）：窗口截图（仅 X11）与窗口标识换算。
//!
//! - 场景：`screenshot --window <id>` 的标识解析与非 X11 会话行为
//! - 判据：图像仅含目标窗口；正确标识成功、错误标识报错；非 X11 明确不支持
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-02`
//!
//! 无图形 CI 没有 X server：窗口直捕的成功路径（图像仅含目标窗口）属环境
//! 受限，由 X11 会话的真机验收覆盖。本用例判定可执行面：十进制标识的四种
//! 等价写法都进入 X11 直捕并得到明确的「无 X server」不支持错误；非十进制
//! （Wayland uuid/花括号不配对/负数/十六进制/溢出）在参数层明确拒绝，绝不
//! 截错窗口也不回落到 portal。

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, Fixture, PrivateBus, ScreenshotMock};
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
}
