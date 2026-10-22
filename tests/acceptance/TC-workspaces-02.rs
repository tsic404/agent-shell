//! TC-workspaces-02（AC-21，P1）：工作区切换与非法序号。
//!
//! - 场景：切换后当前桌面变化；目标词表按序号、原生标识、桌面名解析
//! - 判据：切换可观察；非法序号明确报错且非零退出
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-workspaces-02`
//!
//! 判定面是真实 `workspaces.switch`：夹具合成器记录激活的 `WorkspaceId`。
//! 解析优先级从 1 开始的序号 > 原生标识 > 桌面名；序号与原生标识故意错位，
//! 以证明序号优先（X11 下二者相差一）。

use acceptance::{workspace, FakeCompositor, Fixture};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

#[tokio::test]
async fn workspace_switch_resolves_number_first_and_rejects_unknown() {
    let mut fixture = Fixture::start().await;
    let compositor = FakeCompositor::new();
    // 序号 1 → 原生 id "2"（序号与原生标识错位）；序号 2 → 原生 id "1"。
    compositor.state().lock().expect("fake state").workspaces = vec![
        workspace("2", "Alpha", 1, true),
        workspace("1", "Beta", 2, false),
    ];
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    // 序号优先：输入 "1" 激活序号 1 的工作区（原生 id "2"）。
    fixture
        .call_ok(method::WORKSPACE_SWITCH, Some(json!({"id": "1"})))
        .await;
    assert_eq!(compositor.calls(), vec!["activate:2".to_string()]);
    {
        let state = compositor.state();
        let state = state.lock().expect("fake state");
        assert!(
            state
                .workspaces
                .iter()
                .find(|w| w.id.native_id == "2")
                .expect("ws")
                .is_active,
            "switch must move the active workspace"
        );
    }

    // 原生标识：输入 "1" 在序号已命中后不会再落到原生标识；用 "3" 不存在，
    // 改以桌面名 "Beta" 命中。
    fixture
        .call_ok(method::WORKSPACE_SWITCH, Some(json!({"id": "Beta"})))
        .await;
    assert_eq!(
        compositor.calls().last().map(String::as_str),
        Some("activate:1")
    );

    // 非法目标：明确 NotFound，不静默成功。
    let error = fixture
        .call_err(method::WORKSPACE_SWITCH, Some(json!({"id": "9"})))
        .await;
    assert_eq!(error.code, RpcErrorCode::NotFound as i32);
    assert!(
        error.message.contains("workspace not found: \"9\""),
        "unknown workspace must be explicit: {}",
        error.message
    );

    // 缺参：解析层拒绝。
    let error = fixture.call_err(method::WORKSPACE_SWITCH, None).await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);

    // 无合成器：明确后端不可用。
    let mut bare = Fixture::start().await;
    let error = bare
        .call_err(method::WORKSPACE_SWITCH, Some(json!({"id": "1"})))
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
}
