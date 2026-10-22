//! TC-atspi-03（AC-52，P1）：语义定位的别名归一、组合过滤与零命中。
//!
//! - 场景：元素树含 `push button` 角色（规范名）与同名文本/面板节点；
//!   以 `button`（GTK 方言）与 `push button` 两种写法、单独与组合条件查询。
//! - 判据：角色别名 `button` ↔ `push button` 等价；`role + name` 组合过滤取
//!   交集且只返回同时命中的元素；零命中返回成功响应 `count = 0`（不是错误）。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-atspi-03`
//!
//! 环境说明：真实 AT-SPI 的 `GetRoleName` 方言差异（规范 "push button" 与
//! GTK "button"）由 `FakeA11y` 内 `AtspiRole::matches_name` 的生产别名表吸收；
//! 夹具只替代树来源，过滤与投影仍走生产 `a11y.query`。

use acceptance::{element, FakeA11y, Fixture};
use agent_shell_rpc::method;
use serde_json::{json, Value};

fn paths_of(result: &Value) -> Vec<String> {
    result["elements"]
        .as_array()
        .expect("a11y.query result carries an elements array")
        .iter()
        .map(|element| {
            element["path"]
                .as_str()
                .expect("element path is a string")
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn role_alias_and_combined_filters_locate_elements_without_error_on_empty() {
    let mut fixture = Fixture::start().await;
    fixture.set_a11y(FakeA11y::new(vec![
        element(":1.42", "/root/button-ok", "确定", "push button", 43, 0),
        element(":1.42", "/root/button-cancel", "取消", "push button", 43, 0),
        element(":1.42", "/root/text-ok", "确定", "text", 60, 0),
        element(":1.43", "/root/panel-ok", "确定", "panel", 39, 0),
    ]));

    // 别名：调用方写 GTK 方言 `button`，元素自报规范名 `push button`。
    let result = fixture
        .call_ok(
            method::A11Y_QUERY,
            Some(json!({ "role": "button", "name": "确定" })),
        )
        .await;
    assert_eq!(
        result["count"], 1,
        "alias role + name must match one element"
    );
    assert_eq!(
        paths_of(&result),
        ["/root/button-ok"],
        "alias must select the push-button element, not the same-named text/panel"
    );

    // 规范名同样命中同一元素（两种方言等价）。
    let canonical = fixture
        .call_ok(
            method::A11Y_QUERY,
            Some(json!({ "role": "push button", "name": "确定" })),
        )
        .await;
    assert_eq!(canonical["count"], 1);
    assert_eq!(paths_of(&canonical), paths_of(&result));

    // 组合过滤是交集：单条件各自更宽，合起来严格收窄。
    let role_only = fixture
        .call_ok(method::A11Y_QUERY, Some(json!({ "role": "button" })))
        .await;
    assert_eq!(
        role_only["count"], 2,
        "role-only query returns both buttons"
    );
    let name_only = fixture
        .call_ok(method::A11Y_QUERY, Some(json!({ "name": "确定" })))
        .await;
    assert_eq!(
        name_only["count"], 3,
        "name-only query spans roles (button, text, panel)"
    );

    // 零命中：成功响应，count 0、空数组，而不是 NotFound/InvalidParams。
    let empty = fixture
        .call_ok(
            method::A11Y_QUERY,
            Some(json!({ "role": "button", "name": "不存在的标签" })),
        )
        .await;
    assert_eq!(empty["count"], 0, "zero hits is a successful query");
    assert!(
        empty["elements"]
            .as_array()
            .expect("elements array")
            .is_empty(),
        "zero-hit query must return an empty elements array: {empty}"
    );
}
