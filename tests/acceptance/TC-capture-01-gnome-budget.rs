//! TC-capture-01（AC-40，P0）确认等待的三条预算边界。
//!
//! - 场景一：`--wait` 很长而交互 Screenshot 方法本身挂住
//! - 场景二：静默级的 Response 订阅注册慢于该级 1 秒预算
//! - 场景三：已有可出图的 ScreenCast 会话，而 Screenshot 仍要求确认
//! - 判据：方法调用仍受固定上限约束；静默级整级截止覆盖订阅建立到 Response；
//!   待确认时只禁止新发授权，已有会话照常出图且不新增授权请求
//! - 执行：`cargo test -p agent-shell-acceptance --features portal-test-hooks \
//!   --test TC-capture-01-gnome-budget`
//!
//! 判定面是真实捕获链：daemon → capture 模块 → portal 客户端，私有总线上的
//! mock 提供真实 Request/Response 生命周期；PipeWire 帧由测试钩子注入。

use std::path::Path;
use std::time::{Duration, Instant};

use acceptance::portal::{serve_portal, serve_screenshot_portal, ScreenCastMock};
use acceptance::{sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_capture::portal_common::test_hooks;
use agent_shell_capture::{Frame, PixelFormat};
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

fn synthetic_frame() -> Frame {
    let width = 6;
    let height = 5;
    Frame {
        data: vec![200; (width * height * 4) as usize],
        width,
        height,
        stride: (width * 4) as usize,
        format: PixelFormat::Rgba,
    }
}

/// 钩子是进程级全局状态；离开用例（含 panic）时恢复。
struct ClearHooks;

impl Drop for ClearHooks {
    fn drop(&mut self) {
        test_hooks::clear_subscribe_delay();
        test_hooks::clear_synthetic_frame();
    }
}

/// 三个边界共用一次进程级环境设置；各阶段顺序执行。
#[tokio::test]
async fn confirmation_wait_keeps_method_call_and_level_deadlines_fixed() {
    let _clear = ClearHooks;

    // 场景一：--wait 6 秒 + 交互方法挂住。方法调用仍受固定的 3 秒上限，
    // 但请求已受理，`--wait` 的剩余预算必须继续等同一条确认；失败不再是
    // 「后端不可用」，而是可区分的授权未确认。
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    mock.set_behaviour(PortalBehaviour::Cancel);
    mock.set_interactive_behaviour(PortalBehaviour::Hang);
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    let output = out_dir.path().join("long-wait.png");
    let started = Instant::now();
    let error = fixture
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, 6_000)),
        )
        .await;
    let elapsed = started.elapsed();
    assert_eq!(mock.interactive_calls(), 1);
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(8),
        "interactive wait must use the configured confirmation budget after the fixed method cap: {elapsed:?}"
    );
    assert_eq!(error.code, RpcErrorCode::Denied as i32);
    assert!(
        error.message.contains("authorization was not confirmed"),
        "bounded failure must name the unconfirmed authorization: {}",
        error.message
    );
    assert!(!output.exists());

    // 场景二：静默级的订阅注册慢于该级 1 秒预算。静默级必须在 1 秒内结束
    // （连方法调用都发不出去），把确认预算留给交互级。
    test_hooks::set_subscribe_delay("agent_shell_screenshot_", Duration::from_millis(1200));
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let png = sample_png();
    let slow = ScreenshotMock::new(portal_dir.path().to_path_buf(), png.clone());
    slow.set_behaviour(PortalBehaviour::Hang);
    slow.set_interactive_behaviour(PortalBehaviour::Success);
    let _portal = serve_screenshot_portal(&bus, slow.clone()).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    let output = out_dir.path().join("slow-subscribe.png");
    let started = Instant::now();
    let result = fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, 3_000)),
        )
        .await;
    let elapsed = started.elapsed();
    test_hooks::clear_subscribe_delay();
    assert_eq!(result.get("width"), Some(&json!(4)));
    assert_eq!(result.get("height"), Some(&json!(3)));
    assert_eq!(
        std::fs::read(&output).expect("interactive capture output"),
        png,
        "the interactive level must still produce the portal PNG"
    );
    assert_eq!(
        slow.calls(),
        1,
        "the silent level must end inside its 1s budget, before sending its method call"
    );
    assert_eq!(
        slow.interactive_calls(),
        1,
        "the slow silent probe must not consume the interactive confirmation budget"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the slow subscription must not stack onto the confirmation wait: {elapsed:?}"
    );

    // 场景三：已有能出图的 ScreenCast 会话，而 Screenshot 仍要求确认。
    // 复用暖会话直接出图，不新增授权请求，也不退化成再次确认。
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let screenshot = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    screenshot.set_behaviour(PortalBehaviour::Cancel);
    screenshot.set_interactive_behaviour(PortalBehaviour::Hang);
    let screencast = ScreenCastMock::new();
    screencast.respond_to_start();
    let _portal = serve_portal(&bus, screenshot.clone(), screencast.clone()).await;
    test_hooks::set_synthetic_frame(synthetic_frame());
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;

    // 暖机：静默截图拒绝 → ScreenCast 建会话 → 直接取帧。
    let warm = out_dir.path().join("warm.png");
    let result = fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&warm, 3_000)),
        )
        .await;
    assert_eq!(result.get("width"), Some(&json!(6)));
    assert_eq!(result.get("height"), Some(&json!(5)));
    assert_eq!(screencast.sessions_created(), 1);
    assert_eq!(screencast.start_calls(), 1);
    assert_eq!(screenshot.interactive_calls(), 0);

    // 冷确认：方法已受理但 Response 不来；已有会话必须被复用。
    screenshot.set_behaviour(PortalBehaviour::LateSuccess(Duration::from_secs(30)));
    let output = out_dir.path().join("warm-reuse.png");
    let started = Instant::now();
    let result = fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&output, 3_000)),
        )
        .await;
    let elapsed = started.elapsed();
    assert_eq!(result.get("width"), Some(&json!(6)));
    assert_eq!(result.get("height"), Some(&json!(5)));
    assert!(
        elapsed < Duration::from_secs(3),
        "the warm session must answer before the confirmation budget: {elapsed:?}"
    );
    assert_eq!(
        screencast.sessions_created(),
        1,
        "an established session must be reused instead of creating another"
    );
    assert_eq!(
        screencast.start_calls(),
        1,
        "an established session must not issue a second authorization request"
    );
    assert_eq!(
        screenshot.interactive_calls(),
        0,
        "a warm session must not force the user through Screenshot confirmation"
    );
    assert_eq!(screencast.pending_requests(), 0);
}
