//! TC-capture-03（AC-42，P0）：区域截图与非法区域。
//!
//! - 场景：`screenshot --area X,Y,W,H` 的裁剪与边界
//! - 判据：图像尺寸与内容对应指定区域；非法区域行为可解释
//! - 执行：`cargo test -p agent-shell-acceptance --test TC-capture-03`
//!
//! 判定面是真实 `screenshot.capture` 的区域路由：门户 PNG 在捕获组件内解码为
//! 像素、daemon 按区域裁剪并写 PPM(P6)。样例 PNG 的像素由夹具确定，故可逐
//! 字节断言裁剪结果；负坐标/越界/零尺寸给出 InvalidParams 且不落盘。

use std::path::Path;

use acceptance::portal::serve_screenshot_portal;
use acceptance::{sample_png, Fixture, PrivateBus, ScreenshotMock};
use agent_shell_rpc::{method, RpcErrorCode};
use serde_json::json;

/// 样例 PNG 的像素（编码见 `acceptance::sample_png`）。
fn pixel(x: i32, y: i32) -> [u8; 3] {
    [
        ((x * 60 + 10) % 256) as u8,
        ((y * 80 + 20) % 256) as u8,
        (((x + y) * 40 + 30) % 256) as u8,
    ]
}

fn area_params(output: &Path, area: [i32; 4]) -> serde_json::Value {
    json!({
        "window": null,
        "area": area,
        "output_path": output.display().to_string(),
    })
}

/// 断言 PPM(P6) 的头部与逐像素内容。
fn assert_ppm(path: &Path, width: u32, height: u32, pixels: &[(i32, i32)]) {
    let bytes = std::fs::read(path).expect("ppm exists");
    let header = format!("P6\n{width} {height}\n255\n");
    assert!(
        bytes.starts_with(header.as_bytes()),
        "PPM header must be {header:?}"
    );
    let body = &bytes[header.len()..];
    assert_eq!(body.len(), (width * height * 3) as usize);
    let expected: Vec<u8> = pixels.iter().flat_map(|(x, y)| pixel(*x, *y)).collect();
    assert_eq!(
        body,
        expected.as_slice(),
        "cropped pixels must match the area"
    );
}

#[tokio::test]
async fn area_capture_crops_exact_region_and_rejects_invalid_area() {
    let portal_dir = tempfile::tempdir().expect("portal temp");
    let out_dir = tempfile::tempdir().expect("output temp");
    let bus = PrivateBus::start();
    let mock = ScreenshotMock::new(portal_dir.path().to_path_buf(), sample_png());
    let _portal = serve_screenshot_portal(&bus, mock.clone()).await;
    let mut fixture =
        Fixture::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", bus.address().to_string())]).await;

    // 内部区域：2×2，逐像素对应。
    let output = out_dir.path().join("area.ppm");
    let result = fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(area_params(&output, [1, 1, 2, 2])),
        )
        .await;
    assert_eq!(result.get("width"), Some(&json!(2)));
    assert_eq!(result.get("height"), Some(&json!(2)));
    assert_ppm(&output, 2, 2, &[(1, 1), (2, 1), (1, 2), (2, 2)]);

    // 越界区域：裁剪到捕获边界（右下 1×1）。
    let clipped = out_dir.path().join("clipped.ppm");
    let result = fixture
        .call_ok(
            method::SCREENSHOT_CAPTURE,
            Some(area_params(&clipped, [3, 2, 10, 10])),
        )
        .await;
    assert_eq!(result.get("width"), Some(&json!(1)));
    assert_eq!(result.get("height"), Some(&json!(1)));
    assert_ppm(&clipped, 1, 1, &[(3, 2)]);

    // 非法区域：负坐标 / 零尺寸 / 完全在界外 → InvalidParams 且不落盘。
    let invalid = out_dir.path().join("invalid.ppm");
    for (area, expected) in [
        ([-1, 0, 1, 1], "area coordinates must be non-negative"),
        ([0, 0, 0, 0], "area outside capture bounds"),
        ([9, 9, 1, 1], "area outside capture bounds"),
    ] {
        let calls_before = mock.calls();
        let error = fixture
            .call_err(
                method::SCREENSHOT_CAPTURE,
                Some(area_params(&invalid, area)),
            )
            .await;
        assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
        assert!(
            error.message.contains(expected),
            "area {area:?} must explain rejection ({expected:?}): {}",
            error.message
        );
        assert!(!invalid.exists(), "invalid area must not write a file");
        assert!(
            mock.calls() > calls_before,
            "area validation happens after capture: the portal call is observable"
        );
    }

    // 形状错误在参数解析层拒绝，不触达捕获链。
    let calls_before = mock.calls();
    let error = fixture
        .call_err(
            method::SCREENSHOT_CAPTURE,
            Some(json!({
                "window": null,
                "area": [0, 0, 10],
                "output_path": invalid.display().to_string(),
            })),
        )
        .await;
    assert_eq!(error.code, RpcErrorCode::InvalidParams as i32);
    assert_eq!(mock.calls(), calls_before);
}
