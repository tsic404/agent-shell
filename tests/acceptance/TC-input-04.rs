//! TC-input-04（AC-33，P0）：正向滚动与负数反向滚动。
//!
//! - 场景：`input scroll --dx --dy` 的两个方向
//! - 判据：正向与反向滚动均可观察
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-input-04`
//!
//! 判定面是真实 `input.send` → dispatcher 的 `Op::Scroll` 边界：符号与分量
//! 原样抵达后端（正 dy 向下、正 dx 向右），缺省分量为 0 且不报错。真实滚轮
//! 位移由后端的后端路径执行，真机验收观察页面滚动。

use acceptance::{Fixture, RecordedOp, RecordingInput};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::method;
use serde_json::json;

fn scroll_params(dx: Option<i32>, dy: Option<i32>) -> serde_json::Value {
    match (dx, dy) {
        (Some(dx), Some(dy)) => json!({"kind": "scroll", "payload": {"dx": dx, "dy": dy}}),
        _ => json!({"kind": "scroll", "payload": {}}),
    }
}

#[tokio::test]
async fn scroll_direction_and_sign_reach_backend_both_ways() {
    let mut fixture = Fixture::start().await;
    let backend = RecordingInput::new("record");
    fixture
        .set_input_chain(DesktopEnvironment::KDE, vec![Box::new(backend.clone())])
        .await;

    for (dx, dy) in [(0, 3), (0, -3), (2, 0), (-2, 0), (1, -1)] {
        let result = fixture
            .call_ok(method::INPUT_SEND, Some(scroll_params(Some(dx), Some(dy))))
            .await;
        assert_eq!(result.get("ok"), Some(&json!(true)));
    }
    assert_eq!(
        backend.calls(),
        vec![
            RecordedOp::Scroll(0, 3),
            RecordedOp::Scroll(0, -3),
            RecordedOp::Scroll(2, 0),
            RecordedOp::Scroll(-2, 0),
            RecordedOp::Scroll(1, -1),
        ],
        "scroll deltas must pass through with their sign and axis"
    );

    // 缺省分量：0 且不是错误（滚动命令不要求显式方向）。
    backend.clear();
    let result = fixture
        .call_ok(method::INPUT_SEND, Some(scroll_params(None, None)))
        .await;
    assert_eq!(result.get("ok"), Some(&json!(true)));
    assert_eq!(backend.calls(), vec![RecordedOp::Scroll(0, 0)]);
}
