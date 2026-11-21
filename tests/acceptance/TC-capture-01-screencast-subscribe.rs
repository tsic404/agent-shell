//! TC-capture-01（AC-40，P0）默认特性回归：订阅注册被延迟时，ScreenCast
//! 仍在固定子预算内释放已建资源，交互截图照常落盘。
//!
//! - 场景：静默 Screenshot 卡住；CreateSession / SelectSources 成功；
//!   Start 的 Response 订阅注册（总线 AddMatch）被故意延迟 2.5s；
//!   交互 Screenshot 随后成功
//! - 判据：ScreenCast 在 800ms 工作截止后取消订阅、关闭已建 Session；
//!   Start 未被发送；交互截图仍产出非空 PNG
//! - 执行：`cargo test -p agent-shell-acceptance --features portal-test-hooks \
//!   --test TC-capture-01-screencast-subscribe`

use std::path::Path;
use std::time::Duration;

use acceptance::portal::{serve_portal, ScreenCastMock, ScreenCastStep};
use acceptance::{sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_capture::portal_common::test_hooks;
use agent_shell_rpc::method;
use serde_json::json;

/// 钩子是进程级全局状态；同一目标的用例串行执行。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn capture_params(output: &Path) -> serde_json::Value {
    json!({"window": null, "area": null, "output_path": output.display().to_string()})
}

fn file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .count()
}

/// 测试结束（含 panic）恢复全局钩子，避免影响同进程后续用例。
struct ClearHooks;

impl Drop for ClearHooks {
    fn drop(&mut self) {
        test_hooks::clear_subscribe_delay();
        test_hooks::clear_worker_start_delay();
        test_hooks::clear_frame_wait_timeout();
    }
}

#[tokio::test]
async fn delayed_subscribe_registration_still_degrades_within_screencast_budget() {
    let _serial = SERIAL.lock().await;
    let _clear = ClearHooks;
    // 只延迟 Start 的订阅注册：CreateSession / SelectSources 已成功，
    // 真实 portal 会话存在；旧实现会等满 2.5s 并耗尽 portal 段。
    test_hooks::set_subscribe_delay("agent_shell_sc_start_", Duration::from_millis(2500));

    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let png = sample_png();
    let screenshot = ScreenshotMock::new(portal_dir.path().to_path_buf(), png.clone());
    screenshot.set_behaviour(PortalBehaviour::Hang);
    screenshot.set_interactive_behaviour(PortalBehaviour::Success);
    let screencast = ScreenCastMock::new();
    let _portal = serve_portal(&bus, screenshot.clone(), screencast.clone()).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    let output = out_dir.path().join("delayed-subscribe.png");
    let result = fixture
        .call_ok(method::SCREENSHOT_CAPTURE, Some(capture_params(&output)))
        .await;
    assert_eq!(result.get("width"), Some(&json!(4)));
    assert_eq!(result.get("height"), Some(&json!(3)));
    let written = std::fs::read(&output).expect("capture output exists");
    assert_eq!(written, png, "interactive portal PNG must be copied");
    assert!(!written.is_empty());

    assert_eq!(
        screencast.sessions_created(),
        1,
        "CreateSession must have succeeded before the delayed subscription"
    );
    assert_eq!(
        screencast.start_calls(),
        0,
        "Start must not be sent while its subscription is still delayed"
    );
    assert_eq!(
        screencast.session_closes(),
        1,
        "the created session must be closed within the ScreenCast budget"
    );
    assert_eq!(screencast.open_sessions(), 0);
    assert_eq!(screencast.pending_requests(), 0);
    assert_eq!(
        screenshot.interactive_calls(),
        1,
        "interactive Screenshot must still run after the subscription deadline"
    );
    assert_eq!(file_count(portal_dir.path()), 0);
}

#[tokio::test]
async fn mismatched_request_path_with_delayed_subscription_still_degrades() {
    let _serial = SERIAL.lock().await;
    let _clear = ClearHooks;
    // 兼容路径在方法返回后才订阅非预期 Request 路径；慢总线不得把它
    // 拖过 ScreenCast 的工作截止。
    test_hooks::set_subscribe_delay("_unexpected", Duration::from_millis(2500));

    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let png = sample_png();
    let screenshot = ScreenshotMock::new(portal_dir.path().to_path_buf(), png.clone());
    screenshot.set_behaviour(PortalBehaviour::Hang);
    screenshot.set_interactive_behaviour(PortalBehaviour::Success);
    let screencast = ScreenCastMock::new();
    screencast.set_request_path_mismatch(ScreenCastStep::Start);
    let _portal = serve_portal(&bus, screenshot.clone(), screencast.clone()).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    let output = out_dir.path().join("mismatch-path.png");
    let result = fixture
        .call_ok(method::SCREENSHOT_CAPTURE, Some(capture_params(&output)))
        .await;
    assert_eq!(result.get("width"), Some(&json!(4)));
    assert_eq!(result.get("height"), Some(&json!(3)));
    let written = std::fs::read(&output).expect("capture output exists");
    assert_eq!(written, png, "interactive portal PNG must be copied");
    assert!(!written.is_empty());

    assert_eq!(screencast.sessions_created(), 1);
    assert_eq!(screencast.start_calls(), 1);
    assert_eq!(
        screencast.requests_cancelled(),
        1,
        "the unexpected Start request must be cancelled within the work deadline"
    );
    assert_eq!(screencast.session_closes(), 1);
    assert_eq!(screencast.open_sessions(), 0);
    assert_eq!(screencast.pending_requests(), 0);
    assert_eq!(
        screenshot.interactive_calls(),
        1,
        "interactive Screenshot must run after the subscription deadline"
    );
    assert_eq!(file_count(portal_dir.path()), 0);
}
