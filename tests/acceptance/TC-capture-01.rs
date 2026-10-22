//! TC-capture-01（AC-40，P0）：全屏截图与无效输出路径。
//!
//! - 场景：全屏截图、无效输出路径
//! - 判据：文件存在且大小大于零、内容可辨认；不可写路径明确报错且不生成半成品
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-01`
//!
//! 判定面是真实 `screenshot.capture` → portal Screenshot 客户端 → daemon 落盘。
//! 私有 session bus 上的 mock portal 返回确定 PNG；daemon 逐字节复制并删除
//! 门户源文件。真实屏幕内容需要图形会话，本用例锚定文件产物与错误语义。

use std::path::Path;

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

fn capture_params(output: &Path) -> serde_json::Value {
    json!({"window": null, "area": null, "output_path": output.display().to_string()})
}

/// 目录内的文件数（门户源文件残留观测）。
fn file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .count()
}

#[tokio::test]
async fn fullscreen_capture_writes_nonempty_file_and_rejects_bad_path() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let png = sample_png();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), png.clone());
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let mut fixture =
        Fixture::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string())]).await;

    // 正常路径：非空文件、内容与门户产物逐字节一致、尺寸取自 IHDR。
    let output = out_dir.path().join("shot.ppm");
    let result = fixture
        .call_ok(method::SCREENSHOT_CAPTURE, Some(capture_params(&output)))
        .await;
    assert_eq!(
        result.get("path"),
        Some(&json!(output.display().to_string()))
    );
    assert_eq!(result.get("width"), Some(&json!(4)));
    assert_eq!(result.get("height"), Some(&json!(3)));
    let written = std::fs::read(&output).expect("capture output exists");
    assert_eq!(written, png, "capture output must be the portal PNG bytes");
    assert!(!written.is_empty());
    assert_eq!(
        file_count(portal_dir.path()),
        0,
        "portal source file must be removed after a successful copy"
    );

    // 无效输出路径：父路径是普通文件 → 明确报错、目标不存在、不生成半成品。
    let blocker = out_dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").expect("write blocker");
    let bad_output = blocker.join("shot.png");
    let error = fixture
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&bad_output)),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendError as i32);
    assert!(
        error.message.contains("create"),
        "bad output path must report the failed write: {}",
        error.message
    );
    assert!(
        !bad_output.exists(),
        "no destination file may be left behind"
    );
    assert_eq!(
        file_count(portal_dir.path()),
        1,
        "portal source must be retained when the copy fails"
    );

    // 参数缺失：解析层拒绝，不触达后端。
    let calls_before = mock.calls();
    let error = fixture
        .call_err(method::SCREENSHOT_CAPTURE, Some(json!({"window": null})))
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert_eq!(
        mock.calls(),
        calls_before,
        "missing params must not call portal"
    );

    // portal 拒绝：非 Wayland 且无 X11 兜底 → 明确的后端不可用，不产出文件。
    mock.set_behaviour(PortalBehaviour::Cancel);
    let denied_output = out_dir.path().join("denied.png");
    let error = fixture
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(capture_params(&denied_output)),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
    assert!(
        error.message.contains("no capture backend available"),
        "denied capture must explain the missing backend: {}",
        error.message
    );
    assert!(
        !denied_output.exists(),
        "denied capture must not produce an output file"
    );
}
