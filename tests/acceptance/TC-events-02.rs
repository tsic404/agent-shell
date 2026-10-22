//! TC-events-02（AC-61，P1）：事件回放的顺序、过滤与空缓冲。
//!
//! - 场景：环形缓冲写入 3 条历史事件后回放；带过滤器回放；空缓冲回放。
//! - 判据：回放按旧→新顺序返回全部事件且 `count` 等于长度；过滤器缩小集合时
//!   `count` 仍等于过滤后长度；空历史返回成功响应 `count = 0`。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-events-02`
//!
//! 环境说明：无图形会话下事件由夹具写入生产 `EventRing`（与归一化管线同一
//! 缓冲）；过滤器解析、匹配与协议投影全部走生产 `events.replay`。

use acceptance::{json_wire_name, window_closed, window_focused, window_opened, Fixture};
use agent_shell_rpc::method;
use serde_json::{json, Value};

fn event_names(replay: &Value) -> Vec<String> {
    replay["events"]
        .as_array()
        .expect("replay result carries an events array")
        .iter()
        .map(json_wire_name)
        .collect()
}

#[tokio::test]
async fn replay_preserves_order_applies_filter_and_reports_empty_history() {
    // ── 空缓冲：成功，计数 0 ──
    let mut fixture = Fixture::start().await;
    let empty = fixture.call_ok(method::EVENTS_REPLAY, None).await;
    assert_eq!(empty["count"], 0, "empty history replays zero events");
    assert!(
        empty["events"].as_array().expect("events array").is_empty(),
        "empty history must produce an empty events array: {empty}"
    );

    // ── 写入 3 条事件：回放按旧→新返回 ──
    fixture.push_history(&[window_opened("1"), window_focused("1"), window_closed("1")]);
    let replay = fixture.call_ok(method::EVENTS_REPLAY, None).await;
    assert_eq!(replay["count"], 3, "count equals the replay length");
    assert_eq!(
        event_names(&replay),
        ["WindowOpened", "WindowFocused", "WindowClosed"],
        "replay preserves oldest→newest order"
    );

    // ── 过滤器缩小集合：count 仍等于过滤后长度 ──
    let filtered = fixture
        .call_ok(
            method::EVENTS_REPLAY,
            Some(json!({ "filter": "WindowClosed" })),
        )
        .await;
    assert_eq!(
        filtered["count"], 1,
        "filtered count must equal the filtered length"
    );
    assert_eq!(event_names(&filtered), ["WindowClosed"]);

    let category = fixture
        .call_ok(method::EVENTS_REPLAY, Some(json!({ "filter": "window" })))
        .await;
    assert_eq!(category["count"], 3);
    assert_eq!(
        event_names(&category),
        ["WindowOpened", "WindowFocused", "WindowClosed"]
    );

    // 未命中的过滤值同样是成功响应（过滤后为空，不是错误）。
    let none = fixture
        .call_ok(
            method::EVENTS_REPLAY,
            Some(json!({ "filter": "WorkspaceChanged" })),
        )
        .await;
    assert_eq!(none["count"], 0, "filter matching nothing is not an error");
    assert!(
        none["events"].as_array().expect("events array").is_empty(),
        "non-matching filter yields an empty array: {none}"
    );
}
