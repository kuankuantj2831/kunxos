//! 极简 PCI ECAM（pci-host-ecam-generic）：配置空间 MMIO 访问与设备扫描。
//!
//! QEMU `virt` 机器的 PCIe 主机控制器是 ECAM 风格：配置空间以 MMIO 方式
//! 暴露（地址 = ECAM 基址 + (bus<<20)|(dev<<15)|(fn<<12)|offset），基址与
//! 大小由设备树 `pcie` 节点的 `reg` 属性给出（QEMU 11.x 为 0x4010_0000_00，
//! 256MB）。本模块从设备树解析 ECAM 基址，供 AC97 等 PCI 设备驱动使用。

use core::ptr::{read_volatile, write_volatile};

use crate::fdt;

/// PCI 配置空间寄存器偏移。
pub const VENDOR_ID: usize = 0x00;
pub const COMMAND: usize = 0x04;
pub const BAR0: usize = 0x10;
pub const BAR1: usize = 0x14;
pub const BAR2: usize = 0x18;
pub const BAR3: usize = 0x1c;
pub const BAR4: usize = 0x20;
pub const BAR5: usize = 0x24;

/// Command 寄存器位：内存空间使能 / 总线主控使能。
pub const CMD_MEM_SPACE: u32 = 0x02;
pub const CMD_BUS_MASTER: u32 = 0x04;

/// 一个已定位的 PCI 设备（bus/dev/fn + ECAM 基址）。
#[derive(Clone, Copy)]
pub struct PciDev {
    ecam: usize,
    pub bus: u32,
    pub dev: u32,
    pub func: u32,
}

impl PciDev {
    #[inline]
    fn cfg_addr(&self, offset: usize) -> usize {
        self.ecam
            + ((self.bus << 20) | (self.dev << 15) | (self.func << 12) | offset as u32) as usize
    }

    /// 读配置空间 32 位。
    pub fn read32(&self, offset: usize) -> u32 {
        unsafe { read_volatile(self.cfg_addr(offset) as *const u32) }
    }

    /// 写配置空间 32 位。
    pub fn write32(&self, offset: usize, val: u32) {
        unsafe { write_volatile(self.cfg_addr(offset) as *mut u32, val) }
    }

    /// 读配置空间 16 位。
    pub fn read16(&self, offset: usize) -> u16 {
        unsafe { read_volatile(self.cfg_addr(offset) as *const u16) }
    }

    /// 读配置空间 8 位。
    pub fn read8(&self, offset: usize) -> u8 {
        unsafe { read_volatile(self.cfg_addr(offset) as *const u8) }
    }

    pub fn vendor(&self) -> u16 {
        self.read16(VENDOR_ID)
    }
    pub fn device(&self) -> u16 {
        self.read16(VENDOR_ID + 2)
    }

    /// 读取 BAR（去掉 IO/内存类型位与对齐位），返回 32 位基址；64 位 BAR 未处理。
    pub fn bar(&self, index: usize) -> Option<usize> {
        let raw = self.read32(BAR0 + index * 4);
        if raw == 0 {
            return None;
        }
        // bit0 = IO 空间；内存 BAR 低 4 位为类型位
        Some((raw & !0xF) as usize)
    }

    /// 读取 I/O 空间 BAR：返回 PCI I/O 端口号（不含 PIO 窗口偏移）。
    /// I/O BAR 的 bit0 = 1（I/O 标志），基址低 2 位对齐。
    pub fn io_bar_port(&self, index: usize) -> Option<usize> {
        let raw = self.read32(BAR0 + index * 4);
        if raw == 0 || raw & 1 == 0 {
            return None; // 非 I/O BAR 或未分配
        }
        Some((raw & !0x3) as usize)
    }

    /// 使能内存空间、I/O 空间与总线主控。
    pub fn enable_mem_master(&self) {
        let cmd = self.read16(COMMAND);
        self.write32(COMMAND, (cmd as u32 | CMD_MEM_SPACE | CMD_BUS_MASTER | 0x1) & 0xFFFF);
    }
}

/// 从设备树解析 ECAM 基址。
fn ecam_from_fdt(fdt: &fdt::Fdt) -> Option<usize> {
    let node = fdt.find_compatible("pci-host-ecam-generic")?;
    let (base, _size) = node.reg(fdt, 0)?;
    Some(base as usize)
}

/// 在总线 0 上扫描指定 vendor/device（fn 0 为主功能），返回设备；未找到 None。
///
/// 扫描 bus 0 全部 32 个 dev × 8 个 fn；`vendor == 0xFFFF` 表示空槽。
pub fn find_device(fdt: &fdt::Fdt, vendor: u16, device: u16) -> Option<PciDev> {
    let ecam = ecam_from_fdt(fdt)?;
    for bus in 0..2u32 {
        for dev in 0..32u32 {
            for func in 0..8u32 {
                let d = PciDev { ecam, bus, dev, func };
                // 读 vendor：0xFFFFFFFF 或 0x00000000 表示无设备
                let vid = d.read32(VENDOR_ID);
                if vid == 0xFFFF_FFFF || vid == 0 {
                    continue;
                }
                if (vid & 0xFFFF) as u16 == vendor && ((vid >> 16) & 0xFFFF) as u16 == device {
                    return Some(d);
                }
            }
        }
    }
    None
}
