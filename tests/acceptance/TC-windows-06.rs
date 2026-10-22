//! TC-windows-06（AC-15，P1）：最小化与还原的状态转移。
//!
//! - 场景：`windows.op {"op":"minimize"}` 与 `{"op":"restore"}` 的往返；
//!   列表投影中的 states 集合
//! - 判据：最小化后 `windows.list` 该窗口的 `states` 含 `"Minimized"`；还原后
//!   `states` 不再含 `"Minimized"`；两次操作均返回 `{"ok":true}` 且合成器
//!   记录 `minimize:<id>` / `restore:<id>`
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-windows-06`
//!
//! 判定面是真实 `windows.op` → daemon 写前校验 → 合成器通道（脚本化）与
//! 缓存刷新后的 `windows.list` 投影。无图形会话，无环境绑定部分。

use acceptance::{window, FakeCompositor, Fixture};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::method;
use serde_json::json;

/// 窗口列表是 2s TTL 短缓存；写操作后读回前等待其过期，读到合成器现状。
async fn expire_window_cache() {
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
}

/// 读回 w-1 的 states 集合。
async fn read_states(fixture: &mut Fixture) -> Vec<String> {
    let result = fixture.call_ok(method::WINDOWS_LIST, None).await;
    let entry = result["windows"]
        .as_array()
        .expect("windows array")
        .iter()
        .find(|w| w["native_id"] == json!("w-1"))
        .expect("w-1 present");
    entry["states"]
        .as_array()
        .expect("states array")
        .iter()
        .map(|s| s.as_str().expect("state string").to_string())
        .collect()
}

#[tokio::test]
async fn minimize_adds_state_and_restore_removes_it() {
    let mut fixture = Fixture::start().await;
    let compositor =
        FakeCompositor::with_windows(vec![window("w-1", "Terminal", "konsole", 10, 20, 800, 600)]);
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    // 初始：无最小化状态。
    assert!(!read_states(&mut fixture)
        .await
        .contains(&"Minimized".to_string()));

    // 最小化：states 集合出现 "Minimized"。
    let result = fixture
        .call_ok(
            method::WINDOW_OP,
            Some(json!({"op": "minimize", "target": "w-1"})),
        )
        .await;
    assert_eq!(result, json!({"ok": true}));
    expire_window_cache().await;
    let states = read_states(&mut fixture).await;
    assert!(
        states.iter().any(|s| s == "Minimized"),
        "minimize must surface Minimized in windows.list states, got {states:?}"
    );
    assert!(
        compositor.calls().iter().any(|c| c == "minimize:w-1"),
        "op log must record minimize:w-1, got {:?}",
        compositor.calls()
    );

    // 还原：Minimized 从 states 集合移除，其余状态不受影响。
    let result = fixture
        .call_ok(
            method::WINDOW_OP,
            Some(json!({"op": "restore", "target": "w-1"})),
        )
        .await;
    assert_eq!(result, json!({"ok": true}));
    expire_window_cache().await;
    let states = read_states(&mut fixture).await;
    assert!(
        !states.iter().any(|s| s == "Minimized"),
        "restore must drop Minimized from windows.list states, got {states:?}"
    );
    assert!(
        compositor.calls().iter().any(|c| c == "restore:w-1"),
        "op log must record restore:w-1, got {:?}",
        compositor.calls()
    );
}
