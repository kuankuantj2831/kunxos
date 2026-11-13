//! 显示抽象：帧缓冲与设备能力。未知刷新率不得当成已测得的 VSYNC。

#[path = "../../shared/display.rs"]
pub mod protocol;

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
    /// 实际行字节数，可能包含固件/GPU 对齐填充。
    pub stride_bytes: u32,
    /// 屏幕模式刷新率（mHz）；0 表示后端无法查询。
    pub refresh_millihz: u32,
    /// 1024 = 1x；由设备适配层提供，不能只由分辨率猜测。
    pub density: u32,
    /// protocol::CAP_*：只声明驱动实际实现的能力。
    pub capabilities: u32,
}

impl DisplayInfo {
    /// 构造显示信息。
    pub const fn new(fb_base: usize, width: u32, height: u32, bpp: u32) -> Self {
        DisplayInfo {
            fb_base,
            width,
            height,
            bpp,
            stride_bytes: width * (bpp / 8),
            refresh_millihz: 0,
            density: 1024,
            capabilities: 0,
        }
    }
}
