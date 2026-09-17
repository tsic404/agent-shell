//! 真机冒烟：`--area` 区域截图的原始像素链 + 裁切语义核验。
//!
//! CI 全为无头环境，`--area` 裁切在真机上抓到的是否「预期桌面画面」无从核验
//! （锁定桌面下 X11 兜底直捕 root 得全黑帧，字节级一致无法证明内容语义）。
//! 本冒烟在原生 X11 会话上：读 root 几何、保存标记区域原始像素后填充可识别
//! 色块、走 `capture_pixels`（`--area` 像素链，portal Screenshot 被跳过 → X11
//! 兜底）抓全帧，再把标记子帧从全帧裁出（stride 感知紧排），断言裁切产物尺寸
//! == 请求区域且非全黑；结束经 Drop 恢复原始像素，真机桌面不残留污点。
//!
//! 无 `DISPLAY` 打印 SKIP 退出 0（无头不适用）；`DISPLAY` 存在但连不上 X server
//! 视为环境损坏、退出 2。运行（`--no-default-features` 与 issue 构建口径一致）：
//!   cargo run --release --example area_smoke -p agent-shell-capture --no-default-features

use agent_shell_capture::{CaptureDispatcher, CaptureTarget, Frame, PixelFormat, X11Capture};
use agent_shell_displayserver_x11::X11DisplayServer;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux, ImageFormat, Rectangle};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("DISPLAY").is_none() {
        println!("SKIP: no DISPLAY — area_smoke requires a native X11 session");
        return Ok(());
    }

    let server = match X11DisplayServer::connect() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAIL: DISPLAY set but X11 unreachable: {e}");
            std::process::exit(2);
        }
    };
    let root = server.root_window();
    let geo = server
        .get_window_geometry(root)
        .map_err(|e| format!("root geometry: {e}"))?;
    let (root_w, root_h) = (geo.width, geo.height);
    assert!(root_w > 0 && root_h > 0, "degenerate root geometry");
    println!("xwininfo -root: {root_w}x{root_h}");

    // 中央 1/4 区域色块：确定性「可识别图形」，不依赖桌面壁纸是否可见。
    let m_x = root_w / 4;
    let m_y = root_h / 4;
    let m_w = (root_w / 2).clamp(1, root_w - m_x);
    let m_h = (root_h / 2).clamp(1, root_h - m_y);

    let conn = server.connection();
    let cmap = conn.setup().roots[server.screen_index()].default_colormap;

    // 填充前保存标记区域原始像素（server Z_PIXMAP 字节，可直接 put_image 写回）。
    let original = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            root,
            m_x as i16,
            m_y as i16,
            m_w as u16,
            m_h as u16,
            !0,
        )?
        .reply()
        .map_err(|e| format!("get_image (save original region): {e}"))?;
    let depth = original.depth;

    // AllocColor 在 TrueColor/PseudoColor 上都返回可绘制的非黑像素（亮品红）。
    let pixel = conn
        .alloc_color(cmap, 0xFFFF, 0x0000, 0xFFFF)?
        .reply()?
        .pixel;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::new().foreground(pixel))?;
    conn.poly_fill_rectangle(
        root,
        gc,
        &[Rectangle {
            x: m_x as i16,
            y: m_y as i16,
            width: m_w as u16,
            height: m_h as u16,
        }],
    )?;
    conn.flush()?;
    // sync 保证色块已落 root，后续另一条连接的 GetImage 必见其内容。
    server.sync()?;

    // 此后任何提前返回/panic 都经 Drop 写回原始像素，真机桌面不残留色块。
    let _restore = RootRestore {
        conn,
        root,
        gc,
        depth,
        x: m_x as i16,
        y: m_y as i16,
        width: m_w as u16,
        height: m_h as u16,
        original: original.data,
    };

    // `--area` 原始像素链：portal Screenshot 被跳过（PNG 无法裁剪）→ 原生
    // X11 兜底。无会话总线（装配失败）退化到 X11 直捕——同一终端步骤。
    let frame = match CaptureDispatcher::assemble().await {
        Some(d) => {
            println!("capture_pixels (--area chain) via CaptureDispatcher");
            d.capture_pixels(CaptureTarget::Monitor, false).await?
        }
        None => {
            println!("CaptureDispatcher unavailable (no session bus); X11 direct");
            X11Capture::connect()?.capture_frame().await?
        }
    };

    // 尺寸对齐：全帧尺寸必须等于 root 几何（`xwininfo -root` 对齐）。
    assert_eq!(
        (frame.width as i32, frame.height as i32),
        (root_w, root_h),
        "frame size must match root geometry (xwininfo -root alignment)"
    );

    // 真正执行裁切：从全帧裁出标记子帧（stride 感知、紧排），再对裁切产物断言。
    let cropped = crop_region(&frame, m_x as u32, m_y as u32, m_w as u32, m_h as u32);
    assert_eq!(
        (cropped.width, cropped.height),
        (m_w as u32, m_h as u32),
        "cropped frame must be exactly the requested region"
    );
    let frac = non_black_fraction(&cropped);
    println!(
        "cropped {}x{} non-black fraction: {frac:.3}",
        cropped.width, cropped.height
    );
    assert!(
        frac > 0.5,
        "cropped marker region is mostly black ({frac:.3}) — crop produced no recognizable content"
    );

    println!("SMOKE OK");
    Ok(())
}

/// 从全帧裁出紧排子帧（`--area` 裁切步骤；stride 感知，W/H 夹取到帧边界）。
fn crop_region(frame: &Frame, x: u32, y: u32, w: u32, h: u32) -> Frame {
    let x = x.min(frame.width);
    let y = y.min(frame.height);
    let w = w.min(frame.width - x);
    let h = h.min(frame.height - y);
    let bpp = frame.format.bytes_per_pixel();
    let mut data = Vec::with_capacity(w as usize * h as usize * bpp);
    for row in y..y + h {
        let start = row as usize * frame.stride + x as usize * bpp;
        data.extend_from_slice(&frame.data[start..start + w as usize * bpp]);
    }
    Frame {
        data,
        width: w,
        height: h,
        stride: w as usize * bpp,
        format: frame.format,
    }
}

/// 帧非黑像素占比（格式感知，与 capture::is_all_black 同口径：忽略 alpha/填充）。
fn non_black_fraction(frame: &Frame) -> f64 {
    let bpp = frame.format.bytes_per_pixel();
    let mut non_black = 0u64;
    let total = frame.width as u64 * frame.height as u64;
    for row in 0..frame.height {
        let row_start = row as usize * frame.stride;
        for col in 0..frame.width {
            let off = row_start + col as usize * bpp;
            let px = &frame.data[off..off + bpp];
            let black = match frame.format {
                PixelFormat::Bgra | PixelFormat::Bgrx | PixelFormat::Rgba => {
                    px[..3].iter().all(|&b| b == 0)
                }
                PixelFormat::Rgb565 | PixelFormat::Clut8 => px.iter().all(|&b| b == 0),
            };
            if !black {
                non_black += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        non_black as f64 / total as f64
    }
}

/// 恢复 root 标记区域原始像素的 Drop guard——无论成功/报错/panic 都写回，
/// 保证真机冒烟结束桌面无残留色块。
struct RootRestore<'a> {
    conn: &'a RustConnection,
    root: u32,
    gc: u32,
    depth: u8,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    original: Vec<u8>,
}

impl Drop for RootRestore<'_> {
    fn drop(&mut self) {
        let _ = self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            self.root,
            self.gc,
            self.width,
            self.height,
            self.x,
            self.y,
            0,
            self.depth,
            &self.original,
        );
        let _ = self.conn.flush();
        // 确保 put_image 已提交到 server 再关连接，避免残留。
        let _ = self.conn.sync();
    }
}
