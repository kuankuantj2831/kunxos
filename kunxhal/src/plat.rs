//! Platform trait：各平台后端（QEMU / Raspberry Pi 4）实现的统一接口。

use crate::display::DisplayInfo;
use crate::input::InputInfo;
use crate::net::NetInfo;
use crate::storage::StorageInfo;

/// 硬件平台抽象。
///
/// 每个平台后端以 `static` 单例实现本 trait，由 [`crate::current_platform`]
/// 按编译期 feature 返回对应实现。内核与系统服务只通过本 trait 读取设备
/// 参数，不直接依赖任何平台硬编码。
pub trait Platform {
    /// 平台名称（用于启动日志）。
    fn name(&self) -> &str;
    /// UART MMIO 基地址。
    fn uart_base(&self) -> usize;
    /// UART 中断号（GIC）。
    fn uart_irq(&self) -> u32;
    /// UART 输入时钟（Hz）。
    fn uart_clock(&self) -> u32;
    /// GIC Distributor 基地址。
    fn gicd_base(&self) -> usize;
    /// GIC CPU Interface 基地址。
    fn gicc_base(&self) -> usize;
    /// 主存物理基地址。
    fn ram_base(&self) -> usize;
    /// 主存大小（字节）。
    fn ram_size(&self) -> usize;
    /// Generic Timer 计数频率（Hz）。
    fn timer_freq(&self) -> u64;
    /// RTC（实时时钟）MMIO 基地址；无 RTC 返回 `None`。
    /// QEMU virt 为 PL031（0x09010000），Pi4 暂未提供。
    fn rtc_base(&self) -> Option<usize>;
    /// 显示设备信息（无显示设备返回 `None`）。
    fn display_init(&self) -> Option<DisplayInfo>;
    /// 网络设备信息（无网络设备返回 `None`）。
    fn net_init(&self) -> Option<NetInfo>;
    /// 存储设备信息（内存存储等无 MMIO 控制器时返回 `None`）。
    fn storage_init(&self) -> Option<StorageInfo>;
    /// 输入设备信息（无输入设备返回 `None`）。
    fn input_init(&self) -> Option<InputInfo>;
}
