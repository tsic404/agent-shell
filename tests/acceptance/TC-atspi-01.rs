//! TC-atspi-01（AC-50，P1）：AT-SPI 存活性如实报告（可达 vs 不可达）。
//!
//! - 场景：私有 session bus 上模拟完整的 `org.a11y.Bus` + `org.a11y.atspi.Registry`
//!   访问链，与 headless（无 session bus、无 a11y bus）会话对照。
//! - 判据：可达时 `a11y.status.available = true` 且明细为 enabled 行；不可达时
//!   `available = false` 且明细带 `unavailable` 与阶段原因；两侧的 `a11y.status`
//!   明细与 `doctor.run` 的 AT-SPI 行一致，不得虚报可用。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-atspi-01`
//!
//! 环境说明：真实 AT-SPI 树需要桌面应用注册到会话 a11y bus，CI 无法复现；
//! 可达分支使用私有 dbus-daemon 承载与生产探测链同源的坐标（launcher、Registry
//! 根节点），判定仍是生产 `atspi_line()` 的分级探测与 error-free RPC 投影。

use acceptance::bus::{
    claim, A11yStatus, AtspiRegistry, PrivateBus, A11Y_BUS_SERVICE, A11Y_STATUS_PATH,
    ATSPI_REGISTRY_SERVICE, ATSPI_ROOT_PATH,
};
use acceptance::Fixture;
use agent_shell_rpc::{method, A11yStatusResult, DoctorResult};
use serde_json::Value;

fn status_of(value: Value) -> A11yStatusResult {
    serde_json::from_value(value).expect("A11yStatusResult deserializes")
}

fn doctor_of(value: Value) -> DoctorResult {
    serde_json::from_value(value).expect("DoctorResult deserializes")
}

/// doctor 报告中的 AT-SPI 顶层行（与 `a11y.status` 共用同一探测函数）。
fn atspi_row(lines: &[String]) -> &str {
    lines
        .iter()
        .find(|line| line.starts_with("✓ AT-SPI") || line.starts_with("⚠ AT-SPI"))
        .unwrap_or_else(|| panic!("doctor report carries no AT-SPI row:\n{}", lines.join("\n")))
}

#[tokio::test]
async fn a11y_status_reports_reachability_truthfully_and_matches_doctor() {
    // ── 可达分支：私有 bus 上同时提供 launcher 与 Registry 根节点 ──
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

    let status = status_of(reachable.call_ok(method::A11Y_STATUS, None).await);
    assert!(
        status.available,
        "reachable a11y bus must report available=true; detail: {}",
        status.detail
    );
    assert!(
        status.detail.starts_with("✓ AT-SPI"),
        "reachable status detail must be the enabled row: {}",
        status.detail
    );
    for fragment in [
        "enabled (a11y bus up, Registry reachable",
        "AT-SPI on",
        "3 nodes registered",
    ] {
        assert!(
            status.detail.contains(fragment),
            "status detail must carry {fragment:?}: {}",
            status.detail
        );
    }

    let report = doctor_of(reachable.call_ok(method::DOCTOR, None).await);
    assert_eq!(
        atspi_row(&report.lines),
        status.detail,
        "doctor AT-SPI row must be the same probe result as a11y.status"
    );
    assert!(
        !report.lines.iter().any(|line| line.starts_with("✗ AT-SPI")),
        "reachable AT-SPI must not be reported as an unavailable row:\n{}",
        report.lines.join("\n")
    );

    // ── 不可达分支：撤掉私有 bus 与地址覆盖，回到 headless 形态 ──
    drop(registry);
    drop(launcher);
    drop(bus);
    std::env::remove_var("AT_SPI_BUS_ADDRESS");

    let mut headless = Fixture::start().await;
    let status = status_of(headless.call_ok(method::A11Y_STATUS, None).await);
    assert!(
        !status.available,
        "headless session must not report a11y available; detail: {}",
        status.detail
    );
    assert!(
        status.detail.starts_with("⚠ AT-SPI"),
        "unreachable status detail must be the warning row: {}",
        status.detail
    );
    assert!(
        status.detail.contains("unavailable"),
        "warning row must state unavailability: {}",
        status.detail
    );
    assert!(
        status.detail.contains("session bus"),
        "warning row must name the failing probe stage: {}",
        status.detail
    );

    let report = doctor_of(headless.call_ok(method::DOCTOR, None).await);
    assert_eq!(
        atspi_row(&report.lines),
        status.detail,
        "doctor AT-SPI row must be the same probe result as a11y.status"
    );
    assert!(
        !report.lines.iter().any(|line| line.starts_with("✓ AT-SPI")),
        "headless doctor must not claim AT-SPI reachable:\n{}",
        report.lines.join("\n")
    );
}
