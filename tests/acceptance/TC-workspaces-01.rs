//! TC-workspaces-01（AC-20，P1）：工作区列表与单工作区边界。
//!
//! - 场景：工作区列表字段与单工作区边界
//! - 判据：字段齐全；仅一个工作区时返回单元素列表且不崩溃
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-workspaces-01`
//!
//! 判定面是真实 `workspaces.list`：夹具合成器提供工作区集，daemon 投影为拍平
//! 条目；无工作区时返回空集合（零退出语义 = RPC 成功）。

use acceptance::{workspace, FakeCompositor, Fixture};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::Value;

fn workspaces(value: Value) -> Vec<Value> {
    value
        .get("workspaces")
        .and_then(Value::as_array)
        .cloned()
        .expect("workspaces array")
}

#[tokio::test]
async fn workspace_list_projects_fields_and_handles_single_workspace() {
    let mut fixture = Fixture::start().await;
    let compositor = FakeCompositor::new();
    compositor.state().lock().expect("fake state").workspaces = vec![
        workspace("0", "One", 1, false),
        workspace("1", "Two", 2, true),
    ];
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    let items = workspaces(fixture.call_ok(method::WORKSPACES_LIST, None).await);
    assert_eq!(items.len(), 2);
    for (item, expected) in items
        .iter()
        .zip([("0", "One", 1u32, false), ("1", "Two", 2u32, true)])
    {
        assert_eq!(item.get("id"), Some(&Value::from(expected.0)));
        assert_eq!(item.get("name"), Some(&Value::from(expected.1)));
        assert_eq!(item.get("number"), Some(&Value::from(expected.2)));
        assert_eq!(item.get("is_active"), Some(&Value::from(expected.3)));
        let keys: Vec<&String> = item.as_object().expect("object").keys().collect();
        assert_eq!(
            keys.len(),
            4,
            "list projection must expose exactly id/name/number/is_active: {item}"
        );
    }

    // 单工作区：单元素列表，不崩溃。
    compositor.state().lock().expect("fake state").workspaces =
        vec![workspace("0", "Only", 1, true)];
    let items = workspaces(fixture.call_ok(method::WORKSPACES_LIST, None).await);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].get("is_active"), Some(&Value::from(true)));

    // 无工作区：空集合是成功结果。
    compositor
        .state()
        .lock()
        .expect("fake state")
        .workspaces
        .clear();
    let items = workspaces(fixture.call_ok(method::WORKSPACES_LIST, None).await);
    assert!(items.is_empty());

    // 无合成器：明确的后端不可用，不返回空集合冒充成功。
    let mut bare = Fixture::start().await;
    let error = bare.call_err(method::WORKSPACES_LIST, None).await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
    assert!(
        error.message.contains("compositor channel unavailable"),
        "missing compositor must be explicit: {}",
        error.message
    );
}
