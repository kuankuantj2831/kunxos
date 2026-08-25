//! Raspberry Pi 4（BCM2711）平台后端：硬编码 Pi 4 值。
//!
//! 已实现：
//!   - 显示：通过 VideoCore VI mailbox 属性通道向固件申请 HDMI 帧缓冲，
//!     支持 1080p → 720p → 480p 逐级回退，返回真实物理地址 / 分辨率。
//!   - 网络：GENET 千兆以太网（基址 / 中断 / MAC 信息），驱动接线见内核侧。
//!   - 存储：EMMC2 SD 卡（基址 / 中断 / 块大小），驱动接线见内核侧。
//!   - 输入：Goodix GT911（I2C）或 USB HID 尚未实现，暂返回 `None`。
//!
//! 基础设备地址（UART / GIC / RAM / Timer）硬编码自 BCM2711。
//! 中断号取自 Linux `bcm2711.dtsi` 的 `GIC_SPI`（硬件中断号 = 32 + SPI 编号）。

use core::ptr;

use crate::display::DisplayInfo;
use crate::input::InputInfo;
use crate::net::NetInfo;
use crate::plat::Platform;
use crate::storage::StorageInfo;

/// Raspberry Pi 4 平台单例。
pub static PI4: Pi4Platform = Pi4Platform;

/// Raspberry Pi 4（BCM2711）平台参数。
pub struct Pi4Platform;

// BCM2711 外设 MMIO 基地址。
const MBOX_BASE: usize = 0xFE00_B880; // VideoCore VI mailbox
const GENET_BASE: usize = 0xFD58_0000; // 千兆以太网
const EMMC2_BASE: usize = 0xFE34_0000; // SD 卡（EMMC2）
#[allow(dead_code)]
const XHCI_BASE: usize = 0xFE9C_0000; // USB

impl Platform for Pi4Platform {
    fn name(&self) -> &str {
        "Raspberry Pi 4"
    }

    fn uart_base(&self) -> usize {
        0xFE20_1000 // PL011（亦可用 GPIO 14/15 上的 Mini UART）
    }

    fn uart_irq(&self) -> u32 {
        153 // BCM2711 UART0 (PL011) 中断号（SPI 121 + 32）
    }

    fn uart_clock(&self) -> u32 {
        48_000_000 // BCM2711 clk_uart
    }

    fn gicd_base(&self) -> usize {
        0xFF84_1000 // GIC-400 Distributor（BCM2711 实际基址；0xFF840000 是 GIC 保留区起始，差 0x1000）
    }

    fn gicc_base(&self) -> usize {
        0xFF84_2000 // GIC-400 CPU Interface
    }

    fn ram_base(&self) -> usize {
        0x0000_0000
    }

    fn ram_size(&self) -> usize {
        0x4000_0000 // 1GB（实际 1/2/4/8GB，骨架取 1GB）
    }

    fn timer_freq(&self) -> u64 {
        54_000_000 // BCM2711 系统定时器 54MHz
    }

    fn rtc_base(&self) -> Option<usize> {
        // BCM2711 无 PL031；Pi4 的 RTC 通常通过 I2C 外设（如 DS1307）实现，暂不提供。
        None
    }

    fn display_init(&self) -> Option<DisplayInfo> {
        // 经 VideoCore VI mailbox 属性通道申请 HDMI 帧缓冲（见下方实现）。
        mailbox_framebuffer()
    }

    fn net_init(&self) -> Option<NetInfo> {
        // GENET 千兆以太网（MMIO 0xFD580000）。
        // MAC 实际烧录在 OTP / 由 bootloader 写入 DTB，这里先用 Raspberry Pi
        // 官方 OUI 预留的骨架值，待内核侧 GENET 驱动接线后从寄存器读取。
        Some(NetInfo::new(
            [0xDC, 0xA6, 0x32, 0x00, 0x00, 0x01],
            GENET_BASE,
            189, // GIC_SPI 157 + 32（TX/RX 双中断取主中断）
        ))
    }

    fn storage_init(&self) -> Option<StorageInfo> {
        // EMMC2 SD 卡（MMIO 0xFE340000，标准 SDHCI 寄存器布局）。
        // 内核侧块设备驱动尚未接线，返回信息供后续驱动使用。
        Some(StorageInfo::new(
            EMMC2_BASE,
            158, // GIC_SPI 126 + 32
            512,
        ))
    }

    fn input_init(&self) -> Option<InputInfo> {
        // 骨架：BCM2711 触摸输入经 I2C（Goodix GT911，I2C 地址 0x5D）或 USB HID。
        // 驱动尚未实现，暂返回 None（launcher 检测不到 /dev/input/event0 时
        // 回退为模拟点击）。
        None
    }
}

// ==================== VideoCore VI mailbox 属性通道 ====================
//
// Mailbox 位于 0xFE00B880：ARM 侧通过"写 → 读"一对寄存器与 VideoCore 交换
// 消息。属性通道（channel 8）用于向固件查询/请求设备参数（帧缓冲等）。
// 消息缓冲须 16 字节对齐，且地址须为 VC 视角的 bus 地址（0xC0000000 | 物理地址）。

const MBOX_CHANNEL_PROPERTY: u32 = 8;
const MBOX_STATUS_FULL: u32 = 1 << 31; // 写满
const MBOX_STATUS_EMPTY: u32 = 1 << 30; // 读空
const MBOX_BUS_BASE: u32 = 0xC000_0000; // ARM 物理地址 → VC bus 地址偏移

/// 属性消息缓冲（16 字节对齐，256 字节，足够装下帧缓冲申请序列）。
#[repr(align(16))]
struct MboxBuf([u32; 64]);

/// 向 mailbox 写入一个值（等待不忙）。
fn mbox_write(value: u32) -> bool {
    let status = (MBOX_BASE + 0x0C) as *const u32;
    let write = (MBOX_BASE + 0x14) as *mut u32;
    for _ in 0..1_000_000 {
        if unsafe { ptr::read_volatile(status) } & MBOX_STATUS_FULL == 0 {
            unsafe { ptr::write_volatile(write, value) };
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// 从 mailbox 读取一个值（等待不空，仅接收属性通道消息）。
fn mbox_read() -> Option<u32> {
    let status = (MBOX_BASE + 0x0C) as *const u32;
    let read = (MBOX_BASE + 0x00) as *const u32;
    for _ in 0..1_000_000 {
        if unsafe { ptr::read_volatile(status) } & MBOX_STATUS_EMPTY == 0 {
            let data = unsafe { ptr::read_volatile(read) };
            if data & 0xF == MBOX_CHANNEL_PROPERTY {
                return Some(data & !0xF);
            }
            // 其它通道的消息：继续等待
        }
        core::hint::spin_loop();
    }
    None
}

/// 按 cache line（64B）对一段内存执行 DC CVAC（clean 到 PoC，供 VC 读取）。
fn clean_dcache(addr: usize, len: usize) {
    let line = 64usize;
    let start = addr & !(line - 1);
    let end = (addr + len + line - 1) & !(line - 1);
    let mut a = start;
    while a < end {
        unsafe {
            core::arch::asm!("dc cvac, {0}", in(reg) a, options(nostack, preserves_flags));
        }
        a += line;
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// 按 cache line（64B）对一段内存执行 DC IVAC（invalidate，读回 VC 写入）。
fn invalidate_dcache(addr: usize, len: usize) {
    let line = 64usize;
    let start = addr & !(line - 1);
    let end = (addr + len + line - 1) & !(line - 1);
    let mut a = start;
    while a < end {
        unsafe {
            core::arch::asm!("dc ivac, {0}", in(reg) a, options(nostack, preserves_flags));
        }
        a += line;
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// 属性通道调用：`buf[0]` = 消息字节数、`buf[1]` = 0（请求）。
/// 调用前 clean 缓冲（VC 读取），成功后 invalidate 并检查响应码，返回是否成功。
fn property_call(buf: &mut MboxBuf) -> bool {
    let phys = buf as *mut MboxBuf as u32;
    let bus = phys | MBOX_BUS_BASE;
    clean_dcache(phys as usize, 256); // clean 到 PoC，VC 才能看到请求
    if !mbox_write(bus) {
        return false;
    }
    loop {
        match mbox_read() {
            Some(resp) if resp == bus => break,
            Some(_) => continue, // 其它通道响应，丢弃
            None => return false,
        }
    }
    invalidate_dcache(phys as usize, 256); // invalidate，读回 VC 写入的响应
    unsafe { buf.0[1] & 0x8000_0000 != 0 }
}

/// 向属性消息追加一个 tag：`(tag_id, value_size, value_len, values...)`。
fn push_tag(buf: &mut [u32], pos: &mut usize, tag: u32, values: &[u32]) {
    let n = values.len();
    buf[*pos] = tag;
    *pos += 1;
    buf[*pos] = (n * 4) as u32; // value buffer size（字节）
    *pos += 1;
    buf[*pos] = (n * 4) as u32; // value length（请求时 = buffer size）
    *pos += 1;
    for v in values {
        buf[*pos] = *v;
        *pos += 1;
    }
}

/// 申请 HDMI 帧缓冲：尝试 1080p → 720p → 480p，返回首个成功的结果。
fn mailbox_framebuffer() -> Option<DisplayInfo> {
    const MODES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (640, 480)];
    for (w, h) in MODES {
        if let Some(info) = try_framebuffer(w, h) {
            return Some(info);
        }
    }
    None
}

/// 以指定分辨率向固件申请帧缓冲，成功返回 [`DisplayInfo`]。
fn try_framebuffer(w: u32, h: u32) -> Option<DisplayInfo> {
    let mut buf = MboxBuf([0; 64]);
    let mut pos = 2usize; // buf[0] 消息长度、buf[1] 请求码 0，先留空
    push_tag(&mut buf.0, &mut pos, 0x0004_8003, &[w, h]); // 设置物理分辨率
    push_tag(&mut buf.0, &mut pos, 0x0004_8004, &[w, h]); // 设置虚拟分辨率
    push_tag(&mut buf.0, &mut pos, 0x0004_8005, &[32]); // 设置深度 32bpp
    push_tag(&mut buf.0, &mut pos, 0x0004_8009, &[0, 0]); // 虚拟偏移 (0,0)
    push_tag(&mut buf.0, &mut pos, 0x0004_0001, &[4096, 0]); // 分配帧缓冲：对齐 4096，大小 0 由固件决定
    push_tag(&mut buf.0, &mut pos, 0x0004_0008, &[0]); // 获取行字节数（pitch）
    buf.0[pos] = 0; // 结束 tag
    pos += 1;
    buf.0[0] = ((pos * 4 + 15) & !15) as u32; // 消息总字节数（16 对齐）

    if !property_call(&mut buf) {
        return None;
    }

    // 解析响应：遍历 tag，取分配到的 bus 地址 / 大小与 pitch。
    let mut p = 2usize;
    let mut fb_bus = 0u32;
    let mut pitch = 0u32;
    loop {
        let tag = buf.0[p];
        p += 1;
        if tag == 0 {
            break;
        }
        let val_size = buf.0[p];
        p += 1;
        let val_len = buf.0[p];
        p += 1;
        if tag == 0x0004_0001 && val_len >= 12 {
            fb_bus = buf.0[p + 1]; // [对齐, bus 地址, 大小]
        } else if tag == 0x0004_0008 && val_len >= 4 {
            pitch = buf.0[p]; // [行字节数]
        }
        p += (val_size / 4) as usize;
    }

    if fb_bus == 0 {
        return None; // 固件未分配（分辨率不被支持等）
    }
    // VC 返回 bus 地址，低 1GB 掩码还原物理地址（1GB 变体）。
    let fb_phys = (fb_bus & 0x3FFF_FFFF) as usize;
    // pitch 为固件实际行字节数（可能大于 w*4）；DisplayInfo 不含 stride，
    // 内核侧按 bpp=32 使用，保留字段供后续驱动参考。
    let _ = pitch;
    Some(DisplayInfo::new(fb_phys, w, h, 32))
}
