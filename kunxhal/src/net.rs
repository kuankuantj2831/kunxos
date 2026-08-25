//! 网络抽象：网卡信息。

/// 网卡信息（由平台后端返回，供网络驱动初始化）。
pub struct NetInfo {
    /// MAC 地址。
    pub mac: [u8; 6],
    /// 网卡 MMIO 基地址。
    pub base: usize,
    /// 网卡中断号（GIC）。
    pub irq: u32,
}

impl NetInfo {
    /// 构造网卡信息。
    pub const fn new(mac: [u8; 6], base: usize, irq: u32) -> Self {
        NetInfo { mac, base, irq }
    }
}
