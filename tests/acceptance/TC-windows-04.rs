//! TC-windows-04（AC-13，P1）：移动写操作与几何读回，含边界坐标。
//!
//! - 场景：`windows.op {"op":"move"}` 正向移动；负坐标与超大坐标边界
//! - 判据：正向移动后 `windows.list` 读回的 x/y 与目标一致，合成器记录
//!   `move:<id>:<x>,<y>`；负坐标与超大坐标原样转发到后端并读回（协议不做
//!   钳制，合成器如实接收），进程不崩溃
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-windows-04`
//!
//! 判定面是真实 `windows.op` → daemon 写前校验 → 合成器通道（脚本化）与
//! 缓存刷新后的 `windows.list` 读回。无图形会话，无环境绑定部分。

use acceptance::{window, FakeCompositor, Fixture};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::method;
use serde_json::json;

async fn move_to(fixture: &mut Fixture, x: i32, y: i32) {
    let result = fixture
        .call_ok(
            method::WINDOW_OP,
            Some(json!({"op": "move", "target": "w-1", "x": x, "y": y})),
        )
        .await;
    assert_eq!(result, json!({"ok": true}));
}

/// 窗口列表是 2s TTL 短缓存；写操作后读回前等待其过期，读到合成器现状。
async fn expire_window_cache() {
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
}

/// 读回 w-1 的几何（列表缓存刷新后取合成器真实状态）。
async fn read_geometry(fixture: &mut Fixture) -> (i32, i32, i32, i32) {
    let result = fixture.call_ok(method::WINDOWS_LIST, None).await;
    let entry = result["windows"]
        .as_array()
        .expect("windows array")
        .iter()
        .find(|w| w["native_id"] == json!("w-1"))
        .expect("w-1 present after move");
    (
        entry["x"].as_i64().expect("x") as i32,
        entry["y"].as_i64().expect("y") as i32,
        entry["width"].as_i64().expect("width") as i32,
        entry["height"].as_i64().expect("height") as i32,
    )
}

#[tokio::test]
async fn move_forwards_coordinates_and_reads_them_back_including_boundaries() {
    let mut fixture = Fixture::start().await;
    let compositor =
        FakeCompositor::with_windows(vec![window("w-1", "Terminal", "konsole", 10, 20, 800, 600)]);
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    // 正向移动：x/y 原样落到合成器并读回。
    move_to(&mut fixture, 400, 300).await;
    let (x, y, width, height) = read_geometry(&mut fixture).await;
    assert_eq!((x, y), (400, 300));
    assert_eq!((width, height), (800, 600), "move must not touch size");
    assert!(
        compositor.calls().iter().any(|c| c == "move:w-1:400,300"),
        "op log must record move:w-1:400,300, got {:?}",
        compositor.calls()
    );

    // 负坐标边界：原样转发并读回，不做钳制也不崩溃。
    move_to(&mut fixture, -32000, -1).await;
    expire_window_cache().await;
    let (x, y, _, _) = read_geometry(&mut fixture).await;
    assert_eq!((x, y), (-32000, -1));
    assert!(
        compositor.calls().iter().any(|c| c == "move:w-1:-32000,-1"),
        "negative coordinates must reach the backend verbatim, got {:?}",
        compositor.calls()
    );

    // 超大坐标边界：i32 极值原样转发并读回。
    move_to(&mut fixture, i32::MAX, i32::MIN).await;
    expire_window_cache().await;
    let (x, y, _, _) = read_geometry(&mut fixture).await;
    assert_eq!((x, y), (i32::MAX, i32::MIN));
    let extreme = format!("move:w-1:{},{}", i32::MAX, i32::MIN);
    assert!(
        compositor.calls().contains(&extreme),
        "extreme coordinates must reach the backend verbatim, got {:?}",
        compositor.calls()
    );
}
