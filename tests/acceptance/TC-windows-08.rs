//! TC-windows-08（AC-17，P2）：窗口详情的完整字段与未命中错误。
//!
//! - 场景：`windows.info {"target": <native_id>}` 的完整 `WindowInfo` 投影；
//!   目标不存在时的错误
//! - 判据：详情包含 id/title/app_id/pid/geometry/frame_geometry/states/
//!   workspace_id/monitor_id/stacking_order/desktop_file/window_type/
//!   icon_geometry/keep_above 且取值与合成器状态一致；未知目标返回 1003，
//!   文本点名窗口标识
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-windows-08`
//!
//! 判定面是真实 `windows.info`（daemon 真实 `dispatch` + 缓存/查询路径）；
//! 窗口来源为脚本化合成器。无图形会话，无环境绑定部分。

use acceptance::{window, FakeCompositor, Fixture};
use agent_shell_core::types::{
    DesktopEnvironment, MonitorId, Rect, WindowState, WindowType, WorkspaceId,
};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::{json, Value};

/// `windows.info` 结果的完整字段集（`WindowInfo` serde 形状）。
const INFO_KEYS: [&str; 14] = [
    "id",
    "title",
    "app_id",
    "pid",
    "geometry",
    "frame_geometry",
    "states",
    "workspace_id",
    "monitor_id",
    "stacking_order",
    "desktop_file",
    "window_type",
    "icon_geometry",
    "keep_above",
];

#[tokio::test]
async fn info_returns_full_window_details_and_missing_target_is_explicit() {
    let mut fixture = Fixture::start().await;

    let mut info = window("w-1", "Editor", "org.kde.kate", 7, 8, 900, 700);
    info.pid = 4321;
    info.frame_geometry = Rect {
        x: 3,
        y: 4,
        width: 910,
        height: 710,
    };
    info.states = vec![WindowState::Maximized, WindowState::FullScreen];
    info.workspace_id = Some(WorkspaceId {
        native_id: "ws-2".into(),
        de_type: DesktopEnvironment::KDE,
    });
    info.monitor_id = Some(MonitorId {
        native_id: "DP-1".into(),
        de_type: DesktopEnvironment::KDE,
    });
    info.stacking_order = 9;
    info.desktop_file = Some("org.kde.kate.desktop".into());
    info.window_type = WindowType::Dialog;
    info.icon_geometry = Some(Rect {
        x: 1,
        y: 2,
        width: 32,
        height: 32,
    });
    info.keep_above = true;

    let compositor = FakeCompositor::with_windows(vec![info]);
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    let value = fixture
        .call_ok(method::WINDOW_INFO, Some(json!({"target": "w-1"})))
        .await;

    let object = value.as_object().expect("window info is a JSON object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = INFO_KEYS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        keys, expected,
        "windows.info must carry the full WindowInfo: {value}"
    );

    assert_eq!(value["id"], json!({"native_id": "w-1", "de_type": "KDE"}));
    assert_eq!(value["title"], json!("Editor"));
    assert_eq!(value["app_id"], json!("org.kde.kate"));
    assert_eq!(value["pid"], json!(4321));
    assert_eq!(
        value["geometry"],
        json!({"x": 7, "y": 8, "width": 900, "height": 700})
    );
    assert_eq!(
        value["frame_geometry"],
        json!({"x": 3, "y": 4, "width": 910, "height": 710})
    );
    assert_eq!(value["states"], json!(["Maximized", "FullScreen"]));
    assert_eq!(
        value["workspace_id"],
        json!({"native_id": "ws-2", "de_type": "KDE"})
    );
    assert_eq!(
        value["monitor_id"],
        json!({"native_id": "DP-1", "de_type": "KDE"})
    );
    assert_eq!(value["stacking_order"], json!(9));
    assert_eq!(value["desktop_file"], json!("org.kde.kate.desktop"));
    assert_eq!(value["window_type"], json!("Dialog"));
    assert_eq!(
        value["icon_geometry"],
        json!({"x": 1, "y": 2, "width": 32, "height": 32})
    );
    assert_eq!(value["keep_above"], json!(true));

    // 未命中：显式 1003，错误文本点名窗口标识。
    let error = fixture
        .call_err(method::WINDOW_INFO, Some(json!({"target": "ghost"})))
        .await;
    assert_eq!(error.code, RpcErrorCode::NotFound as i32);
    assert!(
        error.message.contains("window not found"),
        "not-found message must be explicit: {}",
        error.message
    );
    assert!(
        error.message.contains("ghost"),
        "not-found message must name the target: {}",
        error.message
    );

    // 缺失 target 参数是参数错误，而不是 1003。
    let error = fixture.call_err(method::WINDOW_INFO, Some(json!({}))).await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);

    // 只读查询不得改动合成器状态。
    assert!(compositor.calls().is_empty());
    let value: Value = fixture
        .call_ok(method::WINDOW_INFO, Some(json!({"target": "w-1"})))
        .await;
    assert_eq!(value["id"]["native_id"], json!("w-1"));
}
