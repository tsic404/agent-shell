//! TC-windows-05（AC-14，P1）：缩放写操作、读回与零/负尺寸的既定结果。
//!
//! - 场景：`windows.op {"op":"resize"}` 正向缩放并读回；零尺寸与负尺寸边界
//! - 判据：正向缩放后 `windows.list` 读回的 width/height 与目标一致，合成器
//!   记录 `resize:<id>:<w>,<h>`；零/负尺寸有既定结果——要么被转发并读回
//!   （daemon 与合成器通道不做钳制，如实落库），要么显式报 RPC 错误；本用例
//!   断言实际行为，不静默跳过
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-windows-05`
//!
//! 判定面是真实 `windows.op` → daemon 写前校验 → 合成器通道（脚本化）与
//! 缓存刷新后的 `windows.list` 读回。无图形会话，无环境绑定部分。

use acceptance::{window, FakeCompositor, Fixture};
use agent_shell_core::types::DesktopEnvironment;
use agent_shell_rpc::method;
use serde_json::json;

async fn resize(fixture: &mut Fixture, width: i32, height: i32) -> serde_json::Value {
    fixture
        .call_ok(
            method::WINDOW_OP,
            Some(json!({"op": "resize", "target": "w-1", "width": width, "height": height})),
        )
        .await
}

/// 窗口列表是 2s TTL 短缓存；写操作后读回前等待其过期，读到合成器现状。
async fn expire_window_cache() {
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
}

/// 读回 w-1 的几何。
async fn read_geometry(fixture: &mut Fixture) -> (i32, i32, i32, i32) {
    let result = fixture.call_ok(method::WINDOWS_LIST, None).await;
    let entry = result["windows"]
        .as_array()
        .expect("windows array")
        .iter()
        .find(|w| w["native_id"] == json!("w-1"))
        .expect("w-1 present after resize");
    (
        entry["x"].as_i64().expect("x") as i32,
        entry["y"].as_i64().expect("y") as i32,
        entry["width"].as_i64().expect("width") as i32,
        entry["height"].as_i64().expect("height") as i32,
    )
}

#[tokio::test]
async fn resize_reads_back_and_zero_or_negative_sizes_have_a_defined_outcome() {
    let mut fixture = Fixture::start().await;
    let compositor =
        FakeCompositor::with_windows(vec![window("w-1", "Terminal", "konsole", 10, 20, 800, 600)]);
    fixture.install_compositor(&compositor, DesktopEnvironment::KDE);

    // 正向缩放：width/height 原样落到合成器并读回，位置不被触碰。
    assert_eq!(resize(&mut fixture, 1024, 768).await, json!({"ok": true}));
    let (x, y, width, height) = read_geometry(&mut fixture).await;
    assert_eq!((width, height), (1024, 768));
    assert_eq!((x, y), (10, 20), "resize must not touch position");
    assert!(
        compositor
            .calls()
            .iter()
            .any(|c| c == "resize:w-1:1024,768"),
        "op log must record resize:w-1:1024,768, got {:?}",
        compositor.calls()
    );

    // 零尺寸的既定结果：daemon 参数层无校验（合成器通道如实接收），
    // 故断言「成功且读回 0/0」这一实际行为，而不是静默跳过。
    assert_eq!(resize(&mut fixture, 0, 0).await, json!({"ok": true}));
    expire_window_cache().await;
    let (_, _, width, height) = read_geometry(&mut fixture).await;
    assert_eq!((width, height), (0, 0));
    assert!(
        compositor.calls().iter().any(|c| c == "resize:w-1:0,0"),
        "zero size must reach the backend, got {:?}",
        compositor.calls()
    );

    // 负尺寸的既定结果：同样被转发并读回（后端无钳制），进程不崩溃。
    assert_eq!(resize(&mut fixture, -1, -5).await, json!({"ok": true}));
    expire_window_cache().await;
    let (_, _, width, height) = read_geometry(&mut fixture).await;
    assert_eq!((width, height), (-1, -5));
    assert!(
        compositor.calls().iter().any(|c| c == "resize:w-1:-1,-5"),
        "negative size must reach the backend verbatim, got {:?}",
        compositor.calls()
    );
}
