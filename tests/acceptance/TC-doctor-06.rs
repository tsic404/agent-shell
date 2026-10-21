//! TC-doctor-06（AC-06，P1）：无障碍行就绪与未启用两种状态。
//!
//! - 场景：无障碍行在就绪时报告总线可用且注册表可达，未就绪时明确报告不可达
//! - 判据：就绪文本与不可达文本可区分，不得静默通过
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-doctor-06`
//!
//! 未就绪分支是 headless 真实探测（无 session bus）。就绪分支在私有 session
//! bus 上模拟 launcher（`org.a11y.Bus` + `org.a11y.Status.IsEnabled=true`）与
//! Registry（根节点 ChildCount=3），探测链与桥接同源（`AT_SPI_BUS_ADDRESS`）。

use acceptance::bus::{
    claim, A11yStatus, AtspiRegistry, A11Y_BUS_SERVICE, A11Y_STATUS_PATH, ATSPI_REGISTRY_SERVICE,
    ATSPI_ROOT_PATH,
};
use acceptance::{Fixture, PrivateBus};
use agent_shell_rpc::{method, A11yStatusResult, DoctorResult};
use serde_json::Value;

fn atspi_row(value: Value) -> String {
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    report
        .lines
        .into_iter()
        .find(|line| !line.starts_with(' ') && line.contains("AT-SPI"))
        .expect("AT-SPI row")
}

fn a11y_status(value: Value) -> A11yStatusResult {
    serde_json::from_value(value).expect("A11yStatusResult")
}

#[tokio::test]
async fn a11y_row_distinguishes_reachable_from_unavailable() {
    // 未就绪：无 session bus → 明确不可达，且 available=false。
    let mut headless = Fixture::start().await;
    let row = atspi_row(headless.call_ok(method::DOCTOR, None).await);
    assert!(
        row.starts_with("⚠ AT-SPI") && row.contains("unavailable") && row.contains("session bus"),
        "headless row must state the unreachable stage: {row}"
    );
    let status = a11y_status(headless.call_ok(method::A11Y_STATUS, None).await);
    assert!(
        !status.available,
        "unreachable AT-SPI must not report available"
    );
    assert_eq!(
        status.detail, row,
        "a11y.status and doctor must share one conclusion"
    );

    // 就绪：私有 bus 上的 launcher + Registry。
    let bus = PrivateBus::start();
    let launcher = bus.serve(A11Y_STATUS_PATH, A11yStatus::new(true)).await;
    claim(&launcher, A11Y_BUS_SERVICE).await;
    let registry = bus.serve(ATSPI_ROOT_PATH, AtspiRegistry::new(3)).await;
    claim(&registry, ATSPI_REGISTRY_SERVICE).await;

    let mut reachable = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("AT_SPI_BUS_ADDRESS", bus.address().to_string()),
    ])
    .await;
    let row = atspi_row(reachable.call_ok(method::DOCTOR, None).await);
    assert!(
        row.starts_with("✓ AT-SPI")
            && row.contains("enabled (a11y bus up, Registry reachable")
            && row.contains("3 nodes registered")
            && row.contains("AT-SPI on"),
        "reachable row must report bus, registry and enablement: {row}"
    );
    let status = a11y_status(reachable.call_ok(method::A11Y_STATUS, None).await);
    assert!(status.available);
    assert_eq!(status.detail, row);
}
