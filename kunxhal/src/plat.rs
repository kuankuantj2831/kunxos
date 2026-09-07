//! Platform trait：各平台后端（QEMU / Raspberry Pi 4）实现的统一接口。

use crate::display::DisplayInfo;
use crate::input::InputInfo;
use crate::net::NetInfo;
use crate::power::{Battery, DvfsController, PowerControl, ThermalSensor};
use crate::sensor::SensorInfo;
use crate::storage::StorageInfo;
use crate::wifi::WifiDevice;

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

    // ==================== Wi-Fi ====================

    /// Wi-Fi 网卡后端（无 Wi-Fi 硬件返回 `None`）。
    /// QEMU 返回软件模拟后端（真实 4-way handshake 密码学链路，见 `plat_qemu`）；
    /// Pi4 返回 CYW43455 SDIO 后端骨架（见 `plat_pi4`，硬件接线待真机验证）。
    /// 默认 `None`：未实现 Wi-Fi 的平台只失去无线能力，不影响编译。
    /// 返回 `&'static mut`：后端的扫描 / 关联 / EAPOL 中继均为变更接口。
    fn wifi_init(&self) -> Option<&'static mut dyn WifiDevice> {
        None
    }

    // ==================== 传感器 ====================

    /// 传感器信息（无传感器总线返回 `None`）。QEMU 返回软件模拟传感器；
    /// Pi4 返回 BSC1 + MPU6050/9250 参数（真实探测在驱动层完成，探测失败
    /// 回退到模拟器）。默认 `None`。
    fn sensor_init(&self) -> Option<SensorInfo> {
        None
    }

    // ==================== 电源 / 热 / DVFS / 电池 ====================
    //
    // 默认返回 `None`：未实现电源后端的平台只失去对应能力，不影响编译。

    /// 电源控制（关机 / 重启 / 休眠）；无实现返回 `None`。
    fn power_control(&self) -> Option<&'static dyn PowerControl> {
        None
    }

    /// 热传感器（SoC 温度）；无实现返回 `None`。
    fn thermal_sensor(&self) -> Option<&'static dyn ThermalSensor> {
        None
    }

    /// DVFS 控制器（频点表 / 频率读写）；无实现返回 `None`。
    fn dvfs_controller(&self) -> Option<&'static dyn DvfsController> {
        None
    }

    /// 核心电压（mV）；无电压读取能力返回 `None`（Pi4 经 mailbox
    /// `GET_VOLTAGE` 读取，QEMU 无电压概念）。
    fn core_voltage_mv(&self) -> Option<u32> {
        None
    }

    /// 电池 / 充电器；无电池平台返回 `None`。
    fn battery(&self) -> Option<&'static dyn Battery> {
        None
    }
}
