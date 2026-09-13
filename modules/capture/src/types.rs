//! 捕获公共类型（feature 无关）。
//!
//! `Frame` / `PixelFormat` / `CaptureTarget` 被 X11 路径（[`crate::x11`]）、
//! portal Screenshot 与 daemon 直接使用——不能随 `portal-screencast` feature
//! 一起排除（设计文档 §22.3 D2；TSI-3111）。因此独立成模块，始终编译。

/// 捕获目标（portal SourceType 位掩码）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureTarget {
    /// 整个显示器。
    Monitor,
    /// 单个窗口。
    Window,
}

#[cfg(feature = "portal-screencast")]
impl CaptureTarget {
    /// portal SourceType 位掩码（MONITOR=1, WINDOW=2；请求全部可用类型，
    /// 由用户在 Start 弹窗里实际选择）。
    pub(crate) fn source_type_u32(self) -> u32 {
        match self {
            CaptureTarget::Monitor => 1,
            CaptureTarget::Window => 2,
        }
    }
}

/// 帧像素布局（消费方据此做通道序与 bpp 解析）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4 字节：B,G,R,x（X11 ZPixmap little-endian / PipeWire BGRA）。
    Bgra,
    /// 4 字节：B,G,R,x——x 未定义（PipeWire BGRx），与 Bgra 同序。
    Bgrx,
    /// 4 字节：R,G,B,A（PipeWire RGBA）。
    Rgba,
    /// 2 字节：RGB565 little-endian（X11 depth 16）。
    Rgb565,
    /// 1 字节调色板索引（X11 depth 8）。
    Clut8,
}

impl PixelFormat {
    /// 每像素字节数。
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Bgra | PixelFormat::Bgrx | PixelFormat::Rgba => 4,
            PixelFormat::Rgb565 => 2,
            PixelFormat::Clut8 => 1,
        }
    }
}

/// 一帧捕获结果：原始像素 + 帧元数据。
#[derive(Clone, Debug)]
pub struct Frame {
    /// 原始像素，布局由 [`Frame::format`] 描述。
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// 协商出的/源端像素格式。
    pub format: PixelFormat,
}
