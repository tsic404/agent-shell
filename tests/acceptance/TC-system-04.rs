//! TC-system-04（AC-73，P2）：触控板状态查询与设置占位。
//!
//! - 场景：`touchpad.status` 经注入的 fake PeripheralsOps 返回逐设备明细；
//!   `touchpad.set` 尚未接线。
//! - 判据：状态回执恰含 `source`/`enabled`/`natural_scroll`/`tap_to_click`/
//!   `devices` 五键，设备项含 `sysname`/`name`/`enabled`/`natural_scroll`/
//!   `tap_to_click`；无后端 → `1002`；`touchpad.set` 返回
//!   `{"status":"not_implemented","service":"touchpad.set"}` 且后端零调用。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-system-04`
//!
//! 环境限制：验收机没有可写的 KWin/gsettings 通道，外设后端由 fake 注入
//! daemon 的公开装配字段（`Daemon::peripherals`）；请求仍走真实 `dispatch`。
//! 真实 KWin D-Bus / gsettings 读取链由组件单测覆盖。

use std::sync::{Arc, Mutex};

use acceptance::Fixture;
use agent_shell_core::error::Result;
use agent_shell_core::services::{KeyboardLayouts, TouchpadDevice, TouchpadStatus};
use agent_shell_peripherals::PeripheralsOps;
use agent_shell_rpc::{method, RpcErrorCode};
use async_trait::async_trait;
use serde_json::{json, Value};

/// 记录型外设 fake：查询次数用于证明占位方法不触达后端。
struct RecordingPeripherals {
    calls: Mutex<Vec<&'static str>>,
}

impl RecordingPeripherals {
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
impl PeripheralsOps for RecordingPeripherals {
    async fn touchpad_status(&self) -> Result<TouchpadStatus> {
        self.calls.lock().unwrap().push("touchpad.status");
        Ok(TouchpadStatus {
            source: "fake-kde-kwin".into(),
            enabled: Some(true),
            natural_scroll: Some(false),
            tap_to_click: Some(true),
            devices: vec![
                TouchpadDevice {
                    sysname: Some("event7".into()),
                    name: "Fake Touchpad".into(),
                    enabled: true,
                    natural_scroll: Some(false),
                    tap_to_click: Some(true),
                },
                TouchpadDevice {
                    sysname: None,
                    name: "Secondary".into(),
                    enabled: false,
                    natural_scroll: None,
                    tap_to_click: None,
                },
            ],
        })
    }

    async fn keyboard_layouts(&self) -> Result<KeyboardLayouts> {
        self.calls.lock().unwrap().push("kbd.layout.list");
        unreachable!("TC-system-04 只查询触控板")
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
async fn touchpad_status_shape_and_set_placeholder() {
    let mut fixture = Fixture::start().await;
    let backend = Arc::new(RecordingPeripherals::new());
    fixture.daemon_mut().peripherals = Some(Arc::clone(&backend) as Arc<dyn PeripheralsOps>);

    // 1. 状态面：五个顶层字段与逐设备明细都来自后端。
    let status = fixture.call_ok(method::TOUCHPAD_STATUS, None).await;
    assert_eq!(
        keys(&status),
        vec![
            "devices".to_string(),
            "enabled".to_string(),
            "natural_scroll".to_string(),
            "source".to_string(),
            "tap_to_click".to_string()
        ]
    );
    assert_eq!(status["source"], "fake-kde-kwin");
    assert_eq!(status["enabled"], true);
    assert_eq!(status["natural_scroll"], false);
    assert_eq!(status["tap_to_click"], true);
    let devices = status["devices"]
        .as_array()
        .unwrap_or_else(|| panic!("devices must be an array, got: {status}"));
    assert_eq!(devices.len(), 2);
    assert_eq!(
        keys(&devices[0]),
        vec![
            "enabled".to_string(),
            "name".to_string(),
            "natural_scroll".to_string(),
            "sysname".to_string(),
            "tap_to_click".to_string()
        ]
    );
    assert_eq!(devices[0]["sysname"], "event7");
    assert_eq!(devices[0]["name"], "Fake Touchpad");
    assert_eq!(devices[0]["enabled"], true);
    assert_eq!(devices[1]["sysname"], Value::Null);
    assert_eq!(devices[1]["natural_scroll"], Value::Null);
    assert_eq!(backend.calls(), vec!["touchpad.status"]);

    // 2. 无后端：查询显式 1002，而不是伪造空设备。
    fixture.daemon_mut().peripherals = None;
    let error = fixture.call_err(method::TOUCHPAD_STATUS, None).await;
    assert_eq!(
        error.code,
        RpcErrorCode::BackendUnavailable as i32,
        "无外设后端时 touchpad.status 必须返回 1002，got: {error:?}"
    );

    // 3. touchpad.set 是占位：成功 + 精确回执，且不触达外设后端。
    fixture.daemon_mut().peripherals = Some(Arc::clone(&backend) as Arc<dyn PeripheralsOps>);
    let calls_before = backend.calls().len();
    let response = fixture
        .call_ok(
            method::TOUCHPAD_SET,
            Some(json!({ "device": "event7", "enabled": false })),
        )
        .await;
    assert_eq!(
        response,
        json!({ "status": "not_implemented", "service": "touchpad.set" })
    );
    assert_eq!(
        backend.calls().len(),
        calls_before,
        "touchpad.set 占位不得触达外设后端"
    );
}
