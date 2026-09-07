//! 开机启动画面：fw_cfg/ramfb 帧缓冲、KunxOS Logo 位图渲染与内核交接信息。
//!
//! QEMU virt 经 `-device ramfb` 暴露 RAM 帧缓冲（fw_cfg 配置通道 MMIO
//! 0x0902_0000）；未就绪（或 Pi 4 无 fw_cfg）时回退为串口输出 "KunxOS"。

#![allow(dead_code)]

#[cfg(not(feature = "pi4"))]
use core::ptr::{read_volatile, write_volatile};

// ==================== fw_cfg / ramfb 常量 ====================

/// fw_cfg MMIO 基地址（QEMU virt）。
#[cfg(not(feature = "pi4"))]
const FWCFG_BASE: usize = 0x0902_0000;

/// 文件目录选择键。
#[cfg(not(feature = "pi4"))]
const FW_CFG_FILE_DIR: u16 = 0x19;
/// 写通道标志。
#[cfg(not(feature = "pi4"))]
const FW_CFG_WRITE_CHANNEL: u16 = 0x4000;
/// fw_cfg DMA 控制位。
#[cfg(not(feature = "pi4"))]
const DMA_CTL_SELECT: u32 = 0x08;
#[cfg(not(feature = "pi4"))]
const DMA_CTL_WRITE: u32 = 0x10;
/// DRM_FORMAT_XRGB8888。
#[cfg(not(feature = "pi4"))]
const XRGB8888: u32 = 0x3432_5258;

/// 启动画面帧缓冲物理地址（RAMDISK_LOAD_ADDR=0x4700_0000 下方空白区；
/// bootloader 位于 0x46000000 起向上增长，0x46A0_0000 起连续 ~2MB 帧缓冲
/// 落在 0x46F0_0000 握手区之前，互不重叠）。
pub const SPLASH_FB_ADDR: usize = 0x46A0_0000;
/// 启动画面交接信息物理地址。
pub const SPLASH_HANDOFF_ADDR: usize = 0x46F0_0000;

#[cfg(not(feature = "pi4"))]
const FB_W: u32 = 540;
#[cfg(not(feature = "pi4"))]
const FB_H: u32 = 960;
#[cfg(not(feature = "pi4"))]
const FB_STRIDE: u32 = 540 * 4;

/// Logo 位图尺寸（1 位/像素，MSB 为最左像素）。
#[cfg(not(feature = "pi4"))]
const LOGO_W: u32 = 32;
#[cfg(not(feature = "pi4"))]
const LOGO_H: u32 = 32;
#[cfg(not(feature = "pi4"))]
const LOGO: [u32; 32] = [
    0x00038000, 0x0007C000, 0x001EF000, 0x00783C00, 0x01E00F00, 0x07E003C0, 0x0FE001E0, 0x3CE00078,
    0x70E0001C, 0x60E0000C, 0x60FC000C, 0x60FF800C, 0x60FFF00C, 0x60EFFF0C, 0x60E1FFCC, 0x60E03FCC,
    0x60E03FCC, 0x60E1FFCC, 0x60EFFF0C, 0x60FFF00C, 0x60FF800C, 0x60FC000C, 0x70E0001C, 0x3CE00078,
    0x0FE001E0, 0x07E003C0, 0x01E00F00, 0x00783C00, 0x001EF000, 0x0007C000, 0x00038000, 0x00000000,
];

/// 交接信息：内核据此沿用 bootloader 渲染的帧缓冲并做淡出过渡。
#[repr(C)]
pub struct SplashHandoff {
    pub magic: u32, // 0x4B4E_5853 ("KNXS")
    pub fb_addr: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub logo_x: u32,
    pub logo_y: u32,
    pub logo_w: u32,
    pub logo_h: u32,
    pub active: u8,
    _pad: [u8; 3],
}

#[cfg(not(feature = "pi4"))]
const HANDOFF_MAGIC: u32 = 0x4B4E_5853;

// ==================== fw_cfg 状态 ====================

#[cfg(not(feature = "pi4"))]
#[repr(C)]
struct DmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

#[cfg(not(feature = "pi4"))]
static mut RAMFB_SEL: u16 = 0;
#[cfg(not(feature = "pi4"))]
static mut DMA: DmaAccess = DmaAccess { control: 0, length: 0, address: 0 };
#[cfg(not(feature = "pi4"))]
static mut CFG: [u8; 28] = [0; 28];

// ==================== fw_cfg MMIO 原语 ====================

#[cfg(not(feature = "pi4"))]
unsafe fn fwcfg_select(key: u16) {
    write_volatile((FWCFG_BASE + 0x08) as *mut u16, key.swap_bytes());
}

#[cfg(not(feature = "pi4"))]
unsafe fn fwcfg_read_byte() -> u8 {
    read_volatile(FWCFG_BASE as *const u8)
}

#[cfg(not(feature = "pi4"))]
unsafe fn fwcfg_read(out: &mut [u8]) {
    for b in out.iter_mut() {
        *b = fwcfg_read_byte();
    }
}

/// 经 fw_cfg DMA 传输（带超时防护，避免设备缺失时死循环）。
#[cfg(not(feature = "pi4"))]
unsafe fn fwcfg_dma(key: u16, buf_pa: usize, len: usize, write: bool) -> bool {
    let mut ctl = DMA_CTL_SELECT | ((key as u32) << 16);
    ctl |= if write { DMA_CTL_WRITE } else { 0 };

    let initial = ctl.to_be();
    DMA.control = initial;
    DMA.length = (len as u32).to_be();
    DMA.address = (buf_pa as u64).to_be();
    core::arch::asm!("dsb sy");
    let dma_pa = core::ptr::addr_of!(DMA) as usize;
    write_volatile((FWCFG_BASE + 0x10) as *mut u64, (dma_pa as u64).swap_bytes());

    let mut spins = 0usize;
    while read_volatile(&DMA.control) == initial {
        core::hint::spin_loop();
        spins += 1;
        if spins > 200_000_000 {
            return false;
        }
    }
    read_volatile(&DMA.control) == 0
}

// ==================== ramfb ====================

/// 在 fw_cfg 文件目录中查找 `etc/ramfb` 的选择键（未找到返回 0）。
#[cfg(not(feature = "pi4"))]
unsafe fn find_ramfb_selector() -> u16 {
    fwcfg_select(FW_CFG_FILE_DIR);
    let mut cbuf = [0u8; 4];
    fwcfg_read(&mut cbuf);
    let count = u32::from_be_bytes(cbuf) as usize;
    // 条目数上限：设备异常/返回垃圾大数时避免数十亿次 MMIO 读等同挂死
    if count > 0x10000 {
        return 0;
    }

    for _ in 0..count {
        let mut e = [0u8; 64];
        fwcfg_read(&mut e);
        let sel = u16::from_be_bytes([e[4], e[5]]);
        let mut nlen = 0usize;
        while nlen < 56 && e[8 + nlen] != 0 {
            nlen += 1;
        }
        if &e[8..8 + nlen] == b"etc/ramfb" {
            return sel;
        }
    }
    0
}

/// 构造并写入 `etc/ramfb` 配置，把扫描输出切换到 `pa` 处的帧缓冲。
#[cfg(not(feature = "pi4"))]
unsafe fn write_ramfb_cfg(pa: usize, width: u32, height: u32, stride: u32) {
    CFG[0..8].copy_from_slice(&(pa as u64).to_be_bytes());
    CFG[8..12].copy_from_slice(&XRGB8888.to_be_bytes());
    CFG[12..16].copy_from_slice(&0u32.to_be_bytes());
    CFG[16..20].copy_from_slice(&width.to_be_bytes());
    CFG[20..24].copy_from_slice(&height.to_be_bytes());
    CFG[24..28].copy_from_slice(&stride.to_be_bytes());
    fwcfg_dma(RAMFB_SEL | FW_CFG_WRITE_CHANNEL, core::ptr::addr_of!(CFG) as usize, 28, true);
}

// ==================== 渲染 ====================

/// 清屏为纯黑（背景色 #000000）。
#[cfg(not(feature = "pi4"))]
unsafe fn fill_black(fb: *mut u32) {
    let n = (FB_W * FB_H) as usize;
    for i in 0..n {
        *fb.add(i) = 0;
    }
}

/// 把 32x32 Logo 位图最近邻缩放到 `size x size` 并居中绘制（白色像素）。
#[cfg(not(feature = "pi4"))]
unsafe fn blit_logo(fb: *mut u32, lx: u32, ly: u32, size: u32) {
    for dy in 0..size {
        let sy = ((dy as u64) * (LOGO_H as u64)) / (size as u64);
        let row = fb.add(((ly + dy) as usize) * (FB_W as usize) + lx as usize);
        for dx in 0..size {
            let sx = ((dx as u64) * (LOGO_W as u64)) / (size as u64);
            let lit = (LOGO[sy as usize] >> (31 - sx as u32)) & 1;
            if lit == 1 {
                *row.add(dx as usize) = 0x00FF_FFFF;
            }
        }
    }
}

/// 显示启动画面：DDR 初始化完成后立即调用，不等待文件系统/网络/其他驱动。
///
/// ramfb 就绪时渲染黑底 + 居中 KunxOS Logo（尺寸 = 屏幕短边的 25%，保持宽高比），
/// 并把帧缓冲几何与 Logo 位置写入交接信息供内核淡出过渡；未就绪时回退为
/// 串口输出 "KunxOS"。返回 ramfb 是否就绪。
pub fn show() -> bool {
    #[cfg(feature = "pi4")]
    {
        crate::print!("KunxOS\n");
        return false;
    }
    #[cfg(not(feature = "pi4"))]
    unsafe {
        RAMFB_SEL = find_ramfb_selector();
        if RAMFB_SEL == 0 {
            crate::print!("KunxOS\n");
            return false;
        }

        // 边界检查：帧缓冲区域（FB_W×FB_H×4）必须完整落在握手区之前的安全带内，
        // 避免与 SPLASH_HANDOFF_ADDR / RAMDISK_LOAD_ADDR 重叠。
        let fb_bytes = (FB_W as usize) * (FB_H as usize) * 4;
        if SPLASH_FB_ADDR + fb_bytes > SPLASH_HANDOFF_ADDR {
            crate::print!("[boot] splash：帧缓冲溢出交接区\n");
            return false;
        }
        let fb = SPLASH_FB_ADDR as *mut u32;
        fill_black(fb);

        let size = (FB_W.min(FB_H) * 25) / 100; // 屏幕短边的 25%
        let lx = (FB_W - size) / 2;
        let ly = (FB_H - size) / 2;
        blit_logo(fb, lx, ly, size);

        write_ramfb_cfg(SPLASH_FB_ADDR, FB_W, FB_H, FB_STRIDE);

        let ho = SPLASH_HANDOFF_ADDR as *mut SplashHandoff;
        (*ho).magic = HANDOFF_MAGIC;
        (*ho).fb_addr = SPLASH_FB_ADDR as u64;
        (*ho).width = FB_W;
        (*ho).height = FB_H;
        (*ho).stride = FB_STRIDE;
        (*ho).logo_x = lx;
        (*ho).logo_y = ly;
        (*ho).logo_w = size;
        (*ho).logo_h = size;
        (*ho).active = 1;
        true
    }
}
