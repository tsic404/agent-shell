//! TC-events-04（AC-63，P2）：doctor 事件脚本行如实反映 KWin 侧装载态。
//!
//! - 场景：完整可健康环境（截图 portal + 脚本化合成器 + 输入链）下，订阅前后
//!   各读一次 doctor；装载行追加说明/改措辞后再读
//! - 判据：尚未装配时报告未加载并说明懒启动，首次订阅装配后报告已装配；行内
//!   附加说明与措辞变化不作为失败依据
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-events-04`
//!
//! 装配状态由生产状态机提供（`pipeline_state()` + 原始源取流次数），doctor 行
//! 由 compositor 提供、daemon 逐行渲染；健康判定只由 `✗` 行驱动。KWin 侧最终
//! 装载真值由真机 KDE 会话确认。

use acceptance::portal::serve_screenshot_portal;
use acceptance::{
    sample_png, FakeCompositor, Fixture, PrivateBus, RecordingInput, RecordingRawSource,
    ScreenshotMock,
};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, DoctorResult};
use serde_json::Value;

const UNLOADED: &str = "⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）";
const LOADED: &str = "✓ 事件脚本    : 已装配（QTimer 轮询 windowList 差分 + workspace 信号）";
const ANNOTATED: &str = "✓ 事件脚本    : 已装配（QTimer 轮询 windowList 差分 + workspace 信号；附加说明：外部实例已注册）";

fn doctor_of(value: Value) -> DoctorResult {
    serde_json::from_value(value).expect("DoctorResult deserializes")
}

fn script_row(report: &DoctorResult) -> String {
    report
        .lines
        .iter()
        .find(|line| line.contains("事件脚本"))
        .cloned()
        .expect("event script row")
}

#[tokio::test]
async fn event_script_row_is_truthful_and_annotations_do_not_flip_health() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let _portal = serve_screenshot_portal(&bus, mock).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("XDG_CURRENT_DESKTOP", "KDE".to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
    ])
    .await;
    fixture.install_compositor(&FakeCompositor::new(), DesktopEnvironment::KDE);
    fixture
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![Box::new(RecordingInput::new("record-backend"))],
        )
        .await;
    let source = RecordingRawSource::new(event::EventSource::KWinWayland);
    fixture.set_compositor_raw_source(Some(Box::new(source.clone())));

    // 订阅前：未装配是预期；行必须说明懒启动，且不是不可用行。
    assert_eq!(fixture.pipeline_state(), (false, 0));
    fixture.set_compositor_doctor_lines(vec![UNLOADED.to_string()]);
    let before = doctor_of(fixture.call_ok(method::DOCTOR, None).await);
    assert_eq!(script_row(&before), UNLOADED);
    assert!(
        script_row(&before).contains("懒启动"),
        "unloaded row must explain lazy start"
    );
    assert!(
        !before
            .lines
            .iter()
            .any(|line| line.starts_with("✗ 事件脚本")),
        "unloaded/lazy is not an unavailable row:\n{}",
        before.lines.join("\n")
    );
    assert!(
        before.healthy,
        "complete environment must be healthy before subscribe:\n{}",
        before.lines.join("\n")
    );

    // 首次订阅：生产状态机装配管线；随后行转为已装配。
    fixture
        .subscribe(Some("WindowOpened"))
        .await
        .expect("subscribe installs the pipeline without error");
    assert_eq!(
        fixture.pipeline_state(),
        (true, 1),
        "subscribe must flip the production assembly state"
    );
    source.wait_events_calls(1).await;
    fixture.set_compositor_doctor_lines(vec![LOADED.to_string()]);
    let after = doctor_of(fixture.call_ok(method::DOCTOR, None).await);
    assert_eq!(script_row(&after), LOADED);
    assert!(
        script_row(&after).contains("已装配"),
        "loaded row must say assembled"
    );
    assert!(
        after.healthy,
        "loaded row must keep the environment healthy"
    );

    // 附加说明/措辞变化：按当前值渲染，不判失败、不翻转健康。
    fixture.set_compositor_doctor_lines(vec![ANNOTATED.to_string()]);
    let annotated = doctor_of(fixture.call_ok(method::DOCTOR, None).await);
    assert_eq!(script_row(&annotated), ANNOTATED);
    assert_eq!(
        annotated.healthy, after.healthy,
        "additional annotations must not flip health"
    );
}
