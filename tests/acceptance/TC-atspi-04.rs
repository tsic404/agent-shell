//! TC-atspi-04（AC-54，P1）：按 `(bus, path)` 定位后触发元素动作。
//!
//! - 场景：两个应用总线名共享同一对象路径（AT-SPI 路径只在单 bus 内唯一），
//!   先 `a11y.query` 取回元素坐标，再以查询结果的 `bus_name` + `path` 调用
//!   `a11y.action`。
//! - 判据：动作恰好到达 `(bus, path)` 指向的元素一次；使用的是路径定位而非
//!   坐标注入回退（动作日志里没有坐标，只有该二元组）。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-atspi-04`
//!
//! 环境说明：真实 `Action.DoAction` 需要运行中的应用；夹具以 `FakeA11y`
//! 记录 `(bus, path)` 作为动作到达的证据，请求仍走生产 `a11y.action` 的参数
//! 校验、二元组定位（含歧义处理）与错误映射。

use acceptance::{element, FakeA11y, Fixture};
use agent_shell_rpc::method;
use serde_json::json;

#[tokio::test]
async fn action_reaches_queried_element_by_bus_and_path_exactly_once() {
    let fake = FakeA11y::new(vec![
        element(":1.42", "/root/submit", "提交", "push button", 43, 0),
        element(":1.43", "/root/submit", "提交", "push button", 43, 0),
    ]);
    let mut fixture = Fixture::start().await;
    fixture.set_a11y(fake.clone());

    let query = fixture
        .call_ok(method::A11Y_QUERY, Some(json!({ "name": "提交" })))
        .await;
    assert_eq!(
        query["count"], 2,
        "both applications expose the same path under different buses"
    );
    let target = query["elements"]
        .as_array()
        .expect("elements array")
        .iter()
        .find(|element| element["bus_name"] == ":1.43")
        .expect("second application element is projected");
    let bus = target["bus_name"]
        .as_str()
        .expect("bus_name is a string")
        .to_string();
    let path = target["path"]
        .as_str()
        .expect("path is a string")
        .to_string();

    let result = fixture
        .call_ok(
            method::A11Y_ACTION,
            Some(json!({ "path": path, "bus": bus })),
        )
        .await;
    assert_eq!(result["ok"], true, "action result is {{ok:true}}");

    assert_eq!(
        fake.clicks(),
        vec![(Some(bus.clone()), path.clone())],
        "action must reach exactly the queried (bus, path) once"
    );
    assert!(
        !fixture.daemon().has_compositor(),
        "no compositor means no coordinate fallback channel exists"
    );
}
