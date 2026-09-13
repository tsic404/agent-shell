//! X11 原生捕获——MIT-SHM 优先，失败退化 XGetImage（设计文档 §13.1 第三级、§6.4）。
//!
//! X11Generic 会话（或 portal 全部不可用时的 X11 兜底）直用
//! SHM GetImage：server 端把像素写进共享内存段，客户端免大块
//! socket 拷贝。SHM 扩展不可用时回退普通 GetImage。
//!
//! **X11 兜底门控**（§6.4 / §13.1 第三级）：`DISPLAY` 存在即视为可用候选。
//! 纯 Wayland 会话的 XWayland 也导出 `DISPLAY`，其 root 窗口在无合成器内容时
//! 直捕会得到全黑帧——但 portal（ScreenCast/Screenshot）不可用时，X11 兜底是
//! 唯一可用后端，宁可产出全黑帧也优于直接 `BackendUnavailable`（上层 doctor
//! 据此报告降级链真实状态）。原生 X11 会话则始终有真实内容。
//!

use agent_shell_core::error::{AgentShellError, Result};
use agent_shell_displayserver_x11::X11DisplayServer;

use crate::portal_screencast::{Frame, PixelFormat};

/// X11 原生捕获器：持有独立连接，按需抓取根窗口/指定窗口。
///
/// `X11DisplayServer` 内部是 x11rb `RustConnection`（Send + Sync），
/// 以 `Arc<Mutex<…>>` 串行化跨线程访问。
#[derive(Clone)]
pub struct X11Capture {
    x: std::sync::Arc<std::sync::Mutex<X11DisplayServer>>,
}

impl X11Capture {
    /// 连接 `$DISPLAY` 并探测 MIT-SHM 版本。
    ///
    /// 无 `DISPLAY`（无 X server 可达）返回 [`AgentShellError::BackendUnavailable`]。
    pub fn connect() -> Result<Self> {
        if !has_x_display() {
            return Err(AgentShellError::BackendUnavailable(
                "x11 capture unavailable: no DISPLAY (no X server reachable)".into(),
            ));
        }
        Ok(Self {
            x: std::sync::Arc::new(std::sync::Mutex::new(X11DisplayServer::connect()?)),
        })
    }

    /// X11 捕获是否可用（装配探测）。
    ///
    /// `DISPLAY` 存在且能连上 X server（真实连接探测）才判可用——陈旧/失效
    /// `DISPLAY` 不得误报可用。含纯 Wayland 会话下的 XWayland（portal 不可用
    /// 时的兜底，见模块级门控说明）。
    pub fn display_present() -> bool {
        if !has_x_display() {
            return false;
        }
        X11DisplayServer::connect().is_ok()
    }

    /// 抓取整个屏幕（根窗口），返回原始像素帧。
    pub async fn capture_frame(&self) -> Result<Frame> {
        let this = std::sync::Arc::clone(&self.x);
        tokio::task::spawn_blocking(move || {
            let x = this.lock().unwrap_or_else(|p| p.into_inner());
            capture_with(&x, None)
        })
        .await
        .map_err(|e| AgentShellError::Other(Box::new(e)))?
    }

    /// 抓取指定窗口（native X11 window id）。
    pub async fn capture_window(&self, window: u32) -> Result<Frame> {
        let this = std::sync::Arc::clone(&self.x);
        tokio::task::spawn_blocking(move || {
            let x = this.lock().unwrap_or_else(|p| p.into_inner());
            capture_with(&x, Some(window))
        })
        .await
        .map_err(|e| AgentShellError::Other(Box::new(e)))?
    }
}

/// 单次捕获：优先 MIT-SHM（memfd fd-passing），失败回退 XGetImage。
fn capture_with(x: &X11DisplayServer, window: Option<u32>) -> Result<Frame> {
    use x11rb::protocol::shm::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::{ConnectionExt as XProtoExt, Drawable, ImageFormat};

    fn cerr(e: x11rb::errors::ReplyError) -> AgentShellError {
        capture_err(e)
    }
    fn cerr2(e: x11rb::errors::ConnectionError) -> AgentShellError {
        capture_err(e)
    }

    let conn = x.connection();
    let root = window.unwrap_or_else(|| x.root_window());

    // 几何 + 位深决定行步长。X 协议 GetImage 回包无 stride 字段：
    // server 端 bytes_per_line = (width × bits-per-pixel) 按 32 位
    // 扫描线对齐（bitmap-format-scanline-pad），可能大于 width × bpp。
    // 硬编码 width×4 在 depth<32 且 width 非 4 对齐时像素错位。
    let geo = x
        .get_window_geometry(root as Drawable)
        .map_err(capture_err)?;
    if geo.width <= 0 || geo.height <= 0 {
        return Err(AgentShellError::Capture(format!(
            "degenerate geometry {geo:?}"
        )));
    }
    let (w, h) = (geo.width as u32, geo.height as u32);
    let drawable = root as Drawable;
    let geom = XProtoExt::get_geometry(conn, drawable)
        .map_err(cerr2)?
        .reply()
        .map_err(cerr)?;
    let format = match geom.depth {
        24 | 32 => PixelFormat::Bgrx,
        16 => PixelFormat::Rgb565,
        8 => PixelFormat::Clut8,
        d => {
            return Err(AgentShellError::Capture(format!(
                "unsupported x11 depth {d}"
            )));
        }
    };
    // 行步长按 32 位边界补齐；段大小同步放大。
    let bpp = format.bytes_per_pixel();
    let stride = (w as usize * bpp).div_ceil(4) * 4;

    let seg_size = (stride * h as usize) as u32;

    // MIT-SHM 探测 + fd-passing 段创建。
    if x11rb::connection::RequestConnection::extension_information(conn, shm::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some()
    {
        if let Ok(reply) = conn.shm_query_version().map_err(cerr2)?.reply() {
            // 能力门控取 AND：shared_pixmaps 与版本同时满足才走 SHM。
            // major_version 自 X11R6 恒为 1；fd-passing（本实现 memfd +
            // shm_attach_fd）要求 minor_version >= 2，否则回退 GetImage。
            if reply.shared_pixmaps && reply.major_version >= 1 && reply.minor_version >= 2 {
                match shm_capture(conn, drawable, w as u16, h as u16, seg_size) {
                    Ok(data) => {
                        return Ok(Frame {
                            data,
                            width: w,
                            height: h,
                            stride,
                            format,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("MIT-SHM capture failed, falling back to GetImage: {e}");
                    }
                }
            }
        }
    }

    // 回退：普通 GetImage。
    let reply = XProtoExt::get_image(
        conn,
        ImageFormat::Z_PIXMAP,
        drawable,
        0,
        0,
        w as u16,
        h as u16,
        !0,
    )
    .map_err(cerr2)?
    .reply()
    .map_err(cerr)?;
    Ok(Frame {
        data: reply.data,
        width: w,
        height: h,
        stride,
        format,
    })
}

/// MIT-SHM fd-passing 路径：memfd → attach_fd → shm_get_image。
fn shm_capture(
    conn: &x11rb::rust_connection::RustConnection,
    drawable: x11rb::protocol::xproto::Drawable,
    width: u16,
    height: u16,
    seg_size: u32,
) -> std::result::Result<Vec<u8>, String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::shm::ConnectionExt as _;
    use x11rb::protocol::xproto::ImageFormat;

    // 1. memfd 创建共享内存。
    let mem = rustix::fs::memfd_create("agent-shell-shm", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| format!("memfd_create: {e}"))?;
    rustix::fs::ftruncate(&mem, u64::from(seg_size)).map_err(|e| format!("ftruncate: {e}"))?;

    // 2. server 端 attach（fd passing）+ 本地映射。
    let shmseg = conn
        .generate_id()
        .map_err(|e| format!("generate_id: {e}"))?;
    let dup_fd = mem.try_clone().map_err(|e| format!("dup fd: {e}"))?;
    conn.shm_attach_fd(shmseg, dup_fd, false)
        .map_err(|e| format!("shm_attach_fd: {e}"))?;

    let map = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            seg_size as usize,
            rustix::mm::ProtFlags::READ,
            rustix::mm::MapFlags::SHARED,
            &mem,
            0,
        )
        .map_err(|e| format!("mmap: {e}"))?
    };
    struct UnmapGuard(*mut std::ffi::c_void, usize);
    impl Drop for UnmapGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = rustix::mm::munmap(self.0, self.1);
            }
        }
    }
    let _guard = UnmapGuard(map, seg_size as usize);
    // shm_get_image 的回包在 server 写完共享内存段之后才发出：
    // 等 reply 即完成同步（reply() 内部会 flush 并阻塞等待），
    // 否则读到 ftruncate 后的全零段——帧永远全黑。
    conn.shm_get_image(
        drawable,
        0,
        0,
        width,
        height,
        !0,
        ImageFormat::Z_PIXMAP.into(),
        shmseg,
        0,
    )
    .map_err(|e| format!("shm_get_image: {e}"))?
    .reply()
    .map_err(|e| format!("shm_get_image reply: {e}"))?;

    // 4. 从本地映射读出数据。
    let out = unsafe { std::slice::from_raw_parts(map.cast::<u8>(), seg_size as usize) };
    let data = out.to_vec();

    let _ = conn.shm_detach(shmseg);
    conn.flush().map_err(|e| format!("flush: {e}"))?;
    Ok(data)
}
fn capture_err(e: impl std::fmt::Display) -> AgentShellError {
    AgentShellError::Capture(format!("x11 capture: {e}"))
}

/// `DISPLAY` 环境变量是否存在（廉价门控；不含可达性探测）。
///
/// 提取为独立函数而非内联 `var_os("DISPLAY").is_some()`：`connect` 与
/// `display_present` 共用同一判据，避免两处门控漂移。
fn has_x_display() -> bool {
    has_x_display_with(std::env::var_os("DISPLAY").as_deref())
}

/// 纯函数版 `DISPLAY` 存在性判定（可单测，不触碰进程环境变量）。
fn has_x_display_with(display: Option<&std::ffi::OsStr>) -> bool {
    display.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 纯函数门控单测：`DISPLAY` 存在性判定不依赖进程环境变量（审查项：
    /// 旧测试在 DISPLAY 设置时断言体为空）。
    #[test]
    fn has_x_display_with_reflects_env_presence() {
        assert!(has_x_display_with(Some(std::ffi::OsStr::new(":0"))));
        assert!(has_x_display_with(Some(std::ffi::OsStr::new(""))));
        assert!(!has_x_display_with(None));
    }

    /// 无 DISPLAY 时 `display_present` 不触碰 X server 即返回 false；有 X server
    /// 的开发者环境由真机冒烟（`capture`/`capture_window` 实跑）覆盖。
    #[test]
    fn display_present_false_without_display_env() {
        if std::env::var_os("DISPLAY").is_none() {
            assert!(!X11Capture::display_present());
        }
    }
}
