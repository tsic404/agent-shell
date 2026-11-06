//! TC-workspaces-01（AC-20，P1）：工作区列表与单工作区边界。
//!
//! - 场景：工作区列表字段与单工作区边界
//! - 判据：字段齐全；仅一个工作区时返回单元素列表且不崩溃
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-workspaces-01`
//!
//! 判定面是真实 `workspaces.list`：夹具合成器提供工作区集，daemon 投影为拍平
//! 条目；无工作区时返回空集合（零退出语义 = RPC 成功）。

use acceptance::{workspace, FakeCompositor, Fixture, PrivateBus};
use agent_shell_compositor_mutter::{
    parse_shell_version, ExtensionRunner, GnomeEvalBridge, MutterError,
};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::{json, Value};

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

// ── GNOME 通道：扩展载荷 → daemon 投影（真机线格式） ──────────────────────
//
// 扩展的 `ListWorkspaces` 返回 JSON 数组：id 是 0 基原生序号（与窗口
// `workspace` 字段同一标识空间），number 从 1 开始，名称由扩展合成
// （Meta.Workspace 没有名称接口）。Rust 解析与 daemon 投影都不得退化为
// 缺字段/0 基序号。

/// GNOME 51.1 Wayland 真机会话 `ListWorkspaces` 的返回串（两工作区，第 1 个当前）。
const GNOME_LIST_WORKSPACES_PAYLOAD: &str = r#"[{"id":0,"name":"Workspace 1","number":1,"isActive":true},{"id":1,"name":"Workspace 2","number":2,"isActive":false}]"#;

/// mock `org.gnome.Shell.AgentShell`：只实现被测的 ListWorkspaces。
struct GnomeExtensionWorkspaces {
    payload: String,
}

#[zbus::interface(name = "org.gnome.Shell.AgentShell")]
impl GnomeExtensionWorkspaces {
    fn list_workspaces(&self) -> String {
        self.payload.clone()
    }
}

/// GNOME 通道端到端：扩展载荷经 `ExtensionRunner` 解析后，`workspaces.list`
/// 条目字段齐全（原生标识 / 名称 / 1 基序号 / 活动态）。
#[tokio::test]
async fn gnome_extension_payload_projects_complete_workspace_entries() {
    let bus = PrivateBus::start();
    let server = bus
        .serve(
            "/org/gnome/Shell/AgentShell",
            GnomeExtensionWorkspaces {
                payload: GNOME_LIST_WORKSPACES_PAYLOAD.to_string(),
            },
        )
        .await;
    acceptance::bus::claim(&server, "org.gnome.Shell.AgentShell").await;

    let conn = bus.connect().await;
    let parsed = ExtensionRunner::new(conn)
        .list_workspaces()
        .await
        .expect("GNOME extension workspace payload parses over D-Bus");
    assert_eq!(parsed.len(), 2, "captured payload carries two workspaces");
    assert_eq!(parsed[0].id.native_id, "0");
    assert_eq!(parsed[0].id.de_type, DesktopEnvironment::GNOME);
    assert_eq!(parsed[0].name, "Workspace 1");
    assert_eq!(parsed[0].number, 1);
    assert!(parsed[0].is_active);
    assert_eq!(parsed[1].id.native_id, "1");
    assert_eq!(parsed[1].number, 2);
    assert!(!parsed[1].is_active);

    let mut fixture = Fixture::start().await;
    fixture.install_compositor(
        &FakeCompositor::with_workspaces(parsed),
        DesktopEnvironment::GNOME,
    );
    let items = workspaces(fixture.call_ok(method::WORKSPACES_LIST, None).await);
    assert_eq!(
        items,
        vec![
            json!({"id": "0", "name": "Workspace 1", "number": 1, "is_active": true}),
            json!({"id": "1", "name": "Workspace 2", "number": 2, "is_active": false}),
        ],
        "GNOME workspaces must project complete entries"
    );
}

/// C-044 边界：仅一个工作区时 GNOME 通道返回单元素列表，且该项标为当前。
#[tokio::test]
async fn gnome_single_workspace_is_a_single_element_list() {
    let bus = PrivateBus::start();
    let server = bus
        .serve(
            "/org/gnome/Shell/AgentShell",
            GnomeExtensionWorkspaces {
                payload: r#"[{"id":0,"name":"Workspace 1","number":1,"isActive":true}]"#
                    .to_string(),
            },
        )
        .await;
    acceptance::bus::claim(&server, "org.gnome.Shell.AgentShell").await;

    let conn = bus.connect().await;
    let parsed = ExtensionRunner::new(conn)
        .list_workspaces()
        .await
        .expect("single-workspace payload parses");
    assert_eq!(parsed.len(), 1);

    let mut fixture = Fixture::start().await;
    fixture.install_compositor(
        &FakeCompositor::with_workspaces(parsed),
        DesktopEnvironment::GNOME,
    );
    let items = workspaces(fixture.call_ok(method::WORKSPACES_LIST, None).await);
    assert_eq!(items.len(), 1, "single workspace must stay a list");
    assert_eq!(items[0]["id"], json!("0"));
    assert_eq!(items[0]["number"], json!(1));
    assert_eq!(items[0]["is_active"], json!(true));
}

// ── GNOME 通道：Eval 回退载荷 → daemon 投影（GNOME <47） ────────────────────
//
// `org.gnome.Shell.Eval(js) -> (success, result)`；gnome-shell 在脚本返回值上
// 再做一次 `JSON.stringify`（`returnValue = JSON.stringify(eval(code))`），
// 脚本自身 stringify 后，线上 result 是带引号的 JSON 字符串。mock 必须复刻
// 这层编码，否则「生产只拿到 Value::String」的缺陷不会变红。

/// 复刻 gnome-shell 的 `Eval` 编码层：脚本返回 JSON 文本时，协议回包是这段
/// 文本的 JSON 字符串字面量（`JSON.stringify(eval(code))`）。
fn shell_eval_result(json_text: &str) -> String {
    serde_json::to_string(json_text).expect("encode shell Eval result")
}

/// mock `org.gnome.Shell` 根接口：原样返回 `(success, payload)`。
struct GnomeShellEval {
    success: bool,
    payload: String,
}

#[zbus::interface(name = "org.gnome.Shell")]
impl GnomeShellEval {
    fn eval(&self, _script: String) -> (bool, String) {
        (self.success, self.payload.clone())
    }
}

/// 在私有总线上起 Eval mock 并返回桥接；`_server` 必须与测试同寿命，
/// 否则 mock 对象随连接一起销毁。
async fn eval_bridge(
    bus: &PrivateBus,
    success: bool,
    payload: &str,
) -> (zbus::Connection, GnomeEvalBridge) {
    let server = bus
        .serve(
            "/org/gnome/Shell",
            GnomeShellEval {
                success,
                payload: payload.to_string(),
            },
        )
        .await;
    acceptance::bus::claim(&server, "org.gnome.Shell").await;
    let conn = bus.connect().await;
    (
        server,
        GnomeEvalBridge::new(conn, parse_shell_version("45.2").expect("GNOME 45")),
    )
}

/// GNOME <47 回退路径：Eval 载荷经 `GnomeEvalBridge::list_workspaces` 解析后
/// 字段齐全，并由 daemon 投影为与扩展路径相同形态。
#[tokio::test]
async fn gnome_eval_payload_projects_complete_workspace_entries() {
    let bus = PrivateBus::start();
    let (_server, bridge) = eval_bridge(
        &bus,
        true,
        &shell_eval_result(GNOME_LIST_WORKSPACES_PAYLOAD),
    )
    .await;

    let parsed = bridge
        .list_workspaces()
        .await
        .expect("GNOME Eval workspace payload parses over D-Bus");
    assert_eq!(parsed.len(), 2, "captured payload carries two workspaces");
    assert_eq!(parsed[0].id.native_id, "0");
    assert_eq!(parsed[0].id.de_type, DesktopEnvironment::GNOME);
    assert_eq!(parsed[0].name, "Workspace 1");
    assert_eq!(parsed[0].number, 1);
    assert!(parsed[0].is_active);
    assert_eq!(parsed[1].id.native_id, "1");
    assert_eq!(parsed[1].number, 2);
    assert!(!parsed[1].is_active);

    let mut fixture = Fixture::start().await;
    fixture.install_compositor(
        &FakeCompositor::with_workspaces(parsed),
        DesktopEnvironment::GNOME,
    );
    let items = workspaces(fixture.call_ok(method::WORKSPACES_LIST, None).await);
    assert_eq!(
        items,
        vec![
            json!({"id": "0", "name": "Workspace 1", "number": 1, "is_active": true}),
            json!({"id": "1", "name": "Workspace 2", "number": 2, "is_active": false}),
        ],
        "GNOME Eval workspaces must project complete entries"
    );
}

/// C-044 边界：单工作区 Eval 载荷仍解析为单元素列表，严格丢弃规则不误伤正常载荷。
#[tokio::test]
async fn gnome_eval_single_workspace_is_a_single_element_list() {
    let bus = PrivateBus::start();
    let (_server, bridge) = eval_bridge(
        &bus,
        true,
        &shell_eval_result(r#"[{"id":0,"name":"Workspace 1","number":1,"isActive":true}]"#),
    )
    .await;

    let parsed = bridge
        .list_workspaces()
        .await
        .expect("single-workspace Eval payload parses");
    assert_eq!(parsed.len(), 1, "single workspace must stay a list");
    assert_eq!(parsed[0].number, 1);
    assert!(parsed[0].is_active);

    let mut fixture = Fixture::start().await;
    fixture.install_compositor(
        &FakeCompositor::with_workspaces(parsed),
        DesktopEnvironment::GNOME,
    );
    let items = workspaces(fixture.call_ok(method::WORKSPACES_LIST, None).await);
    assert_eq!(
        items,
        vec![json!({"id": "0", "name": "Workspace 1", "number": 1, "is_active": true})]
    );
}

/// Eval 失败与非法载荷必须显式报错，不得吞成空列表冒充成功。
#[tokio::test]
async fn gnome_eval_failures_are_explicit_errors() {
    let failed = PrivateBus::start();
    let (_server, bridge) = eval_bridge(&failed, false, "").await;
    let err = bridge
        .list_workspaces()
        .await
        .expect_err("shell-side failure must surface as an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");

    let malformed = PrivateBus::start();
    let (_server, bridge) = eval_bridge(&malformed, true, "not-json").await;
    let err = bridge
        .list_workspaces()
        .await
        .expect_err("invalid Eval JSON must surface as an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");

    // 协议层是合法 JSON 字符串，但内层不是数组 → 明确失败，不是空集。
    let not_array = PrivateBus::start();
    let (_server, bridge) = eval_bridge(&not_array, true, &shell_eval_result("42")).await;
    let err = bridge
        .list_workspaces()
        .await
        .expect_err("non-array workspaces payload must surface as an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");

    // 数组里每一条都缺字段 → 全部丢弃 → 明确失败，不是静默空集（C-151）。
    let all_dropped = PrivateBus::start();
    let (_server, bridge) =
        eval_bridge(&all_dropped, true, &shell_eval_result(r#"[{"id":0}]"#)).await;
    let err = bridge
        .list_workspaces()
        .await
        .expect_err("all-incomplete workspaces payload must surface as an error");
    assert!(matches!(err, MutterError::Eval(_)), "got {err:?}");
}
