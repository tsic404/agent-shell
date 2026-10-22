//! TC-doctor-01（AC-01，P0）：健康报告完整性与顶层行顺序。
//!
//! - 场景：完整报告、顶层行顺序、无废弃标签、缺失行的显式状态
//! - 判据：行名与顺序正确、无废弃标签、完整环境零退出；缺失行有明确状态
//!   说明而非静默省略
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-doctor-01`
//!
//! 判定面是真实 `doctor.run`。完整环境由夹具建立：私有 session bus 上的
//! Screenshot portal（截图行就绪）+ 脚本化合成器（合成器行）+ 记录型输入链
//! （输入行就绪）；不可用环境是同一夹具的 headless 形态。

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, FakeCompositor, Fixture, PrivateBus, RecordingInput, ScreenshotMock};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, DoctorResult};
use serde_json::Value;

/// 顶层行标签（顺序即 AC-01 的固定顺序；GNOME 的 Shell 扩展行只在 GNOME 出现）。
const TOP_LEVEL_LABELS: [&str; 5] = ["DE 检测", "合成器", "AT-SPI", "截图捕获", "输入后端"];

/// 顶层行标签：非子行（不以空格开头）且形如 `<状态> <标签> : ...`。
fn top_level_label(line: &str) -> Option<&'static str> {
    if line.starts_with(' ') {
        return None;
    }
    TOP_LEVEL_LABELS
        .iter()
        .find(|label| line.contains(&format!("{label} ")) || line.contains(&format!("{label}:")))
        .copied()
}

fn doctor_of(value: Value) -> DoctorResult {
    serde_json::from_value(value).expect("DoctorResult deserializes")
}

#[tokio::test]
async fn health_report_has_fixed_top_level_order_and_explicit_statuses() {
    // 完整环境：portal 就绪 + 合成器就绪 + 输入链就绪。
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let _portal = serve_screenshot_portal(&bus, mock).await;
    let mut complete = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("XDG_CURRENT_DESKTOP", "KDE".to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
        ("XDG_SESSION_TYPE", "wayland".to_string()),
    ])
    .await;
    let compositor = FakeCompositor::new();
    complete.install_compositor(&compositor, DesktopEnvironment::KDE);
    complete
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![Box::new(RecordingInput::new("record-backend"))],
        )
        .await;

    let report = doctor_of(complete.call_ok(method::DOCTOR, None).await);
    let labels: Vec<&str> = report
        .lines
        .iter()
        .filter_map(|line| top_level_label(line))
        .collect();
    assert_eq!(
        labels,
        TOP_LEVEL_LABELS,
        "top-level rows must appear once each in the fixed order; got:\n{}",
        report.lines.join("\n")
    );
    assert!(
        report.healthy,
        "complete environment must be healthy (⚠ AT-SPI does not fail health); got:\n{}",
        report.lines.join("\n")
    );
    assert!(
        report.lines.iter().all(|line| !line.starts_with('✗')),
        "healthy report must not carry unavailable rows; got:\n{}",
        report.lines.join("\n")
    );
    assert!(
        report.lines[0].contains("KDE") && report.lines[0].contains("Wayland"),
        "detection row must report the actual desktop and session type; got:\n{}",
        report.lines[0]
    );

    // 不可用环境：每一行仍有显式状态，不静默省略。
    let mut unavailable = Fixture::start().await;
    let report = doctor_of(unavailable.call_ok(method::DOCTOR, None).await);
    let labels: Vec<&str> = report
        .lines
        .iter()
        .filter_map(|line| top_level_label(line))
        .collect();
    assert_eq!(
        labels,
        TOP_LEVEL_LABELS,
        "incomplete environment must still report every top-level row; got:\n{}",
        report.lines.join("\n")
    );
    assert!(
        !report.healthy,
        "unavailable environment must report unhealthy; got:\n{}",
        report.lines.join("\n")
    );
    for label in ["合成器", "截图捕获", "输入后端"] {
        let line = report
            .lines
            .iter()
            .find(|line| top_level_label(line) == Some(label))
            .unwrap_or_else(|| panic!("missing {label} row"));
        assert!(
            line.starts_with('✗'),
            "{label} row must carry an explicit unavailable status: {line}"
        );
        assert!(
            line.len() > format!("✗ {label}").len() + 3,
            "{label} row must give a reason, got: {line}"
        );
    }
    // 废弃标签不得出现（C-020）。
    for legacy in ["主后端", "事件流"] {
        assert!(
            report.lines.iter().all(|line| !line.contains(legacy)),
            "legacy label {legacy:?} must not appear; got:\n{}",
            report.lines.join("\n")
        );
    }
}
