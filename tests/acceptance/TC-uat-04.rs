//! TC-uat-04（AC-116，P0）：真实应用上的用户目标旅程——读屏点按一环。
//!
//! - 场景：私有 session bus 上注册最小 AT-SPI 树（应用 → 窗口 → 四个元素），
//!   同一会话内先 `a11y.query` 取回元素，再把查询结果的 `(bus, path)` 直接
//!   交给 `a11y.action` 点按。元素分别覆盖 C-074 / C-103 的分支：
//!   无 Action 接口（几何降级）、无 Action 且无可用几何、有 Action 接口但
//!   `DoAction` 返 false 且无可用几何、有 Action 接口且动作成功、授权被拒。
//! - 判据：
//!   ① 无动作接口的元素按几何降级为坐标点击——注入落在元素中心，动作成功；
//!   ② 有动作接口的元素走主动作，不触发坐标注入（既有路径不变）；
//!   ③ 元素没有可用几何时报「元素无法点击」类错误（BackendError），文案不得
//!      归因到指针后端；
//!   ④ 元素有动作接口但 `DoAction` 返 false 且无可用几何时，文案按真实原因
//!      成句，不得说成「没有动作接口」；
//!   ⑤ 会话没有指针后端时报 BackendUnavailable 并透出装配根因，与 ③ 分属
//!      两种原因，且不产生任何注入；
//!   ⑥ 指针通道建立被拒（授权类失败）时报 Denied（1004），与 `input.send`
//!      同码，不得落到 BackendError（1005）。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-uat-04`
//!
//! 环境说明：真实 AT-SPI 树需要桌面应用注册到 a11y bus，CI 无法复现；夹具在
//! 私有 dbus-daemon 上提供与生产探测链同源的坐标（launcher、Registry 根、
//! Accessible/Component/Action 接口），被测组件仍是生产 `AtSpiComponent`，
//! 请求走真实 `dispatch`，坐标注入走真实输入降级链（记录型后端）。

use std::time::Duration;

use acceptance::bus::{claim, A11yStatus, A11Y_BUS_SERVICE, A11Y_STATUS_PATH};
use acceptance::{
    serve_application, serve_registry, Fixture, NodeSpec, PrivateBus, RecordedOp, RecordingInput,
    SetupBehaviour,
};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::{json, Value};

/// 单次调用必须在时限内返回（挂起即验收失败）。
const PROMPT: Duration = Duration::from_secs(10);

/// 应用根（`Application` 接口，桥接据此把它识别为应用）。
const APP_PATH: &str = "/org/example/App";
/// 顶层窗口（角色 23 = FRAME）。
const WINDOW_PATH: &str = "/org/example/App/Window";
/// 无 Action 接口的按钮：坐标降级的目标。
const COPY_PATH: &str = "/org/example/App/Window/Copy";
/// 无 Action 接口且包围盒为空的按钮：降级不可用。
const GHOST_PATH: &str = "/org/example/App/Window/Ghost";
/// 有 Action 接口的按钮：主动作路径（行为不得变化）。
const APPLY_PATH: &str = "/org/example/App/Window/Apply";
/// 有 Action 接口但 `DoAction` 返 false、且包围盒为空的按钮：文案按真实原因成句。
const REFUSE_PATH: &str = "/org/example/App/Window/Refuse";

/// 元素树：窗口下挂四个按钮，覆盖 C-074 / C-103 的分支。
const TREE: &[NodeSpec] = &[
    NodeSpec {
        path: APP_PATH,
        name: "Text Editor",
        role: 75,
        role_name: "application",
        children: &[WINDOW_PATH],
        extents: None,
        action: None,
        is_application: true,
    },
    NodeSpec {
        path: WINDOW_PATH,
        name: "Text Editor",
        role: 23,
        role_name: "frame",
        children: &[COPY_PATH, GHOST_PATH, APPLY_PATH, REFUSE_PATH],
        extents: Some((0, 0, 800, 600)),
        action: None,
        is_application: false,
    },
    NodeSpec {
        path: COPY_PATH,
        name: "Copy",
        role: 43,
        role_name: "push button",
        children: &[],
        extents: Some((100, 50, 40, 20)),
        action: None,
        is_application: false,
    },
    NodeSpec {
        path: GHOST_PATH,
        name: "Ghost",
        role: 43,
        role_name: "push button",
        children: &[],
        extents: Some((0, 0, 0, 0)),
        action: None,
        is_application: false,
    },
    NodeSpec {
        path: APPLY_PATH,
        name: "Apply",
        role: 43,
        role_name: "push button",
        children: &[],
        extents: Some((200, 50, 40, 20)),
        action: Some(true),
        is_application: false,
    },
    NodeSpec {
        path: REFUSE_PATH,
        name: "Refuse",
        role: 43,
        role_name: "push button",
        children: &[],
        extents: Some((0, 0, 0, 0)),
        action: Some(false),
        is_application: false,
    },
];

/// 有界调用：返回 RPC 错误并要求结果为空。
async fn error_within(
    fixture: &mut Fixture,
    method_name: &str,
    params: Option<Value>,
) -> agent_shell_rpc::RpcError {
    let response = tokio::time::timeout(PROMPT, fixture.call(method_name, params))
        .await
        .unwrap_or_else(|_| panic!("{method_name} did not return within {PROMPT:?}"));
    assert!(
        response.result.is_none(),
        "{method_name} must fail, not return a result"
    );
    response
        .error
        .unwrap_or_else(|| panic!("{method_name} returned neither result nor error"))
}

/// `a11y.query --name`：取回唯一命中元素的 `(bus_name, path)`。
async fn locate(fixture: &mut Fixture, name: &str) -> (String, String) {
    let query = fixture
        .call_ok(method::A11Y_QUERY, Some(json!({ "name": name })))
        .await;
    assert_eq!(
        query["count"], 1,
        "query by name must hit exactly one element ({name}): {query}"
    );
    let element = &query["elements"][0];
    (
        element["bus_name"]
            .as_str()
            .expect("bus_name is a string")
            .to_string(),
        element["path"]
            .as_str()
            .expect("path is a string")
            .to_string(),
    )
}

#[tokio::test]
async fn a11y_click_falls_back_to_coordinates_and_separates_failure_causes() {
    let bus = PrivateBus::start();
    // 会话启用位：已启用 → 桥接不写总线，直接枚举树（读屏软件启动后的常态）。
    let launcher = bus.serve(A11Y_STATUS_PATH, A11yStatus::new(true)).await;
    claim(&launcher, A11Y_BUS_SERVICE).await;
    // 连接必须存活到用例结束：对象随连接注销（`_` 前缀的绑定不提前 drop）。
    let (_app, app_bus, actions) = serve_application(&bus, TREE).await;
    let _registry = serve_registry(&bus, &app_bus, APP_PATH).await;

    let mut fixture = Fixture::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string()),
        ("AT_SPI_BUS_ADDRESS", bus.address().to_string()),
    ])
    .await;
    // 已装配的输入链：与 `input.send` 同一条降级链，记录实际注入。
    let input = RecordingInput::new("uinput");
    fixture
        .set_input_chain(
            DesktopEnvironment::GNOME,
            vec![Box::new(input.clone()) as Box<dyn agent_shell_input::InputService>],
        )
        .await;

    // ── ① 无 Action 接口：几何降级为坐标点击，落在元素中心 ──
    let (copy_bus, copy_path) = locate(&mut fixture, "Copy").await;
    assert_eq!(copy_path, COPY_PATH, "query must return the served element");
    let clicked = fixture
        .call_ok(
            method::A11Y_ACTION,
            Some(json!({ "path": copy_path, "bus": copy_bus })),
        )
        .await;
    assert_eq!(clicked["ok"], true, "fallback click reports success");
    assert_eq!(
        input.calls(),
        vec![
            RecordedOp::Move(120, 60),
            RecordedOp::Click(agent_shell_core::types::MouseButton::Left)
        ],
        "coordinate fallback must move to the element centre and click the left button"
    );

    // ── ② 有 Action 接口：主动作路径不变，不触发坐标注入 ──
    input.clear();
    let (apply_bus, apply_path) = locate(&mut fixture, "Apply").await;
    fixture
        .call_ok(
            method::A11Y_ACTION,
            Some(json!({ "path": apply_path, "bus": apply_bus })),
        )
        .await;
    assert_eq!(
        actions[0].calls(),
        1,
        "element with an action interface must dispatch its primary action exactly once"
    );
    assert!(
        input.calls().is_empty(),
        "action interface path must not fall back to coordinate injection: {:?}",
        input.calls()
    );

    // ── ③ 无 Action 且无可用几何：报元素无法点击，不归因指针后端 ──
    input.clear();
    let (ghost_bus, ghost_path) = locate(&mut fixture, "Ghost").await;
    let geometry = error_within(
        &mut fixture,
        method::A11Y_ACTION,
        Some(json!({ "path": ghost_path, "bus": ghost_bus })),
    )
    .await;
    assert_eq!(
        geometry.code,
        RpcErrorCode::BackendError as i32,
        "unusable element geometry must be a backend error: {geometry:?}"
    );
    assert!(
        geometry.message.contains("no usable geometry") && geometry.message.contains(GHOST_PATH),
        "error must name the element and the missing geometry: {}",
        geometry.message
    );
    assert!(
        !geometry.message.contains("pointer input backend"),
        "element geometry failure must not be blamed on the pointer backend: {}",
        geometry.message
    );
    assert!(
        input.calls().is_empty(),
        "a rejected fallback must not inject anything: {:?}",
        input.calls()
    );

    // ── ④ 有 Action 但 DoAction 返 false 且无可用几何：按真实原因成句 ──
    input.clear();
    let (refuse_bus, refuse_path) = locate(&mut fixture, "Refuse").await;
    let refused = error_within(
        &mut fixture,
        method::A11Y_ACTION,
        Some(json!({ "path": refuse_path, "bus": refuse_bus })),
    )
    .await;
    assert_eq!(
        refused.code,
        RpcErrorCode::BackendError as i32,
        "a refused primary action on an unclickable element must be a backend error: {refused:?}"
    );
    assert_eq!(
        actions[1].calls(),
        1,
        "the primary action must be attempted exactly once before falling back"
    );
    assert!(
        refused.message.contains("no usable geometry") && refused.message.contains(REFUSE_PATH),
        "error must name the element and the missing geometry: {}",
        refused.message
    );
    assert!(
        refused
            .message
            .contains("primary AT-SPI action returned false"),
        "error must carry the actual fallback cause: {}",
        refused.message
    );
    assert!(
        !refused.message.contains("no AT-SPI action interface"),
        "a refused primary action must not be reported as a missing action interface: {}",
        refused.message
    );
    assert!(
        input.calls().is_empty(),
        "a rejected fallback must not inject anything: {:?}",
        input.calls()
    );

    // ── ⑤ 会话没有指针后端：BackendUnavailable + 装配根因，与 ③ 分属两种原因 ──
    fixture.set_input_unavailable(Some("portal lacks ConnectToEIS"));
    let no_backend = error_within(
        &mut fixture,
        method::A11Y_ACTION,
        Some(json!({ "path": COPY_PATH, "bus": copy_bus })),
    )
    .await;
    assert_eq!(
        no_backend.code,
        RpcErrorCode::BackendUnavailable as i32,
        "missing pointer backend must be BackendUnavailable: {no_backend:?}"
    );
    assert!(
        no_backend
            .message
            .contains("pointer input backend unavailable")
            && no_backend.message.contains("ConnectToEIS"),
        "error must name the pointer backend and its assembly cause: {}",
        no_backend.message
    );
    assert!(
        !no_backend.message.contains("no usable geometry"),
        "missing pointer backend must not be reported as an element geometry problem: {}",
        no_backend.message
    );
    assert!(
        input.calls().is_empty(),
        "no injection may happen without a pointer backend: {:?}",
        input.calls()
    );

    // ── ⑥ 通道建立被拒：Denied(1004)，与 input.send 同码，不落 1005 ──
    // 单后端 libei + 授权被拒：dispatcher 在候选全部建立失败后原样透出 Permission。
    let denied_input = RecordingInput::new("libei").with_setup(SetupBehaviour::Permission);
    fixture
        .set_input_chain(
            DesktopEnvironment::GNOME,
            vec![Box::new(denied_input.clone()) as Box<dyn agent_shell_input::InputService>],
        )
        .await;
    let denied = error_within(
        &mut fixture,
        method::A11Y_ACTION,
        Some(json!({ "path": COPY_PATH, "bus": copy_bus })),
    )
    .await;
    assert_eq!(
        denied.code,
        RpcErrorCode::Denied as i32,
        "a refused pointer authorization must be Denied(1004), not BackendError(1005): {denied:?}"
    );
    assert!(
        denied.message.contains("authorization denied"),
        "error must carry the authorization cause: {}",
        denied.message
    );
    // 同一会话、同一底层失败：`input.send` 的码必须与 a11y 降级一致。
    // 失败候选已在上一调用内被摘除（active → None），故重置同一条链再发一次
    // ——比较的是同一会话里同一类失败的归一结果，不是降级链的持久状态。
    fixture
        .set_input_chain(
            DesktopEnvironment::GNOME,
            vec![Box::new(denied_input.clone()) as Box<dyn agent_shell_input::InputService>],
        )
        .await;
    let send_denied = error_within(
        &mut fixture,
        method::INPUT_SEND,
        Some(json!({"kind": "click", "payload": {"button": "left", "at": null}})),
    )
    .await;
    assert_eq!(
        send_denied.code,
        RpcErrorCode::Denied as i32,
        "input.send must report the same permission failure as Denied(1004): {send_denied:?}"
    );
    assert_eq!(
        denied.code, send_denied.code,
        "a11y coordinate fallback and input.send must normalize the same permission failure to the same code"
    );
    assert!(
        denied_input.calls().is_empty(),
        "a channel that never became ready must not inject anything: {:?}",
        denied_input.calls()
    );
}
