//! TC-atspi-02（AC-51、AC-53，P1）：全树查询的计数、投影与文本语义。
//!
//! - 场景：`a11y.query` 以 `all:true` 遍历注入的元素树（多个应用、多种角色，
//!   含一个 AT-SPI `text` 角色节点）。
//! - 判据：`count` 与 `elements` 长度一致且等于树中元素数；每个元素只携带
//!   文档化的六个字段（`bus_name`/`path`/`name`/`role`/`role_code`/`states`，
//!   没有 `text` 等多余字段）；`text` 角色元素的可读文本就是 `name`（可见文本），
//!   无需另一个文本字段。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-atspi-02`
//!
//! 环境说明：真实 AT-SPI 树依赖桌面应用注册，CI 不可复现；本用例以
//! `FakeA11y` 替换 `Daemon.a11y`，但请求仍走生产 `a11y.query` 的参数校验与
//! `A11yQueryResult` 协议投影，因此判定的是对外协议形状而非夹具内部类型。

use acceptance::{element, FakeA11y, Fixture};
use agent_shell_rpc::method;
use serde_json::{json, Value};

/// 协议允许的元素字段全集（六个，顺序无关）。
const ELEMENT_KEYS: [&str; 6] = ["bus_name", "path", "name", "role", "role_code", "states"];

fn elements_of(result: &Value) -> &Vec<Value> {
    result["elements"]
        .as_array()
        .expect("a11y.query result carries an elements array")
}

#[tokio::test]
async fn full_tree_query_projects_every_documented_field_and_text_name() {
    let nodes = vec![
        element(
            ":1.42",
            "/org/a11y/atspi/accessible/root/1",
            "欢迎使用验收夹具",
            "text",
            60,
            0,
        ),
        element(
            ":1.42",
            "/org/a11y/atspi/accessible/root/2",
            "确定",
            "push button",
            43,
            0x_0000_0000_0000_0001,
        ),
        element(
            ":1.43",
            "/org/a11y/atspi/accessible/root/1",
            "侧边栏",
            "panel",
            39,
            0,
        ),
        element(
            ":1.43",
            "/org/a11y/atspi/accessible/root/2",
            "文档",
            "document frame",
            69,
            0,
        ),
    ];
    let mut fixture = Fixture::start().await;
    fixture.set_a11y(FakeA11y::new(nodes.clone()));

    let result = fixture
        .call_ok(method::A11Y_QUERY, Some(json!({ "all": true })))
        .await;

    assert_eq!(
        result["count"], 4,
        "full-tree query count must equal the number of injected nodes: {result}"
    );
    let elements = elements_of(&result);
    assert_eq!(elements.len(), 4, "elements array length must equal count");

    for (index, (projected, source)) in elements.iter().zip(nodes.iter()).enumerate() {
        let object = projected
            .as_object()
            .unwrap_or_else(|| panic!("element {index} must be an object: {projected}"));
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = ELEMENT_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            keys, expected,
            "element {index} must expose exactly the six documented fields (no text field)"
        );

        assert_eq!(projected["bus_name"], source.bus_name);
        assert_eq!(projected["path"], source.path);
        assert_eq!(projected["name"], source.name);
        assert_eq!(projected["role"], source.role.name);
        assert_eq!(projected["role_code"], source.role.code);
        assert_eq!(projected["states"], source.states.0);
        assert!(
            projected["name"].is_string()
                && projected["role"].is_string()
                && projected["bus_name"].is_string()
                && projected["path"].is_string(),
            "string fields must stay strings: {projected}"
        );
        assert!(
            projected["role_code"].is_u64() && projected["states"].is_u64(),
            "numeric fields must stay numbers: {projected}"
        );
    }

    // 文本语义：AT-SPI `text` 角色的可见文本即 `name`，没有独立 `text` 字段。
    let text = elements
        .iter()
        .find(|element| element["role"] == "text")
        .expect("text-role element is projected");
    assert_eq!(
        text["name"], "欢迎使用验收夹具",
        "text-role element name must equal the visible text"
    );
    assert!(
        text.get("text").is_none(),
        "text must not be duplicated in a separate field: {text}"
    );
}
