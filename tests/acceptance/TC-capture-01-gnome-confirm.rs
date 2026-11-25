//! TC-capture-01（AC-40，P0）GNOME 确认框：门户在方法返回后才等用户点击。
//!
//! - 场景：GNOME 的 portal Screenshot（含 `interactive=false`）先弹确认框，
//!   用户点击后才发 Response；方法本身立即返回
//! - 判据：静默探测保持固定的短预算并在确认未完成时退到交互级；交互级的
//!   确认在两秒后完成时，默认等待预算内必须落盘成功；确认在预算内没有完成时，
//!   失败点名「授权未确认」而不是笼统的捕获预算耗尽，且不留产物
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-01-gnome-confirm`
//!
//! 判定面是真实捕获链：daemon → capture 模块 → portal Screenshot 客户端，
//! 私有 session bus 上的 mock 提供真实的 Request/Response 生命周期。

use std::path::Path;
use std::time::{Duration, Instant};

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

fn capture_params(output: &Path, wait_ms: u64) -> serde_json::Value {
    json!({
        "window": null,
        "area": null,
        "output_path": output.display().to_string(),
        "wait_ms": wait_ms,
    })
}

/// GNOME 确认框的两条判据共用一次进程级环境设置（私有总线地址是进程级
/// 全局变量，并行用例会互相覆盖）；两个阶段在同一用例内顺序执行。
#[tokio::test]
async fn gnome_portal_confirmation_is_waited_for_and_reported_distinctly() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let png = sample_png();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), png.clone());
    mock.set_behaviour(PortalBehaviour::LateSuccess(Duration::from_secs(2)));
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    let output = out_dir.path().join("confirmed.png");
    let started = Instant::now();
    fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, 3_000)),
        )
        .await;
    let elapsed = started.elapsed();

    let written = std::fs::read(&output).expect("confirmed capture output exists");
    assert_eq!(written, png, "confirmed capture must write the portal PNG");
    assert!(
        elapsed >= Duration::from_secs(2),
        "capture must wait for the confirmation, not return early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "confirmed capture must stay within the command budget: {elapsed:?}"
    );
    assert_eq!(
        mock.calls(),
        2,
        "the fixed silent probe must be abandoned, then one interactive attempt waits"
    );
    assert_eq!(
        mock.interactive_calls(),
        1,
        "the confirmation wait belongs to the interactive fallback level"
    );

    // 第二阶段：确认没有在预算内完成时，失败必须点名授权未确认，而不是
    // 「捕获预算耗尽」。
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    mock.set_behaviour(PortalBehaviour::LateSuccess(Duration::from_secs(30)));
    let _portal = serve_screenshot_portal(&bus, mock).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    let output = out_dir.path().join("unconfirmed.png");
    let error = fixture
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, 800)),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::Denied as i32);
    assert!(
        error.message.contains("authorization was not confirmed"),
        "failure must name the unconfirmed authorization: {}",
        error.message
    );
    assert!(
        !error.message.contains("capture exceeded"),
        "failure must not be reported as a generic capture budget: {}",
        error.message
    );
    assert!(
        !output.exists(),
        "unconfirmed capture must not leave an artifact"
    );
}
