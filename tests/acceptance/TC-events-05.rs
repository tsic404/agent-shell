//! TC-events-05（AC-107，P1）：事件载荷形状与 RPC 契约。
//!
//! - 场景：构造覆盖窗口/工作区/监视器/输入/系统类别的代表性 `DesktopEvent`，
//!   逐一序列化；随后写入回放缓冲并订阅，检查未知过滤器。
//! - 判据：每个事件都是外部标签对象，载荷含 `source`（字符串）与
//!   `occurred_at`（毫秒数值），各变体字段名与类型符合设计载荷；回放计数等于
//!   事件数；未知过滤值被拒且错误信息列出全部合法取值；订阅响应只有
//!   `subscriber_id`（通知不在 RPC 响应里）。
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-events-05`
//!
//! 环境说明：事件由夹具写入生产 `EventRing` / `EventHub`，序列化与过滤均走
//! 生产代码；无图形会话不产生真实合成器事件，故不判定来源事件的内容，只
//! 判定协议载荷与过滤契约。

use std::time::Instant;

use acceptance::{window, wire_names, Fixture};
use agent_shell_core::services::{AccessibilityChange, PowerState};
use agent_shell_core::types::{
    DesktopEnvironment, Key, KeyCombo, ModifierMask, MonitorId, MonitorInfo, MouseButton, Rect,
    WindowId, WindowState, WorkspaceId, WorkspaceInfo,
};
use agent_shell_rpc::{method, RpcErrorCode};
use event::{DesktopEvent, EventFilter, EventSource};
use serde_json::{json, Value};

fn wid(native_id: &str) -> WindowId {
    WindowId {
        native_id: native_id.into(),
        de_type: DesktopEnvironment::KDE,
    }
}

fn wsid(native_id: &str) -> WorkspaceId {
    WorkspaceId {
        native_id: native_id.into(),
        de_type: DesktopEnvironment::KDE,
    }
}

fn rect() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    }
}

fn monitor() -> MonitorInfo {
    MonitorInfo {
        id: MonitorId {
            native_id: "eDP-1".into(),
            de_type: DesktopEnvironment::KDE,
        },
        name: "eDP-1".into(),
        geometry: rect(),
        physical_geometry: Rect {
            width: 1920,
            height: 1080,
            ..rect()
        },
        scale: 1.0,
        is_primary: true,
        workspace_id: Some(wsid("1")),
    }
}

fn workspace_info() -> WorkspaceInfo {
    WorkspaceInfo {
        id: wsid("1"),
        name: "1".into(),
        number: 1,
        is_active: true,
        monitor_ids: vec![MonitorId {
            native_id: "eDP-1".into(),
            de_type: DesktopEnvironment::KDE,
        }],
        window_ids: vec![wid("1")],
    }
}

fn events() -> Vec<DesktopEvent> {
    let now = Instant::now;
    let win = window("1", "Acceptance", "acceptance", 0, 0, 800, 600);
    vec![
        DesktopEvent::WindowOpened {
            info: win.clone(),
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WindowClosed {
            id: wid("1"),
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WindowFocused {
            info: win,
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WindowMoved {
            id: wid("1"),
            geometry: rect(),
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WindowStateChanged {
            id: wid("1"),
            states: vec![WindowState::Maximized],
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WindowMetadataChanged {
            id: wid("1"),
            title: Some("Renamed".into()),
            app_id: None,
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WindowStackingChanged {
            ids: vec![wid("1")],
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WorkspaceChanged {
            info: workspace_info(),
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WorkspaceListChanged {
            workspaces: vec![workspace_info()],
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::WorkspaceWindowMoved {
            window_id: wid("1"),
            from: None,
            to: Some(wsid("2")),
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::MonitorHotplug {
            monitor: monitor(),
            added: true,
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::MonitorChanged {
            info: monitor(),
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::PointerButton {
            window_id: Some(wid("1")),
            button: MouseButton::Left,
            pressed: true,
            position: (10, 20),
            source: EventSource::Input,
            occurred_at: now(),
        },
        DesktopEvent::KeyComboPressed {
            combo: KeyCombo {
                keys: vec![Key::Char('k')],
                modifiers: ModifierMask::CTRL,
            },
            source: EventSource::Input,
            occurred_at: now(),
        },
        DesktopEvent::AppLaunched {
            app_id: "org.example.App".into(),
            pid: 4242,
            desktop_file: Some("org.example.App.desktop".into()),
            source: EventSource::Portal,
            occurred_at: now(),
        },
        DesktopEvent::AppExited {
            pid: 4242,
            source: EventSource::Portal,
            occurred_at: now(),
        },
        DesktopEvent::FullscreenChanged {
            enabled: true,
            window_id: Some(wid("1")),
            source: EventSource::KWinWayland,
            occurred_at: now(),
        },
        DesktopEvent::PowerStateChanged {
            state: PowerState::Suspend,
            source: EventSource::Power,
            occurred_at: now(),
        },
        DesktopEvent::AccessibilityTreeChanged {
            app_pid: 4242,
            change_type: AccessibilityChange::NodeAdded,
            source: EventSource::AtSpi,
            occurred_at: now(),
        },
    ]
}

fn field<'a>(payload: &'a Value, name: &str, variant: &str) -> &'a Value {
    payload
        .get(name)
        .unwrap_or_else(|| panic!("{variant} payload is missing {name:?}: {payload}"))
}

fn expect_source(payload: &Value, variant: &str, source: &str) {
    assert_eq!(
        field(payload, "source", variant),
        source,
        "{variant} must carry the event source label"
    );
}

fn expect_object(value: &Value, what: &str) {
    assert!(value.is_object(), "{what} must be an object: {value}");
}

fn expect_array(value: &Value, what: &str) {
    assert!(value.is_array(), "{what} must be an array: {value}");
}

#[tokio::test]
async fn event_payloads_are_complete_and_subscribe_response_is_minimal() {
    let events = events();
    let encoded: Vec<(String, Value)> = events
        .iter()
        .map(|event| {
            let value = serde_json::to_value(event).expect("DesktopEvent is serializable");
            let map = value.as_object().expect("externally tagged event object");
            assert_eq!(
                map.len(),
                1,
                "exactly one externally tagged variant: {value}"
            );
            let (variant, payload) = map.iter().next().expect("variant entry");
            (variant.clone(), payload.clone())
        })
        .collect();
    assert_eq!(
        encoded.len(),
        events.len(),
        "each event serializes to one externally tagged object"
    );

    for (variant, payload) in &encoded {
        expect_object(payload, &format!("{variant} payload"));
        // 公共字段：来源与发生时间（进程内毫秒偏移）。
        assert!(
            field(payload, "source", variant).is_string(),
            "{variant}.source must be a string"
        );
        assert!(
            field(payload, "occurred_at", variant).is_u64(),
            "{variant}.occurred_at must serialize as an integer millisecond offset"
        );

        match variant.as_str() {
            "WindowOpened" | "WindowFocused" => {
                expect_source(payload, variant, "KWinWayland");
                let info = field(payload, "info", variant);
                expect_object(info, &format!("{variant}.info"));
                for name in [
                    "id",
                    "title",
                    "app_id",
                    "pid",
                    "geometry",
                    "frame_geometry",
                    "states",
                ] {
                    field(info, name, variant);
                }
                expect_object(field(info, "id", variant), "window id");
                expect_array(field(info, "states", variant), "window states");
            }
            "WindowClosed" => {
                expect_source(payload, variant, "KWinWayland");
                let id = field(payload, "id", variant);
                expect_object(id, "WindowClosed.id");
                assert!(field(id, "native_id", variant).is_string());
                assert!(field(id, "de_type", variant).is_string());
            }
            "WindowMoved" => {
                expect_source(payload, variant, "KWinWayland");
                expect_object(field(payload, "id", variant), "WindowMoved.id");
                expect_object(field(payload, "geometry", variant), "WindowMoved.geometry");
            }
            "WindowStateChanged" => {
                expect_source(payload, variant, "KWinWayland");
                let states = field(payload, "states", variant);
                expect_array(states, "WindowStateChanged.states");
                assert_eq!(states, &json!(["Maximized"]));
            }
            "WindowMetadataChanged" => {
                expect_source(payload, variant, "KWinWayland");
                assert!(field(payload, "title", variant).is_string());
                assert!(field(payload, "app_id", variant).is_null());
            }
            "WindowStackingChanged" => {
                expect_source(payload, variant, "KWinWayland");
                let ids = field(payload, "ids", variant);
                expect_array(ids, "WindowStackingChanged.ids");
                assert_eq!(ids.as_array().expect("ids").len(), 1);
            }
            "WorkspaceChanged" => {
                expect_source(payload, variant, "KWinWayland");
                expect_object(field(payload, "info", variant), "WorkspaceChanged.info");
            }
            "WorkspaceListChanged" => {
                expect_source(payload, variant, "KWinWayland");
                let list = field(payload, "workspaces", variant);
                expect_array(list, "WorkspaceListChanged.workspaces");
                assert_eq!(list.as_array().expect("workspaces").len(), 1);
            }
            "WorkspaceWindowMoved" => {
                expect_source(payload, variant, "KWinWayland");
                expect_object(field(payload, "window_id", variant), "window_id");
                assert!(field(payload, "from", variant).is_null());
                expect_object(field(payload, "to", variant), "to workspace");
            }
            "MonitorHotplug" => {
                expect_source(payload, variant, "KWinWayland");
                expect_object(field(payload, "monitor", variant), "MonitorHotplug.monitor");
                assert_eq!(field(payload, "added", variant), &json!(true));
            }
            "MonitorChanged" => {
                expect_source(payload, variant, "KWinWayland");
                expect_object(field(payload, "info", variant), "MonitorChanged.info");
            }
            "PointerButton" => {
                expect_source(payload, variant, "Input");
                expect_object(field(payload, "window_id", variant), "window_id");
                assert_eq!(field(payload, "button", variant), &json!("Left"));
                assert_eq!(field(payload, "pressed", variant), &json!(true));
                assert_eq!(field(payload, "position", variant), &json!([10, 20]));
            }
            "KeyComboPressed" => {
                expect_source(payload, variant, "Input");
                let combo = field(payload, "combo", variant);
                expect_object(combo, "KeyComboPressed.combo");
                expect_array(field(combo, "keys", variant), "combo.keys");
                assert_eq!(field(combo, "modifiers", variant)["ctrl"], json!(true));
            }
            "AppLaunched" => {
                expect_source(payload, variant, "Portal");
                assert!(field(payload, "app_id", variant).is_string());
                assert!(field(payload, "pid", variant).is_u64());
                assert!(field(payload, "desktop_file", variant).is_string());
            }
            "AppExited" => {
                expect_source(payload, variant, "Portal");
                assert!(field(payload, "pid", variant).is_u64());
            }
            "FullscreenChanged" => {
                expect_source(payload, variant, "KWinWayland");
                assert_eq!(field(payload, "enabled", variant), &json!(true));
                expect_object(field(payload, "window_id", variant), "window_id");
            }
            "PowerStateChanged" => {
                expect_source(payload, variant, "Power");
                assert_eq!(field(payload, "state", variant), &json!("Suspend"));
            }
            "AccessibilityTreeChanged" => {
                expect_source(payload, variant, "AtSpi");
                assert!(field(payload, "app_pid", variant).is_u64());
                assert_eq!(field(payload, "change_type", variant), &json!("NodeAdded"));
            }
            other => panic!("unexpected representative event variant: {other}"),
        }
    }

    // ── 回放：计数与顺序对齐同一组事件 ──
    let mut fixture = Fixture::start().await;
    fixture.push_history(&events);
    let replay = fixture.call_ok(method::EVENTS_REPLAY, None).await;
    assert_eq!(
        replay["count"],
        events.len(),
        "replay count must equal the number of pushed events"
    );
    let replayed: Vec<String> = replay["events"]
        .as_array()
        .expect("events array")
        .iter()
        .map(|event| {
            event
                .as_object()
                .expect("externally tagged object")
                .keys()
                .next()
                .expect("variant key")
                .clone()
        })
        .collect();
    assert_eq!(
        replayed,
        wire_names(&events),
        "replay order is oldest→newest"
    );

    // ── 未知过滤值：拒绝并列出合法取值全表 ──
    let error = fixture
        .call_err(
            method::EVENTS_REPLAY,
            Some(json!({ "filter": "NotARealEvent" })),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    for value in EventFilter::valid_values().iter().copied() {
        assert!(
            error.message.contains(value),
            "valid-values list is missing {value}: {}",
            error.message
        );
    }

    // ── 订阅响应只有 subscriber_id（通知走独立通道） ──
    let response = fixture.call(method::EVENTS_SUBSCRIBE, None).await;
    assert!(
        response.error.is_none(),
        "subscribe must succeed: {:?}",
        response.error
    );
    let result = response.result.expect("subscribe result");
    let object = result.as_object().expect("subscribe result is an object");
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        ["subscriber_id"],
        "subscribe response must not embed events/notifications: {result}"
    );
    assert!(
        object["subscriber_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "subscriber_id is a non-empty string: {result}"
    );
}
