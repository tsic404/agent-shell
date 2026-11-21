//! TC-capture-01（AC-40，P0）默认特性回归：显式关闭、worker 退出与 Drop
//! 补发共享同一个 200ms 清理截止。
//!
//! - 场景：已建 ScreenCast 会话无帧；Session.Close 回复延迟；worker 未退出
//! - 判据：关闭/退出整体落在 `SCREENCAST_CLEANUP_BUDGET` 内，随后照常降级
//!   到交互截图；Drop 不再二次计时或补发 Close
//! - 执行：`cargo test -p agent-shell-acceptance --features portal-test-hooks \
//!   --test TC-capture-01-screencast-cleanup`

use std::path::Path;
use std::time::Duration;

use acceptance::portal::{serve_portal, ScreenCastMock};
use acceptance::{sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_capture::portal_common::test_hooks;
use agent_shell_capture::{CaptureTarget, ScreenCastCapture, SCREENCAST_CLEANUP_BUDGET};
use agent_shell_rpc::method;
use serde_json::json;

/// 钩子是进程级全局状态；同一目标的用例串行执行。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct ClearHooks;

impl Drop for ClearHooks {
    fn drop(&mut self) {
        test_hooks::clear_subscribe_delay();
        test_hooks::clear_worker_start_delay();
        test_hooks::clear_frame_wait_timeout();
    }
}

fn capture_params(output: &Path) -> serde_json::Value {
    json!({"window": null, "area": null, "output_path": output.display().to_string()})
}

#[tokio::test]
async fn shutdown_deadline_bounds_close_and_worker_without_drop_resend() {
    let _serial = SERIAL.lock().await;
    let _clear = ClearHooks;
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let bus = PrivateBus::start();
    let screenshot = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let screencast = ScreenCastMock::new();
    screencast.respond_to_start();
    screencast.set_close_delay(Duration::from_millis(400));
    test_hooks::set_worker_start_delay(Duration::from_millis(600));
    let _portal = serve_portal(&bus, screenshot, screencast.clone()).await;
    let conn = bus.connect().await;

    let session = ScreenCastCapture::start(conn, CaptureTarget::Monitor)
        .await
        .expect("ScreenCast session must establish");
    assert_eq!(screencast.open_sessions(), 1);

    let started = tokio::time::Instant::now();
    session
        .shutdown_until(started + SCREENCAST_CLEANUP_BUDGET)
        .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed <= SCREENCAST_CLEANUP_BUDGET + Duration::from_millis(100),
        "close and worker wait must share the 200ms deadline; took {elapsed:?}"
    );
    assert_eq!(
        screencast.session_close_attempts(),
        1,
        "explicit shutdown must attempt Session.Close"
    );

    drop(session);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        screencast.session_close_attempts(),
        1,
        "Drop must not resend Close after the shared cleanup deadline"
    );
}

#[tokio::test]
async fn established_session_without_frames_degrades_within_shared_cleanup_budget() {
    let _serial = SERIAL.lock().await;
    let _clear = ClearHooks;
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let png = sample_png();
    let screenshot = ScreenshotMock::new(portal_dir.path().to_path_buf(), png.clone());
    screenshot.set_behaviour(PortalBehaviour::Hang);
    screenshot.set_interactive_behaviour(PortalBehaviour::Success);
    let screencast = ScreenCastMock::new();
    screencast.respond_to_start();
    screencast.set_close_delay(Duration::from_millis(400));
    test_hooks::set_worker_start_delay(Duration::from_millis(600));
    test_hooks::set_frame_wait_timeout(Duration::from_millis(100));
    let _portal = serve_portal(&bus, screenshot.clone(), screencast.clone()).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    let output = out_dir.path().join("no-frame-cleanup.png");
    let result = fixture
        .call_ok(method::SCREENSHOT_CAPTURE, Some(capture_params(&output)))
        .await;
    assert_eq!(result.get("width"), Some(&json!(4)));
    assert_eq!(result.get("height"), Some(&json!(3)));
    let written = std::fs::read(&output).expect("capture output exists");
    assert_eq!(written, png, "interactive portal PNG must be copied");
    assert!(!written.is_empty());

    assert_eq!(screencast.sessions_created(), 1);
    assert_eq!(screencast.session_close_attempts(), 1);
    assert_eq!(
        screenshot.interactive_calls(),
        1,
        "capture must degrade to interactive Screenshot after cleanup"
    );

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        screencast.session_close_attempts(),
        1,
        "Drop must not add a second cleanup attempt"
    );
    assert_eq!(screencast.open_sessions(), 0);
}
