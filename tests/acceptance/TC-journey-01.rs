//! TC-journey-01（AC-90，P0）：端到端旅程顺序执行与签发门禁裁决。
//!
//! - 场景：端到端旅程顺序执行与签发门禁裁决
//! - 判据：按序执行并逐项记录；裁决与逐项结果一致
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-journey-01`
//!
//! 判定面是真实 `dispatch` 上按计划顺序执行的一轮旅程：健康体检先行（F5），
//! 其后窗口、工作区、输入、截图、无障碍与事件依次执行并逐项记录退出码；桌面
//! 能力组（AC-70–78）与特权链路（NG2）按设计记跳过。门禁裁决只由逐项结果计算，
//! 负向矩阵覆盖 P0/P1 失败、P0/P1 未跑、P2 跳过与 P2 失败边界；不健康夹具单独
//! 验证 F5：健康步失败后后续步骤不得执行，裁决必须失败。

use std::path::Path;

use acceptance::journey::{Journey, Outcome, Priority, StepSpec, Verdict, EXIT_NOT_RUN};
use acceptance::portal::serve_screenshot_portal;
use acceptance::{
    element, sample_png, window, workspace, FakeA11y, FakeCompositor, Fixture, PrivateBus,
    RecordingInput, ScreenshotMock,
};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, DoctorResult};
use serde_json::{json, Value};

/// 旅程计划（顺序即执行顺序；优先级取自设计仓验收条目）。
///
/// `desktop` 覆盖设计旅程的「桌面能力」组（AC-70–78，均 P2）：本旅程不执行该组，
/// 由 `TC-system-*` 承载，故记跳过；`privileged` 按 NG2 记跳过。`security.status`
/// 只有契约（C-105–C-108）而无验收条目，不进入门禁计划。
const PLAN: [StepSpec; 9] = [
    StepSpec::new("health", Priority::P0),
    StepSpec::new("windows", Priority::P1),
    StepSpec::new("workspaces", Priority::P1),
    StepSpec::new("input", Priority::P0),
    StepSpec::new("capture", Priority::P0),
    StepSpec::new("a11y", Priority::P1),
    StepSpec::new("events", Priority::P1),
    StepSpec::new("desktop", Priority::P2),
    StepSpec::new("privileged", Priority::P2),
];

/// 健康体检顶层行标签（顺序即 AC-01 的固定顺序；GNOME 扩展行不在夹具环境出现）。
const TOP_LEVEL_LABELS: [&str; 5] = ["DE 检测", "合成器", "AT-SPI", "截图捕获", "输入后端"];

/// 顶层行标签：不以空格开头且形如 `<状态> <标签> : ...`。
fn top_level_label(line: &str) -> Option<&'static str> {
    if line.starts_with(' ') {
        return None;
    }
    TOP_LEVEL_LABELS
        .iter()
        .find(|label| line.contains(&format!("{label} ")) || line.contains(&format!("{label}:")))
        .copied()
}

/// 发一条请求并要求成功；失败折叠为步骤错误而不是测试内 panic。
async fn call_ok(
    fixture: &mut Fixture,
    name: &str,
    params: Option<Value>,
) -> Result<Value, String> {
    let response = fixture.call(name, params).await;
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (_, Some(error)) => Err(format!("{name} 失败: {} ({})", error.code, error.message)),
        (None, None) => Err(format!("{name} 既无结果也无错误")),
    }
}

/// 健康体检：必须健康且顶层行完整有序，否则 F5 禁止进入后续旅程。
async fn check_health(fixture: &mut Fixture) -> Result<(), String> {
    let result = call_ok(fixture, method::DOCTOR, None).await?;
    let report: DoctorResult = serde_json::from_value(result)
        .map_err(|error| format!("doctor.run 结果不可解析: {error}"))?;
    if !report.healthy {
        return Err(format!(
            "健康体检不健康（F5：不得继续后续旅程）：\n{}",
            report.lines.join("\n")
        ));
    }
    let labels: Vec<&str> = report
        .lines
        .iter()
        .filter_map(|line| top_level_label(line))
        .collect();
    if labels != TOP_LEVEL_LABELS {
        return Err(format!(
            "健康体检顶层行缺失或错序：期望 {TOP_LEVEL_LABELS:?}，实际 {labels:?}"
        ));
    }
    Ok(())
}

/// 窗口列表：注入的两扇窗口按序返回。
async fn check_windows(fixture: &mut Fixture) -> Result<(), String> {
    let result = call_ok(fixture, method::WINDOWS_LIST, None).await?;
    let count = result
        .get("windows")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    if count != 2 {
        return Err(format!(
            "windows.list 应返回 2 扇窗口，实际 {count}: {result}"
        ));
    }
    Ok(())
}

/// 工作区列表：两个工作区、序号与当前态按夹具投影。
async fn check_workspaces(fixture: &mut Fixture) -> Result<(), String> {
    let result = call_ok(fixture, method::WORKSPACES_LIST, None).await?;
    let items = result
        .get("workspaces")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("workspaces.list 缺少 workspaces 数组: {result}"))?;
    if items.len() != 2 {
        return Err(format!(
            "workspaces.list 应返回 2 个工作区，实际 {}",
            items.len()
        ));
    }
    if items[0].get("number") != Some(&json!(1)) || items[0].get("is_active") != Some(&json!(true))
    {
        return Err(format!("首个工作区投影不符合夹具：{}", items[0]));
    }
    Ok(())
}

/// 输入注入：请求落到注入的记录型后端一次。
async fn check_input(fixture: &mut Fixture, backend: &RecordingInput) -> Result<(), String> {
    let before = backend.calls().len();
    let result = call_ok(
        fixture,
        method::INPUT_SEND,
        Some(json!({"kind": "key", "payload": {"combo": "ctrl+shift+t"}})),
    )
    .await?;
    if result.get("ok") != Some(&json!(true)) {
        return Err(format!("input.send 未回执成功: {result}"));
    }
    if backend.calls().len() != before + 1 {
        return Err("input.send 未落到注入的输入后端".into());
    }
    Ok(())
}

/// 截图：portal 夹具的 PNG 落到目标路径且非空。
async fn check_capture(fixture: &mut Fixture, output: &Path) -> Result<(), String> {
    let result = call_ok(
        fixture,
        method::SCREENSHOT_CAPTURE,
        Some(json!({
            "window": null,
            "area": null,
            "output_path": output.display().to_string(),
        })),
    )
    .await?;
    if result.get("path") != Some(&json!(output.display().to_string())) {
        return Err(format!("截图回执路径与目标不一致: {result}"));
    }
    let bytes = std::fs::read(output).map_err(|error| format!("截图产物不可读: {error}"))?;
    if bytes.is_empty() {
        return Err(format!("截图产物为空: {}", output.display()));
    }
    Ok(())
}

/// 无障碍查询：注入的元素树全部返回。
async fn check_a11y(fixture: &mut Fixture) -> Result<(), String> {
    let result = call_ok(fixture, method::A11Y_QUERY, Some(json!({ "all": true }))).await?;
    if result.get("count") != Some(&json!(3)) {
        return Err(format!("a11y.query 应返回 3 个元素: {result}"));
    }
    Ok(())
}

/// 事件订阅：返回非空订阅标识。
async fn check_events(fixture: &mut Fixture) -> Result<(), String> {
    let subscription = fixture
        .subscribe(Some("window"))
        .await
        .map_err(|error| format!("events.subscribe 失败: {} ({})", error.code, error.message))?;
    if subscription.id().is_empty() {
        return Err("events.subscribe 未返回订阅标识".into());
    }
    Ok(())
}

/// 按计划顺序执行旅程并逐项记录；健康体检失败后其余步骤按 F5 记跳过。
///
/// P2 步（桌面能力组、特权链路）不进入门禁，固定记跳过。
async fn execute(
    fixture: &mut Fixture,
    backend: &RecordingInput,
    capture_output: &Path,
) -> Journey {
    let mut journey = Journey::new(PLAN.to_vec());
    let mut health_failed = false;
    for spec in PLAN {
        if health_failed || spec.priority == Priority::P2 {
            journey
                .record(spec.id, Outcome::Skipped, EXIT_NOT_RUN)
                .expect("record skip in plan order");
            continue;
        }
        let step = match spec.id {
            "health" => check_health(fixture).await,
            "windows" => check_windows(fixture).await,
            "workspaces" => check_workspaces(fixture).await,
            "input" => check_input(fixture, backend).await,
            "capture" => check_capture(fixture, capture_output).await,
            "a11y" => check_a11y(fixture).await,
            "events" => check_events(fixture).await,
            other => unreachable!("计划外步骤 {other}"),
        };
        match step {
            Ok(()) => journey
                .record(spec.id, Outcome::Passed, 0)
                .expect("record pass in plan order"),
            Err(message) => {
                eprintln!("旅程步骤 {} 失败：{message}", spec.id);
                if spec.id == "health" {
                    health_failed = true;
                }
                journey
                    .record(spec.id, Outcome::Failed, 1)
                    .expect("record failure in plan order");
            }
        }
    }
    journey
}

/// 以同一计划重放一次旅程：只覆盖一个步骤的结果，其余按通过与 0 退出。
fn replay(step: &str, outcome: Outcome, exit_code: i32) -> Journey {
    let mut journey = Journey::new(PLAN.to_vec());
    for spec in PLAN {
        let (result, code) = if spec.id == step {
            (outcome, exit_code)
        } else {
            (Outcome::Passed, 0)
        };
        journey
            .record(spec.id, result, code)
            .expect("replay in plan order");
    }
    journey
}

/// 裁决必须可由逐项结果重算：阻断项、跳过项与非阻断失败项逐条相等。
fn assert_verdict_matches_records(journey: &Journey) -> Verdict {
    let verdict = journey.verdict().expect("complete journey is judgeable");
    let blocking: Vec<&str> = journey
        .records()
        .iter()
        .filter(|record| record.priority.is_blocking() && record.outcome != Outcome::Passed)
        .map(|record| record.id)
        .collect();
    let skipped: Vec<&str> = journey
        .records()
        .iter()
        .filter(|record| record.priority == Priority::P2 && record.outcome == Outcome::Skipped)
        .map(|record| record.id)
        .collect();
    let non_blocking: Vec<&str> = journey
        .records()
        .iter()
        .filter(|record| record.priority == Priority::P2 && record.outcome == Outcome::Failed)
        .map(|record| record.id)
        .collect();
    assert_eq!(
        verdict.blocking, blocking,
        "阻断项必须逐条来自 P0/P1 未通过"
    );
    assert_eq!(verdict.skipped, skipped, "P2 未跑必须逐条标注");
    assert_eq!(
        verdict.non_blocking_failed, non_blocking,
        "P2 失败必须逐条标注且不阻断"
    );
    assert_eq!(verdict.is_pass(), blocking.is_empty());
    verdict
}

#[tokio::test]
async fn ordered_journey_records_every_step_and_gate_matches_results() {
    // 完整环境：私有 session bus 上的截图 portal（截图步）+ 脚本化合成器
    // （窗口/工作区步）+ 记录型输入链（输入步）+ 无障碍夹具。
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let _portal = serve_screenshot_portal(&bus, mock).await;
    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("XDG_CURRENT_DESKTOP", "KDE".to_string()),
        ("WAYLAND_DISPLAY", "wayland-acceptance".to_string()),
        ("XDG_SESSION_TYPE", "wayland".to_string()),
    ])
    .await;

    let compositor = FakeCompositor::new();
    {
        let desktop = compositor.state();
        let mut state = desktop.lock().expect("fake state");
        state.windows = vec![
            window("w-1", "Terminal", "org.kde.konsole", 0, 0, 800, 600),
            window("w-2", "Files", "org.kde.dolphin", 100, 200, 640, 480),
        ];
        state.workspaces = vec![
            workspace("0", "One", 1, true),
            workspace("1", "Two", 2, false),
        ];
    }
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);
    let input = RecordingInput::new("record-backend");
    fixture
        .set_input_chain(DesktopEnvironment::KDE, vec![Box::new(input.clone())])
        .await;
    fixture.set_a11y(FakeA11y::new(vec![
        element(":1.42", "/root/1", "确定", "push button", 43, 1),
        element(":1.43", "/root/2", "文档", "document frame", 69, 0),
        element(":1.43", "/root/3", "侧边栏", "panel", 39, 0),
    ]));

    // ── 场景 1：真实旅程按序执行，逐项记录与计划一一对应 ──
    let capture_output = out_dir.path().join("journey-shot.png");
    let journey = execute(&mut fixture, &input, &capture_output).await;
    let recorded: Vec<&str> = journey.records().iter().map(|record| record.id).collect();
    let planned: Vec<&str> = PLAN.iter().map(|spec| spec.id).collect();
    assert_eq!(recorded, planned, "记录顺序必须与计划顺序一致");
    for (record, spec) in journey.records().iter().zip(PLAN.iter()) {
        assert_eq!(record.priority, spec.priority);
        if record.priority == Priority::P2 {
            assert_eq!(
                record.outcome,
                Outcome::Skipped,
                "P2 能力组 {} 按设计记跳过",
                record.id
            );
            assert_eq!(record.exit_code, EXIT_NOT_RUN);
        } else {
            assert_eq!(
                record.outcome,
                Outcome::Passed,
                "步骤 {} 必须通过",
                record.id
            );
            assert_eq!(record.exit_code, 0, "通过步骤的退出码必须为零");
        }
    }

    // ── 场景 2：真实旅程的裁决与逐项结果一致，P2 跳过已标注 ──
    let verdict = assert_verdict_matches_records(&journey);
    assert!(verdict.is_pass(), "真实旅程必须通过签发门禁: {verdict:?}");
    assert_eq!(verdict.skipped, vec!["desktop", "privileged"]);
    assert_eq!(verdict.non_blocking_failed, Vec::<&str>::new());
    assert!(
        verdict.summary().contains("P2 跳过: desktop, privileged"),
        "裁决必须标注未跑的 P2: {}",
        verdict.summary()
    );

    // ── 场景 3：门禁边界矩阵——P0/P1 失败必失败，P0/P1 未跑不算通过 ──
    let verdict = assert_verdict_matches_records(&replay("input", Outcome::Failed, 1));
    assert!(!verdict.is_pass(), "P0 失败必须判失败");
    assert_eq!(verdict.blocking, vec!["input"]);

    let verdict =
        assert_verdict_matches_records(&replay("capture", Outcome::Skipped, EXIT_NOT_RUN));
    assert!(!verdict.is_pass(), "P0 未跑不算通过");
    assert_eq!(verdict.blocking, vec!["capture"]);

    let verdict = assert_verdict_matches_records(&replay("windows", Outcome::Failed, 1));
    assert!(!verdict.is_pass(), "P1 失败必须判失败");
    assert_eq!(verdict.blocking, vec!["windows"]);

    let verdict = assert_verdict_matches_records(&replay("a11y", Outcome::Skipped, EXIT_NOT_RUN));
    assert!(!verdict.is_pass(), "P1 未跑不算通过");
    assert_eq!(verdict.blocking, vec!["a11y"]);
    assert!(verdict.skipped.is_empty(), "P1 未跑不得记为 P2 跳过");

    // ── 场景 4：P2 不阻断，跳过与失败都必须标注 ──
    let verdict =
        assert_verdict_matches_records(&replay("privileged", Outcome::Skipped, EXIT_NOT_RUN));
    assert!(verdict.is_pass(), "P2 未跑不算失败");
    assert_eq!(verdict.skipped, vec!["privileged"]);

    let verdict = assert_verdict_matches_records(&replay("privileged", Outcome::Failed, 2));
    assert!(verdict.is_pass(), "P2 失败不阻断签发");
    assert_eq!(verdict.non_blocking_failed, vec!["privileged"]);

    let mut all_pass = Journey::new(PLAN.to_vec());
    for spec in PLAN {
        all_pass
            .record(spec.id, Outcome::Passed, 0)
            .expect("ordered all-pass replay");
    }
    let verdict = assert_verdict_matches_records(&all_pass);
    assert!(verdict.is_pass());
    assert!(verdict.skipped.is_empty() && verdict.non_blocking_failed.is_empty());

    // ── 场景 5：记录面拒绝乱序、重复、计划外与漏项裁决 ──
    let mut rejected = Journey::new(PLAN.to_vec());
    let error = rejected
        .record("windows", Outcome::Passed, 0)
        .expect_err("乱序必须被拒绝");
    assert!(error.contains("乱序"), "{error}");
    let error = rejected
        .record("nope", Outcome::Passed, 0)
        .expect_err("计划外步骤必须被拒绝");
    assert!(error.contains("不在计划内"), "{error}");
    let error = rejected.verdict().expect_err("漏项不得产出裁决");
    assert!(
        error.contains("health") && error.contains("缺少 9 步"),
        "{error}"
    );
    rejected
        .record("health", Outcome::Passed, 0)
        .expect("按序记录 health");
    let error = rejected
        .record("health", Outcome::Passed, 0)
        .expect_err("重复记录必须被拒绝");
    assert!(error.contains("不得重复"), "{error}");
    for spec in &PLAN[1..] {
        rejected
            .record(spec.id, Outcome::Passed, 0)
            .expect("按序记录剩余步骤");
    }
    let error = rejected
        .record("health", Outcome::Passed, 0)
        .expect_err("计划已满不得再记录");
    assert!(
        error.contains("已记录") || error.contains("超出计划"),
        "{error}"
    );

    // ── 场景 6：F5 逆变——健康体检不健康时不得继续旅程，裁决必须失败 ──
    // 只装配空夹具并显式摘除输入组件：合成器、截图捕获与输入后端都不可用，
    // 健康报告必有 ✗ 行（与 TC-doctor-01 的不可用环境同口径）。
    let mut unhealthy = Fixture::start_with_env(&[("PATH", String::new())]).await;
    unhealthy.set_input_unavailable(Some("输入面被夹具隔离"));
    let report: DoctorResult =
        serde_json::from_value(unhealthy.call_ok(method::DOCTOR, None).await)
            .expect("doctor result deserializes");
    assert!(
        !report.healthy,
        "前提：空夹具必须不健康；实际报告:\n{}",
        report.lines.join("\n")
    );
    let blocked_output = out_dir.path().join("must-not-exist.png");
    let backend = RecordingInput::new("record-backend");
    let failed = execute(&mut unhealthy, &backend, &blocked_output).await;
    assert!(
        backend.calls().is_empty(),
        "健康体检失败后不得调用输入后端: {:?}",
        backend.calls()
    );
    assert!(!blocked_output.exists(), "健康体检失败后不得执行截图");
    let recorded: Vec<&str> = failed.records().iter().map(|record| record.id).collect();
    assert_eq!(recorded, planned, "逆变旅程也必须逐项记录、无漏项");
    for record in failed.records() {
        if record.id == "health" {
            assert_eq!(record.outcome, Outcome::Failed, "健康步必须记失败");
            assert_eq!(record.exit_code, 1, "失败步的退出码约定为 1");
        } else {
            assert_eq!(
                record.outcome,
                Outcome::Skipped,
                "F5：健康体检失败后不得执行 {}",
                record.id
            );
            assert_eq!(record.exit_code, EXIT_NOT_RUN, "跳过步没有退出码");
        }
    }
    let verdict = assert_verdict_matches_records(&failed);
    assert!(!verdict.is_pass(), "健康体检失败必须判失败");
    assert_eq!(
        verdict.blocking,
        vec![
            "health",
            "windows",
            "workspaces",
            "input",
            "capture",
            "a11y",
            "events"
        ],
        "阻断项必须覆盖全部 P0/P1 步骤"
    );
    assert_eq!(verdict.skipped, vec!["desktop", "privileged"]);
}
