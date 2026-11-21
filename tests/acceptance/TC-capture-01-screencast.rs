//! TC-capture-01（AC-40，P0）默认特性回归：ScreenCast 超时后释放门户资源，
//! 并仍走到交互截图。
//!
//! - 场景：默认特性（portal-screencast 开启）下，静默 Screenshot 与
//!   ScreenCast 的 Start 都不返回，交互 Screenshot 随后成功；同一 daemon
//!   连续执行两次捕获
//! - 判据：交互截图确实发起并产出非空 PNG；每次超时后旧 Session 被 Close、
//!   Start 授权请求被取消，连续调用不遗留远端会话或未完成请求
//! - 执行：`cargo test -p agent-shell-acceptance --features portal-screencast \
//!   --test TC-capture-01-screencast`
//!
//! 判定面是真实捕获链：静默 Screenshot 卡住、ScreenCast `Start` 不返回、
//! 交互 Screenshot 返回确定 PNG。daemon 逐字节复制并删除门户源文件。

use std::path::Path;
use std::time::Duration;

use acceptance::portal::{serve_portal, ScreenCastMock};
use acceptance::{sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_capture::{CaptureTarget, ScreenCastCapture, ScreenCastOptions};
use agent_shell_rpc::method;
use serde_json::json;

fn capture_params(output: &Path) -> serde_json::Value {
    json!({"window": null, "area": null, "output_path": output.display().to_string()})
}

fn file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .count()
}

#[tokio::test]
async fn screencast_timeout_releases_session_and_still_reaches_interactive_screenshot() {
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

    for round in 1..=2 {
        let output = out_dir
            .path()
            .join(format!("screencast-fallback-{round}.png"));
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
            round,
            "each capture must create one portal session attempt"
        );
        assert_eq!(
            screencast.session_closes(),
            round,
            "the timed-out session must be closed on the portal"
        );
        assert_eq!(
            screencast.open_sessions(),
            0,
            "no portal session may outlive the ScreenCast sub-budget"
        );
        assert_eq!(
            screencast.requests_cancelled(),
            round,
            "the pending Start authorization request must be cancelled"
        );
        assert_eq!(
            screencast.pending_requests(),
            0,
            "no unfinished authorization request may be left behind"
        );
        assert_eq!(
            screencast.start_calls(),
            round,
            "ScreenCast Start must be attempted once per capture"
        );
        assert_eq!(
            screenshot.interactive_calls(),
            round,
            "interactive Screenshot must run after each ScreenCast sub-budget expires"
        );
        assert_eq!(
            file_count(portal_dir.path()),
            0,
            "portal source must be removed after a successful copy"
        );
    }
    assert_eq!(
        screencast.calls(),
        6,
        "two captures must each run CreateSession, SelectSources and Start"
    );
}

#[tokio::test]
async fn dropping_established_capture_closes_session_within_cleanup_budget() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let bus = PrivateBus::start();
    let screenshot = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let screencast = ScreenCastMock::new();
    screencast.respond_to_start();
    let _portal = serve_portal(&bus, screenshot, screencast.clone()).await;
    let conn = bus.connect().await;

    let session = ScreenCastCapture::start_with_options(
        conn,
        CaptureTarget::Monitor,
        &ScreenCastOptions::default(),
    )
    .await
    .expect("ScreenCast session must establish");
    assert_eq!(screencast.sessions_created(), 1);
    assert_eq!(screencast.open_sessions(), 1);

    let drop_started = std::time::Instant::now();
    drop(session);
    let drop_elapsed = drop_started.elapsed();
    assert!(
        drop_elapsed < Duration::from_secs(1),
        "Drop must bound the worker wait; blocked for {drop_elapsed:?}"
    );

    // Session.Close 由 Drop 在运行时上补发；有界等待清理完成。
    let cleanup_started = std::time::Instant::now();
    while screencast.session_closes() == 0 && cleanup_started.elapsed() < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        screencast.session_closes(),
        1,
        "dropping an established capture must close the portal session"
    );
    assert_eq!(screencast.open_sessions(), 0);
}
