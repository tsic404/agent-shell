//! TC-system-05（AC-74，P2）：键盘布局列表与设置占位。
//!
//! - 场景：`kbd.layout.list` 经注入的 fake PeripheralsOps 返回布局列表；
//!   `kbd.layout.set` 尚未接线。
//! - 判据：回执恰含 `source`/`active_index`/`layouts` 三键；布局项含
//!   `index`/`layout`/`variant`/`display_name`（无变体为 null）；无后端 →
//!   `1002`；`kbd.layout.set` 返回
//!   `{"status":"not_implemented","service":"kbd.layout.set"}` 且后端零调用。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-05`
//!
//! 环境限制：验收机没有 KWin/gsettings/setxkbmap 可写通道，外设后端由 fake
//! 注入 daemon 的公开装配字段（`Daemon::peripherals`）；请求仍走真实
//! `dispatch`。真实布局解析链由组件单测覆盖。

use std::sync::{Arc, Mutex};

use acceptance::Fixture;
use agent_shell_core::error::Result;
use agent_shell_core::services::{KeyboardLayout, KeyboardLayouts, TouchpadStatus};
use agent_shell_peripherals::PeripheralsOps;
use agent_shell_rpc::{method, RpcErrorCode};
use async_trait::async_trait;
use serde_json::{json, Value};

/// 记录型外设 fake：只实现布局查询，触控板分支不可达。
struct RecordingLayouts {
    calls: Mutex<Vec<&'static str>>,
}

impl RecordingLayouts {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl PeripheralsOps for RecordingLayouts {
    async fn touchpad_status(&self) -> Result<TouchpadStatus> {
        self.calls.lock().unwrap().push("touchpad.status");
        unreachable!("TC-system-05 只查询键盘布局")
    }

    async fn keyboard_layouts(&self) -> Result<KeyboardLayouts> {
        self.calls.lock().unwrap().push("kbd.layout.list");
        Ok(KeyboardLayouts {
            source: "fake-kde-kxkbrc".into(),
            active_index: Some(1),
            layouts: vec![
                KeyboardLayout {
                    index: 0,
                    layout: "us".into(),
                    variant: None,
                    display_name: Some("English (US)".into()),
                },
                KeyboardLayout {
                    index: 1,
                    layout: "de".into(),
                    variant: Some("nodeadkeys".into()),
                    display_name: None,
                },
            ],
        })
    }
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got: {value}"))
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn keyboard_layout_list_shape_and_set_placeholder() {
    let mut fixture = Fixture::start().await;
    let backend = Arc::new(RecordingLayouts::new());
    fixture.daemon_mut().peripherals = Some(Arc::clone(&backend) as Arc<dyn PeripheralsOps>);

    // 1. 列表面：三顶层字段 + 四项布局字段。
    let layouts = fixture.call_ok(method::KBD_LAYOUT_LIST, None).await;
    assert_eq!(
        keys(&layouts),
        vec![
            "active_index".to_string(),
            "layouts".to_string(),
            "source".to_string()
        ]
    );
    assert_eq!(layouts["source"], "fake-kde-kxkbrc");
    assert_eq!(layouts["active_index"], 1);
    let items = layouts["layouts"]
        .as_array()
        .unwrap_or_else(|| panic!("layouts must be an array, got: {layouts}"));
    assert_eq!(items.len(), 2);
    assert_eq!(
        keys(&items[0]),
        vec![
            "display_name".to_string(),
            "index".to_string(),
            "layout".to_string(),
            "variant".to_string()
        ]
    );
    assert_eq!(items[0]["index"], 0);
    assert_eq!(items[0]["layout"], "us");
    assert_eq!(items[0]["variant"], Value::Null);
    assert_eq!(items[0]["display_name"], "English (US)");
    assert_eq!(items[1]["index"], 1);
    assert_eq!(items[1]["layout"], "de");
    assert_eq!(items[1]["variant"], "nodeadkeys");
    assert_eq!(items[1]["display_name"], Value::Null);
    assert_eq!(backend.calls(), vec!["kbd.layout.list"]);

    // 2. 无后端：显式 1002。
    fixture.daemon_mut().peripherals = None;
    let error = fixture.call_err(method::KBD_LAYOUT_LIST, None).await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendUnavailable as i32,
        "无外设后端时 kbd.layout.list 必须返回 1002，got: {error:?}"
    );

    // 3. kbd.layout.set 是占位：成功 + 精确回执，且不触达外设后端。
    fixture.daemon_mut().peripherals = Some(Arc::clone(&backend) as Arc<dyn PeripheralsOps>);
    let calls_before = backend.calls().len();
    let response = fixture
        .call_ok(
            method::KBD_LAYOUT_SET,
            Some(json!({ "index": 1, "layout": "de", "variant": "nodeadkeys" })),
        )
        .await;
    assert_eq!(
        response,
        json!({ "status": "not_implemented", "service": "kbd.layout.set" })
    );
    assert_eq!(
        backend.calls().len(),
        calls_before,
        "kbd.layout.set 占位不得触达外设后端"
    );
}
