//! TC-capture-04（AC-43，P1）：输出编码与后端一致性。
//!
//! - 场景：输出编码由捕获后端决定，与扩展名无关
//! - 判据：文件魔数与当前选中后端一致
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-04`
//!
//! 可判定分支：portal Screenshot → PNG（全屏，即使目标名 `.ppm`）；区域/原始
//! 像素链 → PPM(P6)（即使目标名 `.png`）；交互兜底同样产出 PNG。`x11` 与
//! portal 屏幕流的 PPM 需要真实 X server / PipeWire，由真机验收覆盖。

use std::path::Path;

use acceptance::portal::serve_screenshot_portal;
use acceptance::{black_png, sample_png, Fixture, PortalBehaviour, PrivateBus, ScreenshotMock};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

fn full_params(output: &Path) -> serde_json::Value {
    json!({"window": null, "area": null, "output_path": output.display().to_string()})
}

#[tokio::test]
async fn output_encoding_follows_backend_not_extension() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let png = sample_png();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), png.clone());
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let mut fixture =
        Fixture::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string())]).await;

    // portal Screenshot → PNG，扩展名不影响编码。
    let full = out_dir.path().join("full.ppm");
    fixture
        .call_ok(method::SCREENSHOT_CAPTURE, Some(full_params(&full)))
        .await;
    let bytes = std::fs::read(&full).expect("full capture");
    assert_eq!(
        &bytes[..8],
        &PNG_MAGIC,
        "portal screenshot output must be PNG even with a .ppm name"
    );
    assert_eq!(bytes, png, "portal PNG must be copied byte-for-byte");

    // 区域/像素链 → PPM(P6)，扩展名不影响编码。
    let area = out_dir.path().join("area.png");
    fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(json!({
                "window": null,
                "area": [0, 0, 4, 3],
                "output_path": area.display().to_string(),
            })),
        )
        .await;
    let bytes = std::fs::read(&area).expect("area capture");
    assert!(
        bytes.starts_with(b"P6\n4 3\n255\n"),
        "pixel path must write PPM(P6) even with a .png name"
    );

    // 交互兜底：静默被拒、交互成功 → 仍是 portal Screenshot 的 PNG。
    mock.set_behaviour(PortalBehaviour::Cancel);
    mock.set_interactive_behaviour(PortalBehaviour::Success);
    let interactive = out_dir.path().join("interactive.out");
    let result = fixture
        .call_ok(method::SCREENSHOT_CAPTURE, Some(full_params(&interactive)))
        .await;
    assert_eq!(result.get("width"), Some(&json!(4)));
    let bytes = std::fs::read(&interactive).expect("interactive capture");
    assert_eq!(&bytes[..8], &PNG_MAGIC);

    // 全黑/无效 PNG：校验拒绝，不产出文件。
    mock.set_behaviour(PortalBehaviour::Success);
    mock.set_interactive_behaviour(PortalBehaviour::Success);
    mock.set_png(black_png());
    let black_out = out_dir.path().join("black.png");
    let error = fixture
        .call_err(method::SCREENSHOT_CAPTURE, Some(full_params(&black_out)))
        .await;
    assert_eq!(error.code, RpcErrorCode::BackendUnavailable as i32);
    assert!(
        error.message.contains("no capture backend available"),
        "all-black artifact must be rejected as unusable: {}",
        error.message
    );
    assert!(!black_out.exists());
}
