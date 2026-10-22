//! TC-doctor-05（AC-05，P1）：输入后端绑定态与不可用原因。
//!
//! - 场景：输入后端行显示选中后端、与降级链一致；不可用时给出根因
//! - 判据：选中后端落在候选链内；不可用时给出候选与根因
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-doctor-05`
//!
//! 判定面是真实 `doctor.run` 的输入行与真实 `input.send` 的同一状态源：
//! 绑定态由夹具候选链给出（记录型后端），不可用态由 `input_error` 根因给出。

use acceptance::{Fixture, RecordingInput};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, DoctorResult, RpcErrorCode};
use serde_json::json;

fn input_row(value: serde_json::Value) -> String {
    let report: DoctorResult = serde_json::from_value(value).expect("DoctorResult");
    report
        .lines
        .into_iter()
        .find(|line| !line.starts_with(' ') && line.contains("输入后端"))
        .expect("input row")
}

#[tokio::test]
async fn input_row_reports_chain_selection_and_unavailable_reason() {
    let mut fixture = Fixture::start().await;

    // 绑定态：箭头链 + 选中后端；选中者必须在链内。
    fixture
        .set_input_chain(
            DesktopEnvironment::KDE,
            vec![
                Box::new(RecordingInput::new("native")),
                Box::new(RecordingInput::new("ydotool")),
            ],
        )
        .await;
    let row = input_row(fixture.call_ok(method::DOCTOR, None).await);
    assert!(
        row.starts_with("✓ 输入后端"),
        "bound input row must be ready: {row}"
    );
    assert!(
        row.contains("native → ydotool（选中 native）"),
        "row must render the chain and the selected backend: {row}"
    );
    let selected = fixture.input_backend_name().expect("selected backend");
    assert!(
        fixture.input_chain_names().contains(&selected),
        "selected backend {selected} must belong to the chain"
    );

    // 不可用态：带根因；与 input.send 的错误同源。
    let reason = "portal backend does not support EIS: RemoteDesktop.ConnectToEIS missing";
    fixture.set_input_unavailable(Some(reason));
    let row = input_row(fixture.call_ok(method::DOCTOR, None).await);
    assert!(
        row.starts_with("✗ 输入后端") && row.contains(reason),
        "unavailable row must carry the probe reason: {row}"
    );
    let error = fixture
        .call_err(
            method::INPUT_SEND,
            Some(json!({"kind": "key", "payload": {"combo": "a"}})),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
    assert_eq!(
        error.message, reason,
        "input.send must surface the same root cause as doctor"
    );

    // 无具体根因时回退到友好文案（TTY/无注入后端）。
    fixture.set_input_unavailable(None);
    let row = input_row(fixture.call_ok(method::DOCTOR, None).await);
    assert!(
        row.contains("不可用（TTY 或无注入后端）"),
        "missing root cause must fall back to the fixed friendly text: {row}"
    );
}
