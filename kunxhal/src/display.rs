//! 显示抽象：帧缓冲信息。

/// 帧缓冲信息（由平台后端返回，供显示驱动初始化）。
pub struct DisplayInfo {
    /// 帧缓冲物理基地址。
    pub fb_base: usize,
    /// 水平分辨率（像素）。
    pub width: u32,
    /// 垂直分辨率（像素）。
    pub height: u32,
    /// 每像素位数。
    pub bpp: u32,
}

impl DisplayInfo {
    /// 构造显示信息。
    pub const fn new(fb_base: usize, width: u32, height: u32, bpp: u32) -> Self {
        DisplayInfo {
            fb_base,
            width,
            height,
            bpp,
        }
    }
}
