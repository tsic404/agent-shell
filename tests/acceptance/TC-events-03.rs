//! TC-events-03（AC-62，P2）：事件脚本懒启动——冷启动未装配、首次订阅装配。
//!
//! - 场景：脚本化合成器 + 记录型原始事件源；首次 `events.subscribe` 前后读取
//!   事件管线装配状态与 `doctor.run` 的事件脚本行
//! - 判据：订阅前「未装配」属预期；订阅后转为已装配（装配由生产状态机完成，
//!   原始事件源被取流一次）；再次订阅不重复装配
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-events-03`
//!
//! 装载态的可判据证据：`pipeline_state()` 读 daemon 的装配标志与原始源被取走
//! 次数，`RecordingRawSource::events_calls()` 读 `EventNormalizer` 是否真正取流
//! ——三者都由生产路径写入，测试只读取。KWin 侧脚本装载态的最终真值由真机
//! KDE 会话确认（headless 无法复现 KWin 进程内信号）。

use acceptance::{FakeCompositor, Fixture, RecordingRawSource};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, DoctorResult};
use serde_json::Value;

const UNLOADED: &str = "⚠ 事件脚本    : 未加载（懒启动，首次 events subscribe 时装配）";
const LOADED: &str = "✓ 事件脚本    : 已装配（QTimer 轮询 windowList 差分 + workspace 信号）";

fn script_row(value: Value) -> String {
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    report
        .lines
        .into_iter()
        .find(|line| line.contains("事件脚本"))
        .expect("event script row")
}

#[tokio::test]
async fn subscribe_installs_the_event_pipeline_and_doctor_renders_load_state() {
    let mut fixture = Fixture::start().await;
    fixture.install_compositor(&FakeCompositor::new(), DesktopEnvironment::KDE);
    let source = RecordingRawSource::new(event::EventSource::KWinWayland);
    fixture.set_compositor_raw_source(Some(Box::new(source.clone())));

    // 订阅前：未装配、原始源未被取流；doctor 行如实报告懒启动。
    assert_eq!(
        fixture.pipeline_state(),
        (false, 0),
        "cold start must not have an installed pipeline"
    );
    assert_eq!(source.events_calls(), 0);
    fixture.set_compositor_doctor_lines(vec![UNLOADED.to_string()]);
    assert_eq!(
        script_row(fixture.call_ok(method::DOCTOR, None).await),
        UNLOADED,
        "before subscribe the script row must report unloaded/lazy"
    );

    // 首次订阅：生产状态机装配管线（取走原始源一次），订阅建立。
    let subscription = fixture
        .subscribe(Some("WindowOpened"))
        .await
        .expect("subscribe must succeed (pipeline installed)");
    assert!(!subscription.id().is_empty(), "subscriber_id is present");
    assert_eq!(fixture.subscriber_count(), 1);
    assert_eq!(
        fixture.pipeline_state(),
        (true, 1),
        "first subscribe must install the pipeline and take the raw source once"
    );
    source.wait_events_calls(1).await;

    // 再次订阅：幂等，不重复取源、不重复装配。
    let second = fixture
        .subscribe(None)
        .await
        .expect("second subscribe succeeds without reinstalling the pipeline");
    assert_ne!(second.id(), subscription.id());
    assert_eq!(fixture.subscriber_count(), 2);
    assert_eq!(
        fixture.pipeline_state(),
        (true, 1),
        "second subscribe must not re-install or re-take the source"
    );
    assert_eq!(source.events_calls(), 1);

    // 装配后的行渲染为已装配（KWin 侧最终真值归真机会话）。
    fixture.set_compositor_doctor_lines(vec![LOADED.to_string()]);
    assert_eq!(
        script_row(fixture.call_ok(method::DOCTOR, None).await),
        LOADED,
        "doctor must render the current compositor script row after assembly"
    );
}
