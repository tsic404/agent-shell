//! TC-capture-01（AC-40，P0）GNOME 确认框阻塞方法调用时的等待语义。
//!
//! - 场景：GNOME 的 portal Screenshot 方法在用户点击 Allow 前不返回；
//!   固定的方法调用上限到期后，交互级继续等同一条 Request 的成功 Response
//! - 判据：方法上限不放大单次调用，但也不能撤下已弹出的确认；用户点 Allow
//!   后在 `--wait` 预算内产出 PNG；一直没人点则跑完预算并报授权未确认
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-01-gnome-allow`
//!
//! 判定面是真实捕获链：daemon → capture 模块 → portal Screenshot 客户端，
//! 私有 session bus 上的 mock 提供真实的 Request/Response 生命周期。
//!
//! 夹具的环境覆盖是进程级的，各腿共用一次进程设置，故顺序执行。

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

/// 起一套 portal 夹具；静默级一律被拒（快速落到交互级），交互级取 `behaviour`。
async fn portal_fixture(behaviour: PortalBehaviour) -> PortalFixture {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    mock.set_behaviour(PortalBehaviour::Cancel);
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

/// 未确认路径的公共判据：可区分的授权未确认、无产物、耗时落在预算量级。
async fn assert_unconfirmed(f: &mut PortalFixture, wait_ms: u64, min: Duration, max: Duration) {
    let out_dir = tempfile::tempdir().expect("output temp");
    let output = out_dir.path().join(format!("wait-{wait_ms}.png"));
    let started = Instant::now();
    let error = f
        .fixture
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, wait_ms)),
        )
        .await;
    let elapsed = started.elapsed();

    assert_eq!(error.code, RpcErrorCode::Denied as i32, "{}", error.message);
    assert!(
        error.message.contains("authorization was not confirmed"),
        "unconfirmed wait must name the authorization: {}",
        error.message
    );
    assert!(
        !error.message.contains("unreachable or denied"),
        "the confirmation wait must not degrade into backend unavailability: {}",
        error.message
    );
    assert_eq!(f.mock.interactive_calls(), 1);
    assert_eq!(
        f.mock.calls(),
        2,
        "a bounded wait must not issue a second authorization request"
    );
    assert!(
        elapsed >= min && elapsed < max,
        "a {wait_ms}ms budget must expire in its own order of magnitude: {elapsed:?}"
    );
    assert!(
        !output.exists(),
        "an unconfirmed capture must not leave an artifact"
    );
}

#[tokio::test]
async fn confirmation_budget_governs_the_wait_past_the_fixed_method_call_cap() {
    // 腿一：默认入口的短预算——`--wait 1000`，方法上限与整级截止同时到期。
    // 请求已经发出、门户仍在等确认，收场必须是可区分的授权未确认。
    let mut short = portal_fixture(PortalBehaviour::Hang).await;
    assert_unconfirmed(
        &mut short,
        1_000,
        Duration::from_millis(900),
        Duration::from_secs(2),
    )
    .await;

    // 腿二：默认值 `--wait 3000` 与 3s 方法上限相等，同样在方法等待里耗尽
    // 整级预算——这是最常用的调用，判据与上一腿相同。
    let mut default = portal_fixture(PortalBehaviour::Hang).await;
    assert_unconfirmed(
        &mut default,
        3_000,
        Duration::from_millis(2_800),
        Duration::from_millis(4_500),
    )
    .await;

    // 腿三：预算高于方法上限时，`--wait` 继续兑现到预算耗尽，而不是停在 3s。
    let mut above_cap = portal_fixture(PortalBehaviour::Hang).await;
    assert_unconfirmed(
        &mut above_cap,
        4_500,
        Duration::from_millis(4_200),
        Duration::from_secs(6),
    )
    .await;

    // 腿四：同一形态把预算再拉长一档，耗时跟着预算走——证明等待由配置预算
    // 派生，而不是某个固定常数。
    let mut longer = portal_fixture(PortalBehaviour::Hang).await;
    assert_unconfirmed(
        &mut longer,
        7_000,
        Duration::from_millis(6_600),
        Duration::from_secs(9),
    )
    .await;

    // 腿五：用户在方法调用上限之后点 Allow，捕获必须落盘，且在预算内。
    let mut delayed = portal_fixture(PortalBehaviour::ConfirmAfter(Duration::from_secs(5))).await;
    let out_dir = tempfile::tempdir().expect("output temp");
    let output = out_dir.path().join("delayed-allow.png");
    let started = Instant::now();
    let result = delayed
        .fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, 8_000)),
        )
        .await;
    let elapsed = started.elapsed();
    assert_eq!(delayed.mock.interactive_calls(), 1);
    assert_eq!(result.get("width"), Some(&json!(4)));
    assert_eq!(result.get("height"), Some(&json!(3)));
    assert_eq!(
        std::fs::read(&output).expect("Allow capture output exists"),
        sample_png(),
        "the delayed Allow must still produce the portal PNG"
    );
    assert!(
        elapsed >= Duration::from_secs(5),
        "capture must wait past the fixed method call cap for the delayed Allow: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the delayed Allow must stay inside the --wait budget: {elapsed:?}"
    );
    assert_eq!(
        delayed.mock.calls(),
        2,
        "one abandoned silent probe followed by one interactive attempt"
    );
    assert_eq!(
        delayed.mock.late_responses(),
        1,
        "the artifact must come from the confirmed Response on the interactive request"
    );
}
