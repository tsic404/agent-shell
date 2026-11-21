//! TC-capture-01（AC-40，P0）回归：静默探测放弃后 portal 仍写出 PNG 的竞态。
//!
//! - 场景：静默 Screenshot 立即返回 Request 路径，但在 1s 探测预算之后才
//!   写出 PNG 并发出成功 Response；交互 Screenshot 随后成功
//! - 判据：本次捕获返回交互产物（不是晚到图）；晚到的门户源文件被清理，
//!   目录不残留未跟踪文件；被放弃的 Request 收到有界 Close
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-01-screenshot-late`
//!   （`--no-default-features` 同样覆盖）
//!
//! 判定面是真实捕获链：daemon → capture 模块 → portal Screenshot 客户端，
//! 私有 session bus 上的 mock 提供真实的 Request/Response 生命周期。

use std::path::Path;
use std::time::{Duration, Instant};

use acceptance::portal::serve_screenshot_portal;
use acceptance::{late_png, sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
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
async fn abandoned_silent_probe_cleans_late_portal_file() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let interactive_png = sample_png();
    let late = late_png();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), interactive_png.clone());
    mock.set_late_png(late.clone());
    // 1s 静默预算到期后 portal 才落盘：正是本次缩短预算放开的窗口。
    mock.set_behaviour(PortalBehaviour::LateSuccess(Duration::from_millis(1500)));
    mock.set_interactive_behaviour(PortalBehaviour::Success);
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    let output = out_dir.path().join("late-shot.png");
    let started = Instant::now();
    let result = fixture
        .call_ok(method::SCREENSHOT_CAPTURE, Some(capture_params(&output)))
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "capture must stay within the command budget: {:?}",
        started.elapsed()
    );

    // 晚到响应不得改变本次返回值：尺寸与字节都来自交互授权产物。
    assert_eq!(result.get("width"), Some(&json!(4)));
    assert_eq!(result.get("height"), Some(&json!(3)));
    let written = std::fs::read(&output).expect("capture output exists");
    assert_eq!(
        written, interactive_png,
        "the interactive portal image must be the capture result"
    );
    assert_ne!(written, late, "the late silent image must not be returned");
    assert_eq!(
        mock.calls(),
        2,
        "one abandoned silent attempt followed by one interactive attempt"
    );
    assert_eq!(
        mock.interactive_calls(),
        1,
        "the fallback must be the interactive Screenshot path"
    );

    // 先确认晚到图确实写过（否则清理断言没有区分力），再等清理完成。
    let deadline = Instant::now() + Duration::from_secs(6);
    while mock.late_writes() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        mock.late_writes() >= 1,
        "the portal must have produced the late silent image"
    );
    while file_count(portal_dir.path()) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        file_count(portal_dir.path()),
        0,
        "an abandoned silent probe must not leave an untracked portal file"
    );
    assert!(
        mock.request_close_attempts() >= 1,
        "the abandoned portal Request must be closed within the cleanup budget"
    );
}
