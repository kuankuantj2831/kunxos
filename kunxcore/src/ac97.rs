//! AC97 音频控制器驱动（QEMU `virt` 的 PCI AC97，Intel 82801AA）。
//!
//! 通过 PCI ECAM 层定位设备，操作两段 MMIO：
//! - NAM（Native Audio Mixer，BAR0，16 位寄存器）：复位、音量、采样率。
//! - NABM（Native Audio Bus Master，BAR2）：PCM 输出 DMA。
//!
//! 播放路径（PCM 输出，16 位立体声小端，48kHz）：
//!   1. 混音器复位 + 取消静音 + EACS=0（QEMU 据此以 48kHz 打开输出声道）
//!   2. 生成正弦波采样到 DMA 缓冲，构建 BD（Buffer Descriptor）链表
//!   3. 写 PO_BDBAR / PO_LVI / PO_CR=run 启动播放
//!   4. 轮询 PO_SR 的 SR_DCH（DMA 完成）位直到播放结束
//!
//! BD 格式（QEMU 语义）：每项 8 字节 = { 缓冲物理地址(u32)，ctl_len(u32) }，
//! ctl_len 低 16 位 = 长度（16 位采样单元，立体声一帧 = 2 单元），
//! bit31 = IOC（完成中断，本驱动不用 IRQ）、bit30 = BUP（最后缓冲）。

use core::ptr::{read_volatile, write_volatile};

use crate::allocator;
use crate::fdt;
use crate::pci;

/// Intel 82801AA（ICH）AC97 的 vendor/device id（QEMU "AC97" 设备）。
const VENDOR_AC97: u16 = 0x8086;
const DEVICE_AC97: u16 = 0x2415;

/// QEMU `virt` 机器 PCIe PIO（I/O）窗口的 CPU 基址（设备树 pcie ranges
/// 0x1000000 段：PCI I/O 端口 0 映射到 0x3EFF_0000）。AC97 的 NAM/NABM
/// 均为 I/O 空间 BAR，CPU 地址 = 窗口基址 + PCI I/O 端口号。
const PIO_BASE: usize = 0x3EFF_0000;

const RATE: u32 = 48000;
/// 每块 BD 时长 0.25 秒（12000 帧 × 4 字节 = 48000 字节）。
const CHUNK_FRAMES: usize = 12000;
const CHUNK_BYTES: usize = CHUNK_FRAMES * 4;
/// 最大块数（4 秒）。
const MAX_CHUNKS: usize = 16;

// ---- NAM 混音器寄存器（16 位） ----
const NAM_RESET: usize = 0x00;
const NAM_MASTER_VOL: usize = 0x02;
const NAM_PCM_OUT_VOL: usize = 0x18;
const NAM_EXT_AUDIO_CTRL: usize = 0x28;

// ---- NABM 寄存器 ----
const PO_BDBAR: usize = 0x10; // u32 BD 链表物理地址
const PO_LVI: usize = 0x15; // u8  最后有效 BD 索引
const PO_SR: usize = 0x16; // u16 状态
const PO_CR: usize = 0x1b; // u8  控制

const SR_BCIS: u16 = 8;
const SR_DCH: u16 = 1;

const CR_RPBM: u8 = 1; // run

const BD_IOC: u32 = 1 << 31;
const BD_BUP: u32 = 1 << 30;

/// 设备是否就绪。
static mut READY: bool = false;
static mut NAM: usize = 0;
static mut NABM: usize = 0;
/// 正弦表（2048 项，i16 满幅）。
static mut SINE: [i16; 2048] = [0; 2048];

// ==================== MMIO 原语 ====================

/// 释放 N 个连续页（alloc_pages_contig 的配套：逐页清位图）。
unsafe fn free_pages(addr: usize, n: usize) {
    for i in 0..n {
        allocator::free_page(addr + i * allocator::PAGE_SIZE);
    }
}

#[inline]
unsafe fn r16(addr: usize) -> u16 {
    read_volatile(addr as *const u16)
}
#[inline]
unsafe fn w16(addr: usize, v: u16) {
    write_volatile(addr as *mut u16, v);
}
#[inline]
unsafe fn r8(addr: usize) -> u8 {
    read_volatile(addr as *const u8)
}
#[inline]
unsafe fn w8(addr: usize, v: u8) {
    write_volatile(addr as *mut u8, v);
}
#[inline]
unsafe fn w32a(addr: usize, v: u32) {
    write_volatile(addr as *mut u32, v);
}

/// 按缓存行清洗数据缓存，使 DMA 读取到最新数据。
#[inline]
unsafe fn dcache_clean(va: usize, len: usize) {
    let mut a = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while a < end {
        core::arch::asm!("dc cvac, {}", in(reg) a);
        a += 64;
    }
    core::arch::asm!("dsb sy");
}

// ==================== 正弦波 ====================

/// Bhaskara I 近似正弦：x ∈ [0, π/2]（f32，内核已保存 FP 上下文）。
fn sin_f32(mut x: f32) -> f32 {
    let pi = core::f32::consts::PI;
    let two_pi = 2.0 * pi;
    x %= two_pi;
    if x < 0.0 {
        x += two_pi;
    }
    let neg = x > pi;
    if neg {
        x -= pi;
    }
    if x > pi / 2.0 {
        x = pi - x;
    }
    let num = 16.0 * x * (pi - x);
    let den = 5.0 * pi * pi - 4.0 * x * (pi - x);
    let v = num / den;
    if neg {
        -v
    } else {
        v
    }
}

fn build_sine_table() {
    unsafe {
        let n = SINE.len();
        for i in 0..n {
            let v = sin_f32(2.0 * core::f32::consts::PI * i as f32 / n as f32);
            SINE[i] = (v * 32767.0) as i16;
        }
    }
}

// ==================== 初始化 ====================

/// 初始化 AC97：定位 PCI 设备、映射 BAR、复位混音器、构建正弦表。
/// 成功返回 true；找不到设备返回 false。
pub fn init(fdt: &fdt::Fdt) -> bool {
    let Some(dev) = pci::find_device(fdt, VENDOR_AC97, DEVICE_AC97) else {
        return false;
    };
    unsafe {
        dev.enable_mem_master();
        // QEMU gpex 无固件做资源分配：I/O BAR 初始全为 0，需驱动自行分配
        // PCI I/O 端口（写入 BAR 寄存器后 QEMU 即把该端口路由到设备）。
        // 选择 0x400/0x800（PIO 窗口 0x3EFF_0000 内，无其他 I/O 占用者）。
        const NAM_PORT: u32 = 0x400;
        const NABM_PORT: u32 = 0x800;
        dev.write32(crate::pci::BAR0, NAM_PORT | 1);
        dev.write32(crate::pci::BAR1, NABM_PORT | 1);
        let nam = PIO_BASE + NAM_PORT as usize;
        let nabm = PIO_BASE + NABM_PORT as usize;
        NAM = nam;
        NABM = nabm;

        // 混音器：复位 → 取消静音 → 固定 48kHz 输出
        w16(nam + NAM_RESET, 0x0000);
        w16(nam + NAM_MASTER_VOL, 0x0000); // 主音量最大、不静音
        w16(nam + NAM_PCM_OUT_VOL, 0x0000); // PCM 音量最大、不静音
        w16(nam + NAM_EXT_AUDIO_CTRL, 0x0000); // VRA=0：QEMU 以 48kHz 打开输出声道

        build_sine_table();
        READY = true;
    }
    crate::kinfo! {
        crate::uart_send_str("[ac97] AC97 音频就绪 (NAM=");
        crate::print_hex(unsafe { NAM } as u64);
        crate::uart_send_str(" NABM=");
        crate::print_hex(unsafe { NABM } as u64);
        crate::uart_send_str(")\n");
    }
    true
}

// ==================== 播放 ====================

/// 合成 `freq` Hz 正弦波（16 位立体声 48kHz，带 5ms 淡入淡出防爆音），
/// 填充到 `buf`（`frames` 帧 = `frames*4` 字节），返回总帧数。
fn synthesize(freq: u32, frames: usize, buf: &mut [u8]) -> usize {
    if freq == 0 || frames == 0 {
        return 0;
    }
    // 相位累加（步进 = freq/48000 圈 × 2048 表长；u32 回绕对 2048 取模无影响）
    let step: u32 = (freq as u64 * 2048 / RATE as u64) as u32;
    if step == 0 {
        return 0;
    }
    let fade = (RATE as usize / 200).min(frames / 2); // 5ms
    let mut phase: u32 = 0;
    for i in 0..frames {
        // 相位累加（每样本前进 step 个表项），取表长 2048 的余数为索引
        let idx = (phase & 2047) as usize;
        let mut v = unsafe { SINE[idx] as i32 };
        // 0.4 幅度
        v = (v * 13107) >> 15;
        // 淡入淡出
        if i < fade {
            v = v * i as i32 / fade as i32;
        } else if i >= frames - fade {
            v = v * (frames - 1 - i) as i32 / fade as i32;
        }
        let s = v.clamp(-32767, 32767) as i16;
        let off = i * 4;
        buf[off] = (s as u16 & 0xFF) as u8; // 左声道
        buf[off + 1] = ((s as u16 >> 8) & 0xFF) as u8;
        buf[off + 2] = (s as u16 & 0xFF) as u8; // 右声道
        buf[off + 3] = ((s as u16 >> 8) & 0xFF) as u8;
        phase = phase.wrapping_add(step);
    }
    frames
}

/// 播放 `freq` Hz 音调 `dur_ms` 毫秒（上限 4 秒）。成功返回 0；失败 -1。
///
/// 同步阻塞：轮询 PO_SR 直到 DMA 完成（QEMU 实时播放，1 秒音调约耗时 1 秒）。
pub fn play_tone(freq: u32, dur_ms: u32) -> i32 {
    if !unsafe { READY } {
        return -1;
    }
    let dur_ms = dur_ms.clamp(10, 4000);
    let total_frames = (RATE as u64 * dur_ms as u64 / 1000) as usize;
    let chunks = (total_frames + CHUNK_FRAMES - 1) / CHUNK_FRAMES;
    let chunks = chunks.clamp(1, MAX_CHUNKS);
    let frames = chunks * CHUNK_FRAMES;

    // 分配 DMA 缓冲（音频数据 + BD 链表），恒等映射下虚拟地址即物理地址
    let audio_bytes = frames * 4;
    let audio = match allocator::alloc_pages_contig(audio_bytes.div_ceil(allocator::PAGE_SIZE)) {
        Some(a) => a,
        None => return -1,
    };
    let bd_pages = (chunks * 8).div_ceil(allocator::PAGE_SIZE);
    let bd_list = match allocator::alloc_pages_contig(bd_pages) {
        Some(b) => b,
        None => {
            unsafe { free_pages(audio, audio_bytes.div_ceil(allocator::PAGE_SIZE)) };
            return -1;
        }
    };

    // 合成音频并构建 BD 链表
    let audio_slice = unsafe { core::slice::from_raw_parts_mut(audio as *mut u8, audio_bytes) };
    synthesize(freq, frames, audio_slice);
    for c in 0..chunks {
        let addr = (audio + c * CHUNK_BYTES) as u32;
        let len = (CHUNK_FRAMES * 2) as u32; // 16 位采样单元（立体声一帧 = 2）
        let mut ctl = len;
        if c == chunks - 1 {
            ctl |= BD_IOC | BD_BUP;
        }
        unsafe {
            let base = bd_list as *mut u8;
            *(base.add(c * 8) as *mut u32) = addr;
            *(base.add(c * 8 + 4) as *mut u32) = ctl;
        }
    }
    unsafe {
        dcache_clean(audio, audio_bytes);
        dcache_clean(bd_list, chunks * 8);
    }

    // 启动播放：BDBAR → LVI → CR=run
    unsafe {
        w32a(NABM + PO_BDBAR, bd_list as u32);
        w8(NABM + PO_LVI, (chunks - 1) as u8);
        w8(NABM + PO_CR, CR_RPBM);
    }

    // 轮询直到 SR_DCH（DMA 完成）或超时（时长 + 2s）
    let deadline = crate::timer::now_us() + dur_ms as u64 * 1000 + 2_000_000;
    let mut done = false;
    loop {
        let sr = unsafe { r16(NABM + PO_SR) };
        if sr & SR_DCH != 0 {
            done = true;
            break;
        }
        if crate::timer::now_us() >= deadline {
            break;
        }
        // 让出 CPU（定时器 IRQ 也会抢占），避免长时间独占
        if unsafe { READY } {
            crate::sched_cfs::yield_now();
        }
    }

    unsafe {
        w8(NABM + PO_CR, 0); // 停止
        free_pages(audio, audio_bytes.div_ceil(allocator::PAGE_SIZE));
        free_pages(bd_list, bd_pages);
    }
    if !done {
        crate::uart_send_str("[ac97] 播放超时 (设备可能缺少后端)\n");
        return -1;
    }
    0
}
