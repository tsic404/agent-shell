//! TC-doctor-04（AC-04，P0）：截图捕获行绑定态、未绑定态与括注文案。
//!
//! - 场景：绑定态 / 未绑定态 / 四种未就绪括注
//! - 判据：两种状态形态正确；四种括注逐字一致
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-doctor-04`
//!
//! 可判定分支（逐字断言）：
//! - 绑定：私有 bus 上的 Screenshot portal 返回有效非黑 PNG →
//!   `✓ ... （选中 portal-screenshot）`；
//! - 纯 Wayland 未授权：`⚠ ... （Wayland 下 portal 未就绪/未授权，无可用后端）`；
//! - 原生非 Wayland 未授权：`⚠ ... （非交互探测未就绪，需 portal 交互授权）`；
//! - 无任何后端：`✗ ... 不可用（无 portal 且无原生 X11 会话，TTY？）`。
//!
//! 另外两条括注（Wayland 候选含 x11、无 portal 且仅有 x11 候选）需要真实可连的
//! X server 才能让 `x11_present=true`，无图形 CI 不可建立执行面；判据归属为
//! `modules/capture/src/lib.rs` 的纯函数单测与真机验收。

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_rpc::{method, DoctorResult};
use serde_json::Value;

fn capture_row(value: Value) -> String {
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    report
        .lines
        .into_iter()
        .find(|line| !line.starts_with(' ') && line.contains("截图捕获"))
        .expect("capture row")
}

#[tokio::test]
async fn capture_doctor_row_covers_bound_and_annotation_branches() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;

    // 1) 绑定态：非交互探测得到有效帧 → 选中后端。
    let mut bound =
        Fixture::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string())]).await;
    let row = capture_row(bound.call_ok(method::DOCTOR, None).await);
    assert!(
        row.starts_with("✓ 截图捕获") && row.contains("（选中 portal-screenshot）"),
        "bound row must show the selected backend: {row}"
    );

    // 2) 纯 Wayland 未授权：portal 拒绝且无 x11 兜底。
    mock.set_behaviour(PortalBehaviour::Cancel);
    let mut wayland = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;
    let row = capture_row(wayland.call_ok(method::DOCTOR, None).await);
    assert!(
        row.starts_with("⚠ 截图捕获")
            && row.contains("Wayland 下 portal 未就绪/未授权，无可用后端"),
        "pure Wayland denial annotation must match verbatim: {row}"
    );

    // 3) 原生非 Wayland 未授权：portal 候选存在但静默探测未就绪。
    let mut x11_session =
        Fixture::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string())]).await;
    let row = capture_row(x11_session.call_ok(method::DOCTOR, None).await);
    assert!(
        row.starts_with("⚠ 截图捕获") && row.contains("非交互探测未就绪，需 portal 交互授权"),
        "non-interactive probe annotation must match verbatim: {row}"
    );

    // 4) 无任何后端：无 portal、无原生 X11。
    let mut bare = Fixture::start().await;
    let row = capture_row(bare.call_ok(method::DOCTOR, None).await);
    assert_eq!(
        row, "✗ 截图捕获        : 不可用（无 portal 且无原生 X11 会话，TTY？）",
        "no-backend row must match the fixed text"
    );
}
