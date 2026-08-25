//! QEMU `virt` 平台后端：硬编码现有设备的地址 / 中断号 / 时钟。

use crate::display::DisplayInfo;
use crate::input::{InputInfo, InputKind, InputTransport};
use crate::net::NetInfo;
use crate::plat::Platform;
use crate::storage::StorageInfo;

/// QEMU virt 平台单例。
pub static QEMU: QemuPlatform = QemuPlatform;

/// QEMU `virt` 机器（Cortex-A72）平台参数。
pub struct QemuPlatform;

impl Platform for QemuPlatform {
    fn name(&self) -> &str {
        "QEMU virt"
    }

    fn uart_base(&self) -> usize {
        0x0900_0000 // PL011
    }

    fn uart_irq(&self) -> u32 {
        33
    }

    fn uart_clock(&self) -> u32 {
        24_000_000 // 24MHz
    }

    fn gicd_base(&self) -> usize {
        0x0800_0000 // GICv2 Distributor
    }

    fn gicc_base(&self) -> usize {
        0x0801_0000 // GICv2 CPU Interface
    }

    fn ram_base(&self) -> usize {
        0x4000_0000
    }

    fn ram_size(&self) -> usize {
        0x2000_0000 // 512MB
    }

    fn timer_freq(&self) -> u64 {
        62_500_000 // CNTFRQ 62.5MHz
    }
    fn rtc_base(&self) -> Option<usize> {
        // QEMU virt PL031：RTC 位于 0x09010000（MMIO，DTB /pl031）
        Some(0x0901_0000)
    }

    fn display_init(&self) -> Option<DisplayInfo> {
        // ramfb：帧缓冲置于 RAM 偏移 128MB 处（0x40000000 + 0x08000000）。
        Some(DisplayInfo::new(0x4800_0000, 540, 960, 32))
    }

    fn net_init(&self) -> Option<NetInfo> {
        Some(NetInfo::new(
            [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
            0x0a00_3000,
            36,
        ))
    }

    fn storage_init(&self) -> Option<StorageInfo> {
        // ramdisk：内存存储（SFS 文件系统），无 MMIO 控制器。
        None
    }

    fn input_init(&self) -> Option<InputInfo> {
        // QEMU virt 无真实触摸屏：用主 PL011 UART 上的协议帧模拟绝对坐标触摸。
        // 面板物理范围与 ramfb 逻辑分辨率一致（540x960），launcher 据此做
        // 物理坐标 → 屏幕像素坐标的映射（此处为 1:1，真实面板按 max_x/max_y 缩放）。
        Some(InputInfo::new(
            InputKind::Touch,
            InputTransport::UartProto,
            self.uart_base(),
            self.uart_irq(),
            540,
            960,
        ))
    }
}
