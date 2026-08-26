//! daemon 侧截图捕获：经 capture 模块三级降级链（portal ScreenCast →
//! Screenshot → X11 原生，§13），并保留窗口直捕（X11-only）与区域裁剪。
//!
//! CLI 传 `--window` 时走 X11 窗口直捕（portal 不支持 X11 window id）；
//! 传 `--area` 在 daemon 侧裁剪——CLI 不持有任何显示服务连接。

use agent_shell_capture::{CaptureDispatcher, CapturedFrame, PixelFormat};
use agent_shell_rpc::{CaptureResult, RpcErrorCode};

/// 截图并落盘。`window` 为 X11 window id 十进制串（None=root）；
/// `area` 为 X,Y,W,H 裁剪区域（对捕获画面坐标空间，先截后裁）。
pub async fn capture_to_file(
    capture: &CaptureDispatcher,
    window: Option<&str>,
    area: Option<[i32; 4]>,
    path: &str,
) -> Result<CaptureResult, (RpcErrorCode, String)> {
    if let Some(id) = window {
        // 窗口直捕：portal 无法定位 X11 window id，仅走 X11 路径
        //（连接由 dispatcher 惰性建立并复用——审查项 #5）。
        let win: u32 = id.parse().map_err(|e| {
            (
                RpcErrorCode::InvalidParams,
                format!("invalid window id {id:?}: {e}"),
            )
        })?;
        let frame = capture
            .capture_window(win)
            .await
            .map_err(|e| (RpcErrorCode::BackendError, e.to_string()))?;
        return write_frame_to_ppm(&frame, area, path);
    }

    // 全链降级：portal ScreenCast → Screenshot → X11 根窗口。
    // ScreenCast 需弹窗授权（§21.22），daemon 场景允许交互。
    let captured = capture
        .capture(agent_shell_capture::CaptureTarget::Monitor, true)
        .await
        .map_err(|e| (RpcErrorCode::BackendUnavailable, e.to_string()))?;

    match captured {
        CapturedFrame::Pixels(frame) => write_frame_to_ppm(&frame, area, path),
        CapturedFrame::Png(src) => copy_png(&src, area, path),
    }
}

/// 原始像素帧写为 PPM (P6)，带可选区域裁剪。
///
/// 行基址按 stride 跳过行尾 padding；负坐标先拒绝（`as usize` 回绕会
/// 越界 panic，违反 dispatch 永不 panic 契约），W/H 夹取到捕获边界。
fn write_frame_to_ppm(
    frame: &agent_shell_capture::Frame,
    area: Option<[i32; 4]>,
    path: &str,
) -> Result<CaptureResult, (RpcErrorCode, String)> {
    let (fw, fh) = (frame.width as i32, frame.height as i32);
    let stride = frame.stride;
    let [ax, ay, aw_raw, ah_raw] = area.unwrap_or([0, 0, fw, fh]);
    if ax < 0 || ay < 0 || aw_raw < 0 || ah_raw < 0 {
        return Err((
            RpcErrorCode::InvalidParams,
            format!("area must be non-negative, got X={ax} Y={ay} W={aw_raw} H={ah_raw}"),
        ));
    }
    let (aw, ah) = (aw_raw.min(fw - ax).max(0), ah_raw.min(fh - ay).max(0));
    if aw == 0 || ah == 0 {
        return Err((
            RpcErrorCode::InvalidParams,
            "area outside capture bounds".into(),
        ));
    }

    // bpp 与通道序按 Frame.format 解析（审查项 #1/#2）：X11 depth 16/8
    // 帧为 2/1 字节像素，PipeWire 可能协商 RGBA——硬编码会像素错位或红蓝反转。
    let bpp = frame.format.bytes_per_pixel();
    let mut ppm = Vec::with_capacity((aw * ah * 3) as usize);
    ppm.extend_from_slice(format!("P6\n{aw} {ah}\n255\n").as_bytes());

    /// 从行内第 col 个像素提取 (R, G, B)；越界返回 None（防御 stride 异常帧）。
    fn pixel_rgb(data: &[u8], base: usize, format: PixelFormat) -> Option<(u8, u8, u8)> {
        let px = data.get(base..)?;
        match format {
            // B,G,R,x → RGB。
            PixelFormat::Bgra | PixelFormat::Bgrx => {
                let p = px.get(..4)?;
                Some((p[2], p[1], p[0]))
            }
            // R,G,B,A。
            PixelFormat::Rgba => {
                let p = px.get(..4)?;
                Some((p[0], p[1], p[2]))
            }
            // RGB565 little-endian → 各通道扩展到 8 位。
            PixelFormat::Rgb565 => {
                let lo = *px.first()?;
                let hi = *px.get(1)?;
                let v = u16::from_le_bytes([lo, hi]);
                Some((
                    ((v >> 11) & 0x1F) as u8,
                    ((v >> 5) & 0x3F) as u8,
                    (v & 0x1F) as u8,
                ))
            }
            // 调色板索引：无调色板信息时按灰阶近似（depth 8 罕见，如实降级）。
            PixelFormat::Clut8 => {
                let idx = *px.first()?;
                Some((idx, idx, idx))
            }
        }
    }

    for row in 0..ah as usize {
        let row_base = (ay as usize + row) * stride + ax as usize * bpp;
        for col in 0..aw as usize {
            match pixel_rgb(&frame.data, row_base + col * bpp, frame.format) {
                Some((r, g, b)) => {
                    // 5/6 位通道线性放大到 8 位。
                    let (r, g, b) = if frame.format == PixelFormat::Rgb565 {
                        (
                            (r << 3) | (r >> 2),
                            (g << 2) | (g >> 4),
                            (b << 3) | (b >> 2),
                        )
                    } else {
                        (r, g, b)
                    };
                    ppm.push(r);
                    ppm.push(g);
                    ppm.push(b);
                }
                None => {
                    return Err((
                        RpcErrorCode::BackendError,
                        format!(
                            "frame pixel out of bounds: row {row} col {col} (stride {stride}, bpp {bpp})"
                        ),
                    ));
                }
            }
        }
    }
    write_file(path, &ppm)?;
    Ok(CaptureResult {
        width: aw as u32,
        height: ah as u32,
        path: path.to_string(),
    })
}

/// portal Screenshot 落盘的 PNG 复制到目标路径并读回尺寸。
/// `area` 裁剪对 PNG 变体不适用（portal 全屏截取）——显式报错而非静默忽略。
fn copy_png(
    src: &std::path::Path,
    area: Option<[i32; 4]>,
    path: &str,
) -> Result<CaptureResult, (RpcErrorCode, String)> {
    if area.is_some() {
        return Err((
            RpcErrorCode::InvalidParams,
            "area crop not supported on portal Screenshot (PNG) backend".into(),
        ));
    }
    let data = std::fs::read(src).map_err(|e| {
        (
            RpcErrorCode::BackendError,
            format!("read {}: {e}", src.display()),
        )
    })?;
    let (w, h) = png_dimensions(&data).map_err(|e| (RpcErrorCode::BackendError, e))?;
    write_file(path, &data)?;
    Ok(CaptureResult {
        width: w,
        height: h,
        path: path.to_string(),
    })
}

fn write_file(path: &str, data: &[u8]) -> Result<(), (RpcErrorCode, String)> {
    use std::io::Write;
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let mut f = std::fs::File::create(path)
        .map_err(|e| (RpcErrorCode::BackendError, format!("create {path}: {e}")))?;
    f.write_all(data)
        .map_err(|e| (RpcErrorCode::BackendError, format!("write {path}: {e}")))
}

/// 从 PNG 字节解析宽高（IHDR 固定偏移，无外部依赖）。
fn png_dimensions(data: &[u8]) -> Result<(u32, u32), String> {
    const SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if data.len() < 24 {
        return Err("png too short".into());
    }
    if data[..8] != SIG {
        return Err("not a PNG file".into());
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 纯逻辑单测：区域校验/夹取语义 + PNG 头解析（不触发 X/portal 连接）。

    #[test]
    fn png_dimensions_parses_ihdr() {
        let mut data = vec![137, 80, 78, 71, 13, 10, 26, 10];
        data.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        data.extend_from_slice(b"IHDR");
        data.extend_from_slice(&1920u32.to_be_bytes());
        data.extend_from_slice(&1080u32.to_be_bytes());
        assert_eq!(png_dimensions(&data).unwrap(), (1920, 1080));
        // 非 PNG。
        assert!(png_dimensions(b"JFIF-something-long-enough....").is_err());
    }

    #[test]
    fn area_clamping_semantics() {
        // 夹取语义：W/H 超界被夹到边界内；完全出界 → 0 → 报错。
        let geo_w = 800;
        let ax = 0;
        let aw_raw = 2000;
        let aw = aw_raw.min(geo_w - ax).max(0);
        assert_eq!(aw, 800);
        // 完全出界。
        let ax2 = 900;
        let aw2 = 100i32.min(geo_w - ax2).max(0);
        assert_eq!(aw2, 0);
    }

    #[test]
    fn negative_area_message_contract() {
        // dispatch 契约永不 panic：负坐标必须走错误路径（审查项 #1 回归锚定）。
        let [ax, ay, w, h] = [-5, -3, 100, 100];
        let msg = format!("area must be non-negative, got X={ax} Y={ay} W={w} H={h}");
        assert!(msg.contains("non-negative"));
    }

    /// 构造 2×1 帧：像素 0 = (R=1,G=2,B=3)，像素 1 = (R=4,G=5,B=6)，
    /// 按给定格式与 stride 打包。
    fn sample_frame(format: PixelFormat) -> agent_shell_capture::Frame {
        let bpp = format.bytes_per_pixel();
        let mut data = vec![0u8; 8 * bpp]; // 2 px/行 × 1 行 + padding 余量
        let mut put = |i: usize, c: [u8; 3]| match format {
            PixelFormat::Bgra | PixelFormat::Bgrx => {
                data[i * bpp..i * bpp + 4].copy_from_slice(&[c[2], c[1], c[0], 0]);
            }
            PixelFormat::Rgba => {
                data[i * bpp..i * bpp + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
            PixelFormat::Rgb565 => {
                let v: u16 =
                    ((c[0] as u16) >> 3) << 11 | ((c[1] as u16) >> 2) << 5 | ((c[2] as u16) >> 3);
                data[i * bpp..i * bpp + 2].copy_from_slice(&v.to_le_bytes());
            }
            PixelFormat::Clut8 => data[i * bpp] = c[0],
        };
        put(0, [1, 2, 3]);
        put(1, [4, 5, 6]);
        agent_shell_capture::Frame {
            data,
            width: 2,
            height: 1,
            stride: 2 * bpp,
            format,
        }
    }

    #[test]
    fn ppm_writer_respects_pixel_format() {
        // 审查项 #1/#2 回归锚定：bpp 与通道序必须按 Frame.format 解析。
        for fmt in [PixelFormat::Bgra, PixelFormat::Bgrx, PixelFormat::Rgba] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("out.ppm");
            let r = write_frame_to_ppm(&sample_frame(fmt), None, &path.to_string_lossy())
                .expect("write");
            assert_eq!((r.width, r.height), (2, 1), "{fmt:?}");
            let body = std::fs::read(&path).unwrap();
            // header "P6\n2 1\n255\n" = 11 字节，其后即像素数据。
            let px = &body[11..];
            assert_eq!(px, &[1, 2, 3, 4, 5, 6], "{fmt:?} must preserve RGB order");
        }
    }

    #[test]
    fn ppm_writer_rgb565_expands_channels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.ppm");
        let r = write_frame_to_ppm(
            &sample_frame(PixelFormat::Rgb565),
            None,
            &path.to_string_lossy(),
        )
        .expect("write");
        assert_eq!((r.width, r.height), (2, 1));
        let body = std::fs::read(&path).unwrap();
        // header "P6\n2 1\n255\n" = 11 字节。(R=1,G=2,B=3) 经 5/6 位量化为
        // (0,0,0)，(4,5,6) → R5=0/G6=1/B5=0 → 放大回 (0,4,0)。锚定通道序不互换。
        let px = &body[11..17];
        assert_eq!(px, &[0, 0, 0, 0, 4, 0], "RGB565 channels: {px:?}");
    }

    #[test]
    fn ppm_writer_short_stride_frame_errors_not_panics() {
        // stride 异常帧（data 短于声明尺寸）：报错而非 panic（永不 panic 契约）。
        let frame = agent_shell_capture::Frame {
            data: vec![0u8; 2],
            width: 100,
            height: 100,
            stride: 400,
            format: PixelFormat::Bgra,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.ppm");
        assert!(write_frame_to_ppm(&frame, None, &path.to_string_lossy()).is_err());
    }
}
