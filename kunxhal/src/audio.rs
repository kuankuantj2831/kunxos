//! 音频抽象（骨架：当前无平台后端实现音频设备）。

/// 音频设备信息（由平台后端返回，供音频驱动初始化）。
pub struct AudioInfo {
    /// 音频控制器 MMIO 基地址。
    pub base: usize,
    /// 音频控制器中断号（GIC）。
    pub irq: u32,
}

impl AudioInfo {
    /// 构造音频设备信息。
    pub const fn new(base: usize, irq: u32) -> Self {
        AudioInfo { base, irq }
    }
}
