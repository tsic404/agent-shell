//! TC-capture-01（AC-40，P0）最小确认等待下的交互回退。
//!
//! - 场景：`wait_ms=500` 是 CLI `--wait 0` 钳制出的最小合法确认预算；静默
//!   Screenshot 立即受理请求，却要到 1s 之后才落盘（慢静默）
//! - 判据：交互级仍须发出自己的请求——交互立即成功时写出正确 PNG；确认未完成
//!   时返回可区分的授权未确认、无产物，且只有一次交互请求（不叠第二个授权界面）
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-01-gnome-min-wait`
//!
//! 判定面是真实捕获链：daemon → capture 模块 → portal Screenshot 客户端，
//! 私有 session bus 上的 mock 提供真实的 Request/Response 生命周期。两条腿
//! 共用一次进程级环境设置，故顺序执行。

use std::path::Path;
use std::time::{Duration, Instant};

use acceptance::portal::serve_screenshot_portal;
use acceptance::{late_png, sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
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

/// 一套私有总线上的 Screenshot portal 与夹具。
///
/// 总线句柄、portal 连接与落盘目录必须与夹具同寿命：任何一个提前释放都会
/// 让捕获链瞬间不可达，判定面就不再是确认等待。
struct PortalFixture {
    _bus: PrivateBus,
    _portal_dir: tempfile::TempDir,
    _portal: zbus::Connection,
    mock: ScreenshotMock,
    fixture: Fixture,
}

/// 静默级一律慢到超过自己的探测预算；交互级取 `behaviour`。
async fn slow_silent_fixture(behaviour: PortalBehaviour) -> PortalFixture {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    mock.set_behaviour(PortalBehaviour::LateSuccess(Duration::from_millis(1_500)));
    mock.set_late_png(late_png());
    mock.set_interactive_behaviour(behaviour);
    let portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;
    PortalFixture {
        _bus: bus,
        _portal_dir: portal_dir,
        _portal: portal,
        mock,
        fixture,
    }
}

#[tokio::test]
async fn minimum_wait_reaches_the_interactive_level_after_a_slow_silent_probe() {
    // 腿一：交互级立即成功。最小等待值下这张图必须产出，内容来自交互级，
    // 而不是被放弃的静默请求（两者 PNG 不同）。
    let mut success = slow_silent_fixture(PortalBehaviour::Success).await;
    let out_dir = tempfile::tempdir().expect("output temp");
    let output = out_dir.path().join("minimum-wait.png");
    let started = Instant::now();
    let result = success
        .fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, 500)),
        )
        .await;
    let elapsed = started.elapsed();
    assert_eq!(
        success.mock.interactive_calls(),
        1,
        "the minimum wait must still issue one interactive request after a slow silent probe"
    );
    assert_eq!(result.get("width"), Some(&json!(4)));
    assert_eq!(result.get("height"), Some(&json!(3)));
    assert_eq!(
        std::fs::read(&output).expect("minimum-wait capture output"),
        sample_png(),
        "the artifact must come from the interactive level, not the abandoned silent probe"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the interactive request must be issued as soon as the silent probe ends: {elapsed:?}"
    );

    // 腿二：同样的慢静默，交互级也没人确认。收场是可区分的授权未确认，
    // 无产物，且只有一次交互请求（不重试、不叠第二个授权界面）。
    let mut unconfirmed = slow_silent_fixture(PortalBehaviour::Hang).await;
    let out_dir = tempfile::tempdir().expect("output temp");
    let output = out_dir.path().join("minimum-wait-unconfirmed.png");
    let error = unconfirmed
        .fixture
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, 500)),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::Denied as i32, "{}", error.message);
    assert!(
        error.message.contains("authorization was not confirmed"),
        "the minimum wait must fail as an unconfirmed authorization: {}",
        error.message
    );
    assert!(
        !error.message.contains("unreachable or denied"),
        "the confirmation wait must not be lost to backend unavailability: {}",
        error.message
    );
    assert_eq!(
        unconfirmed.mock.interactive_calls(),
        1,
        "an unconfirmed minimum wait must issue exactly one interactive request"
    );
    assert!(
        !output.exists(),
        "an unconfirmed capture must not leave an artifact"
    );
}
