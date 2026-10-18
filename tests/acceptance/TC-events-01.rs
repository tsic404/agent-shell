//! TC-events-01（AC-60）：订阅窗口打开事件与过滤边界。
//!
//! - 场景：订阅窗口打开事件与过滤边界
//! - 判据：收到目标事件；非目标事件不打印；未知过滤值不崩溃
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-events-01`
//!
//! 本用例把验证阶段用过的 dispatch 驱动固化为随代码演进的验收形式：
//! 枚举名单点预过滤、类别名整类、回放复用同一过滤器、未知 token 被拒并给出合法
//! 取值全表。驱动方式见 `acceptance` 夹具文档（真实 `dispatch` + 真实事件枢纽）。

use acceptance::{
    delivery, json_wire_name, window_closed, window_focused, window_opened, wire_names,
    workspace_changed, Fixture,
};
use agent_shell_rpc::{method, RpcErrorCode};
use event::EventFilter;
use serde_json::{json, Value};

#[tokio::test]
async fn subscribe_filter_boundaries_and_replay() {
    let mut fixture = Fixture::start().await;
    // 观察样本：窗口类别三种事件 + 一个非同类别事件（负向面）。
    let sample = [
        window_focused("1"),
        window_opened("1"),
        window_closed("2"),
        workspace_changed(),
    ];

    // ── 场景 1：订阅过滤——枚举名单点、类别名整类、追加与缺省 ──
    let mut opened = fixture
        .subscribe(Some("WindowOpened"))
        .await
        .expect("事件枚举名必须被接受");
    assert!(!opened.id().is_empty(), "订阅必须返回订阅标识");
    fixture.publish(&sample).await;
    assert_eq!(
        wire_names(&opened.drain()),
        ["WindowOpened"],
        "枚举名只交付该事件，不得退化为所属类别的整类"
    );

    // 类别名交付整类：同一批事件下交付集严格宽于枚举名，两级语义可区分
    assert_eq!(
        delivery(&mut fixture, Some("window"), &sample).await,
        ["WindowFocused", "WindowOpened", "WindowClosed"],
        "类别名交付该类别全部事件，不越出类别"
    );

    // 逗号分隔的多个 token 各自生效（追加语义，跨类别取并集）
    assert_eq!(
        delivery(&mut fixture, Some("WindowOpened,WorkspaceChanged"), &sample).await,
        ["WindowOpened", "WorkspaceChanged"],
        "多 token 追加：每个 token 的选取各自生效"
    );

    // 缺省过滤器不缩小交付集（C-088：缺失等价于 all）
    assert_eq!(
        delivery(&mut fixture, None, &sample).await,
        [
            "WindowFocused",
            "WindowOpened",
            "WindowClosed",
            "WorkspaceChanged"
        ],
        "缺省过滤器等价于 all"
    );

    // ── 场景 2：未知过滤值被拒绝，不创建订阅、不破坏后续订阅 ──
    let subscribers_before = fixture.subscriber_count();
    let error = match fixture.subscribe(Some("NotARealEvent")).await {
        Ok(subscription) => panic!(
            "未知过滤值必须被拒绝，实际订阅成功：subscriber_id={}",
            subscription.id()
        ),
        Err(error) => error,
    };
    assert_eq!(
        error.code,
        RpcErrorCode::InvalidParams as i32,
        "拒绝形态是参数错误"
    );
    assert!(
        error.message.contains("NotARealEvent"),
        "错误信息必须点名被拒的取值：{}",
        error.message
    );
    assert_valid_values(&error.message, "订阅侧");
    assert_eq!(
        fixture.subscriber_count(),
        subscribers_before,
        "被拒绝的订阅不得留下订阅句柄"
    );
    assert_eq!(
        delivery(&mut fixture, Some("WindowOpened"), &[window_opened("9")]).await,
        ["WindowOpened"],
        "拒绝之后订阅仍可用（不崩溃）"
    );

    // 回放侧复用同一解析：未知取值同样拒绝并给出同一张全表
    let error = fixture
        .call_err(
            method::EVENTS_REPLAY,
            Some(json!({ "filter": "NotARealEvent" })),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert_valid_values(&error.message, "回放侧");

    // ── 场景 3：回放复用同一过滤器（C-087）──
    fixture.push_history(&sample);
    let replay = fixture
        .call_ok(
            method::EVENTS_REPLAY,
            Some(json!({ "filter": "WindowClosed" })),
        )
        .await;
    assert_eq!(replay["count"], 1, "计数等于过滤后长度");
    assert_eq!(
        replay_event_names(&replay),
        ["WindowClosed"],
        "枚举名回放只给出该事件，不扩展为整类"
    );

    let replay = fixture
        .call_ok(method::EVENTS_REPLAY, Some(json!({ "filter": "window" })))
        .await;
    assert_eq!(replay["count"], 3, "类别名回放给出该类别全部历史事件");
    assert_eq!(
        replay_event_names(&replay),
        ["WindowFocused", "WindowOpened", "WindowClosed"],
        "回放顺序为旧到新（C-087）"
    );
}

/// 拒绝信息必须给出合法取值全表（C-088），逐项核对。
fn assert_valid_values(message: &str, side: &str) {
    for value in EventFilter::valid_values().iter().copied() {
        assert!(
            message.contains(value),
            "{side}合法取值全表缺少 {value}：{message}"
        );
    }
}

/// 回放响应里的事件名（`events.replay` 的 `events` 数组）。
fn replay_event_names(replay: &Value) -> Vec<String> {
    replay["events"]
        .as_array()
        .expect("replay result carries events array")
        .iter()
        .map(json_wire_name)
        .collect()
}
