//! MIT-SHM / XGetImage 截图的合成器层封装（设计文档 §11 模块结构表 `capture.rs`）。
//!
//! [`X11DisplayServer`] 已在协议层（§6.4）实现 `capture_window`（首选
//! MIT-SHM 零拷贝，失败自动降级 XGetImage）；本模块提供 thin wrapper，
//! 供 [`crate::X11Compositor::screenshot`] 与 `CaptureComponent` 复用。
//!
//! 截图优先级：MIT-SHM（零拷贝）→ XGetImage（socket 传输）。两条路径
//! 均不依赖 ImageMagick `import` / `xwd` 等外部工具。

use agent_shell_core::error::Result;
use agent_shell_displayserver_x11::X11DisplayServer;

/// 抓取指定窗口内容（原生 x11rb，无需外部工具）。
///
/// 返回原始像素数据（Z_PIXMAP，bytes-per-row 按 32 位对齐）。
/// 调用方按窗口几何的 depth/stride 解释。
pub fn capture_window(ds: &X11DisplayServer, window: u32) -> Result<Vec<u8>> {
    ds.capture_window(window)
}

/// 抓取整个屏幕（根窗口）。
pub fn capture_root(ds: &X11DisplayServer) -> Result<Vec<u8>> {
    ds.capture_window(ds.root_window())
}

/// MIT-SHM 零拷贝路径是否可用（doctor 报告）。
pub fn is_shm_available(ds: &X11DisplayServer) -> bool {
    ds.is_shm_available()
}
