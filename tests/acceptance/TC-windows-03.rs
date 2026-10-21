//! TC-windows-03（AC-12，P1）：聚焦写操作、可观测状态与未命中错误。
//!
//! - 场景：`windows.op {"op":"focus"}` 对存在的窗口生效；合成器可观测状态
//!   （focused + 调用记录）随之更新；目标不存在时显式报错
//! - 判据：返回 `{"ok":true}`；fake 的 `focused` 变为目标窗口、调用记录出现
//!   `focus:<id>`；活动窗口查询可读回该窗口；未知目标返回 1003 且文本点名
//!   窗口标识
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-windows-03`
//!
//! 判定面是真实 `windows.op` → daemon 写前校验 → 合成器通道（脚本化）。
//! 无图形会话，无环境绑定部分。

use acceptance::{window, FakeCompositor, Fixture};
use agent_shell_core::component::CompositorComponent;
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

#[tokio::test]
async fn focus_updates_desktop_state_and_missing_target_fails_with_404() {
    let mut fixture = Fixture::start().await;
    let compositor = FakeCompositor::with_windows(vec![
        window("w-1", "Terminal — Konsole", "konsole", 0, 0, 800, 600),
        window("w-2", "Files", "org.gnome.Nautilus", 10, 10, 640, 480),
    ]);
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    // 聚焦前：没有活动窗口。
    let state = compositor.state();
    assert!(state.lock().expect("fake state").focused.is_none());

    let result = fixture
        .call_ok(
            method::WINDOW_OP,
            Some(json!({"op": "focus", "target": "w-2"})),
        )
        .await;
    assert_eq!(result, json!({"ok": true}));

    // 合成器侧可观测状态：最近聚焦的窗口与后端调用记录。
    {
        let state = compositor.state();
        let desktop = state.lock().expect("fake state");
        assert_eq!(desktop.focused.as_deref(), Some("w-2"));
        assert!(
            desktop.calls.contains(&"focus:w-2".to_string()),
            "op log must record focus:w-2, got {:?}",
            desktop.calls
        );
    }

    // 活动窗口可读回：daemon 的聚焦写入落到的正是合成器通道的活动窗口查询。
    let active = compositor
        .get_active_window()
        .await
        .expect("active window query succeeds")
        .expect("a window is focused");
    assert_eq!(active.id.native_id, "w-2");

    // 协议面同样可读回被聚焦窗口的完整信息（聚焦不改变窗口元数据）。
    let info = fixture
        .call_ok(method::WINDOW_INFO, Some(json!({"target": "w-2"})))
        .await;
    assert_eq!(info["id"]["native_id"], json!("w-2"));
    assert_eq!(info["title"], json!("Files"));

    // 聚焦一次不重复入账（幂等调用记录仍只含一条 focus:w-2）。
    fixture
        .call_ok(
            method::WINDOW_OP,
            Some(json!({"op": "focus", "target": "w-1"})),
        )
        .await;
    let calls = compositor.calls();
    assert!(
        calls.iter().filter(|c| c.as_str() == "focus:w-2").count() == 1
            && calls.iter().filter(|c| c.as_str() == "focus:w-1").count() == 1,
        "each focus op must be logged exactly once, got {calls:?}"
    );

    // 未命中目标：显式 1003，文本点名窗口标识；合成器零调用。
    let calls_before = compositor.calls();
    let error = fixture
        .call_err(
            method::WINDOW_OP,
            Some(json!({"op": "focus", "target": "ghost"})),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::NotFound as i32);
    assert!(
        error.message.contains("ghost"),
        "error must name the missing window: {}",
        error.message
    );
    assert_eq!(
        compositor.calls(),
        calls_before,
        "miss must not reach the backend"
    );
}
