//! TC-atspi-05（AC-55，P2）：a11y 不可用与动作失败的降级语义。
//!
//! - 场景：① 无 a11y 组件（headless）下的查询与动作；② 组件存在但元素的
//!   主动作失败（`Action.DoAction` 返回错误）。
//! - 判据：① 返回 `BackendUnavailable`（1002）且信息说明 AT-SPI 不可用；
//!   ② 返回 `BackendError`（1005）并透出动作失败原因；两类调用都立即返回，
//!   不挂起。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-atspi-05`
//!
//! 环境说明：真实 AT-SPI 动作失败需要损坏的应用树；此处注入返回错误的
//! `A11yOps` 实现作为动作后端，请求仍走生产 `a11y.action` 的错误映射。
//! 不修改共享夹具（`tests/acceptance/src/**`），失败组件只存在于本用例。

use std::sync::Arc;
use std::time::Duration;

use acceptance::{element, Fixture};
use agent_shell_a11y::{A11yOps, ElementNode};
use agent_shell_core::error::{AgentShellError, Result as A11yResult};
use agent_shell_core::types::SemanticTarget;
use agent_shell_rpc::{method, RpcErrorCode};
use async_trait::async_trait;
use serde_json::{json, Value};

/// 调用必须在时限内返回（挂起即验收失败）。
const PROMPT: Duration = Duration::from_secs(5);

/// 元素可定位但主动作失败的组件（`Action.DoAction` 错误的来源）。
struct ClickFails;

fn failing_node() -> ElementNode {
    element(":1.99", "/root/fail", "提交", "push button", 43, 0)
}

#[async_trait]
impl A11yOps for ClickFails {
    async fn locate(&self, _target: &SemanticTarget) -> A11yResult<Vec<ElementNode>> {
        Ok(vec![failing_node()])
    }

    async fn locate_by_path(&self, bus: Option<&str>, path: &str) -> A11yResult<ElementNode> {
        let node = failing_node();
        let bus_matches = match bus {
            Some(bus) => node.bus_name == bus,
            None => true,
        };
        if bus_matches && node.path == path {
            Ok(node)
        } else {
            Err(AgentShellError::WindowNotFound(format!(
                "a11y path {path} not found"
            )))
        }
    }

    async fn click(&self, _element: &ElementNode) -> A11yResult<()> {
        Err(AgentShellError::DBus(
            "Action.DoAction failed (fixture): element exposes no default action".into(),
        ))
    }
}

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

#[tokio::test]
async fn unavailable_and_failed_action_map_to_unavailable_and_backend_errors_promptly() {
    let mut fixture = Fixture::start().await;

    // ── ① 无 a11y 组件：查询与动作同为 BackendUnavailable ──
    fixture.clear_a11y();
    let query = error_within(
        &mut fixture,
        method::A11Y_QUERY,
        Some(json!({ "name": "提交" })),
    )
    .await;
    assert_eq!(
        query.code,
        RpcErrorCode::BackendUnavailable as i32,
        "missing a11y component must map to BackendUnavailable: {query:?}"
    );
    assert!(
        query.message.contains("AT-SPI unavailable"),
        "error must name the unavailable backend: {}",
        query.message
    );

    let action = error_within(
        &mut fixture,
        method::A11Y_ACTION,
        Some(json!({ "path": "/root/fail" })),
    )
    .await;
    assert_eq!(
        action.code,
        RpcErrorCode::BackendUnavailable as i32,
        "action without an a11y component must map to BackendUnavailable: {action:?}"
    );
    assert!(
        action.message.contains("AT-SPI unavailable"),
        "error must name the unavailable backend: {}",
        action.message
    );

    // ── ② 组件存在但动作失败：BackendError（1005）并带失败原因 ──
    fixture.daemon_mut().a11y = Some(Arc::new(ClickFails));
    let failure = error_within(
        &mut fixture,
        method::A11Y_ACTION,
        Some(json!({ "path": "/root/fail", "bus": ":1.99" })),
    )
    .await;
    assert_eq!(
        failure.code,
        RpcErrorCode::BackendError as i32,
        "action backend failure must map to BackendError: {failure:?}"
    );
    assert!(
        failure.message.contains("Action.DoAction"),
        "error must carry the action failure reason: {}",
        failure.message
    );
}
