//! 存储抽象：块设备信息。

/// 存储设备信息（由平台后端返回，供存储驱动初始化）。
pub struct StorageInfo {
    /// 存储控制器 MMIO 基地址。
    pub base: usize,
    /// 存储控制器中断号（GIC）。
    pub irq: u32,
    /// 块大小（字节）。
    pub block_size: usize,
}

impl StorageInfo {
    /// 构造存储设备信息。
    pub const fn new(base: usize, irq: u32, block_size: usize) -> Self {
        StorageInfo {
            base,
            irq,
            block_size,
        }
    }
}
