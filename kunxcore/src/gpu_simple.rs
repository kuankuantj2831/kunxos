//! 显示后端：优先 virtio-gpu 2D，无设备或初始化失败时回退 QEMU ramfb。
//! 应用仍使用 XRGB8888 /dev/fb0；两种后端均使用软件合成，无硬件 VSYNC。
//!
//! `-M virt` 下添加 `-device ramfb` 时，QEMU 通过 fw_cfg 暴露一个纯内存
//! 帧缓冲。内核通过 fw_cfg DMA 接口向 `etc/ramfb` 文件写入 `RAMFBCfg`
//! 结构（字段大端），指定帧缓冲物理地址、分辨率、stride 与像素格式，
//! QEMU 即把该段内存扫描输出到显示器。配置写入会重建宿主显示表面，
//! 因此正常呈现复用固定扫描缓冲，仅初始化或模式改变时写入配置。
//!
//! 参考（QEMU 源码）：
//!   - `hw/nvram/fw_cfg.c`：fw_cfg MMIO 布局 data@+0 / selector@+8 / dma@+16，
//!     selector 与 DMA 寄存器为 DEVICE_BIG_ENDIAN，数据寄存器按字节顺序返回。
//!   - `hw/display/ramfb.c`：`RAMFBCfg` 布局（28 字节，大端），写入触发扫描输出切换。

#![allow(static_mut_refs)]

use core::arch::asm;
use core::ptr::{addr_of, read_volatile, write_volatile};

use crate::allocator;
use core::sync::atomic::{AtomicU32, Ordering};

static REFRESH_MILLIHZ: AtomicU32 = AtomicU32::new(0);
static DENSITY: AtomicU32 = AtomicU32::new(1024);
static HOST_CURSOR: AtomicU32 = AtomicU32::new(0);
static RESIZE_ENABLED: AtomicU32 = AtomicU32::new(0);
static RESIZE_REQUEST: AtomicU32 = AtomicU32::new(0);
static GENERATION: AtomicU32 = AtomicU32::new(1);
static PRESENTS: AtomicU32 = AtomicU32::new(0);
static RECONFIGURATIONS: AtomicU32 = AtomicU32::new(0);
// 0 = unavailable, 1 = ramfb, 2 = virtio-gpu. Explicit selection is useful
// for old firmware, performance comparisons and machines exposing both devices.
static BACKEND: AtomicU32 = AtomicU32::new(0);
static FORCE_RAMFB: AtomicU32 = AtomicU32::new(0);
static mut SCANOUT: (usize, u32, u32, u32) = (0, 0, 0, 0);

pub fn backend_name() -> &'static str {
    match BACKEND.load(Ordering::Relaxed) {
        1 => "ramfb", 2 => "virtio-gpu", _ => "none",
    }
}

/// virtio-gpu snapshots backing memory into a host resource before completing
/// its fence. The compositor may reuse its composition buffer afterwards.
pub fn uses_host_resource() -> bool {
    BACKEND.load(Ordering::Relaxed) == 2
}

pub fn scanout_stats() -> (u32, u32) {
    (PRESENTS.load(Ordering::Relaxed), RECONFIGURATIONS.load(Ordering::Relaxed))
}

// The compositor serializes presentation. Retain a surface until its binding changes.
unsafe fn bind_scanout(pa: usize, damage: Option<crate::virtio_gpu_protocol::Damage>) {
    PRESENTS.fetch_add(1, Ordering::Relaxed);
    asm!("fence iorw, iorw");
    if BACKEND.load(Ordering::Relaxed) == 2 {
        if let Some(changed) = crate::virtio_gpu::present(pa, WIDTH, HEIGHT, damage) {
            if changed { RECONFIGURATIONS.fetch_add(1, Ordering::Relaxed); }
            return;
        }
        // The GPU keeps all DMA pages pinned after a failure. The same front
        // buffer can be scanned by ramfb when the machine exposes that device.
        BACKEND.store(if RAMFB_SEL != 0 { 1 } else { 0 }, Ordering::Relaxed);
        crate::uart_send_str("[gpu] display fallback: ");
        crate::uart_send_str(backend_name());
        crate::uart_send_str("\n");
    }
    if BACKEND.load(Ordering::Relaxed) != 1 { return; }
    let binding = (pa, WIDTH, HEIGHT, STRIDE);
    if SCANOUT != binding && write_ramfb_cfg(pa, WIDTH, HEIGHT, STRIDE) {
        SCANOUT = binding;
        RECONFIGURATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn request_resize(w: u32, h: u32) {
    if RESIZE_ENABLED.load(Ordering::Relaxed) != 0
        && (360..=1920).contains(&w) && (480..=1200).contains(&h) {
        RESIZE_REQUEST.store((w << 16) | h, Ordering::Release);
    }
}

/// Constant backing pages keep all existing fb0 mappings valid across modesets.
pub fn backing_len() -> usize {
    if RESIZE_ENABLED.load(Ordering::Relaxed) != 0 { 1920 * 1200 * 4 } else { fb_len() }
}

pub fn commit_resize() -> i32 {
    let packed = RESIZE_REQUEST.load(Ordering::Acquire);
    if packed == 0 { return 0; }
    let (w, h) = (packed >> 16, packed & 0xffff);
    if w == width() && h == height() {
        let _ = RESIZE_REQUEST.compare_exchange(packed, 0, Ordering::AcqRel, Ordering::Relaxed);
        return 0;
    }
    if !crate::compositor::resize(w, h) { return -16; }
    let _ = RESIZE_REQUEST.compare_exchange(packed, 0, Ordering::AcqRel, Ordering::Relaxed);
    0
}

pub unsafe fn set_geometry(w: u32, h: u32) {
    WIDTH = w; HEIGHT = h; STRIDE = w * 4;
    GENERATION.fetch_add(1, Ordering::Release);
}

/// Neither current backend implements a VSYNC interrupt or a host resize event.
/// Keep both capability bits clear until a display driver implements them.
pub fn configure_display(info: Option<&kunxhal::DisplayInfo>) {
    if let Some(info) = info {
        REFRESH_MILLIHZ.store(info.refresh_millihz, Ordering::Relaxed);
        DENSITY.store(info.density.clamp(256, 8192), Ordering::Relaxed);
    }
}

/// Explicit QEMU host hint, not a hardware refresh measurement.
/// Used only when the display backend cannot report a native rate.
static HOST_RATE_MILLIHZ: AtomicU32 = AtomicU32::new(0);
pub fn configure_host_rate(args: &str) {
    for arg in args.split_whitespace() {
        if arg == "display-backend=ramfb" { FORCE_RAMFB.store(1, Ordering::Relaxed); }
        if arg == "host-cursor" { HOST_CURSOR.store(4, Ordering::Relaxed); }
        if arg == "display-resize" { RESIZE_ENABLED.store(1, Ordering::Relaxed); }
        if let Some(hz) = arg.strip_prefix("display-hz=").and_then(|s| s.parse::<u32>().ok()) {
            if (24..=360).contains(&hz) {
                HOST_RATE_MILLIHZ.store(hz * 1000, Ordering::Relaxed);
            }
        }
    }
}

/// 未知刷新率时的软件目标帧率（mHz）。
///
/// ramfb 查不到刷新率、也没有 `display-hz=` 提示时才使用，只是一个目标，
/// 不是实测值。取 120Hz 而非 240Hz：QEMU TCG（Windows/x86 宿主无 KVM）下
/// 每次定时器中断往返都是纯软件模拟，240Hz 的 IRQ 密度本身就会吃掉大量
/// CPU，留给合成与应用渲染的时间反而更少。需要更高目标请显式传
/// `display-hz=<N>`（真机/KVM 宿主）。
const FALLBACK_TARGET_MILLIHZ: u32 = 120_000;

pub fn frame_interval_us() -> u64 {
    let native = REFRESH_MILLIHZ.load(Ordering::Relaxed);
    let host = HOST_RATE_MILLIHZ.load(Ordering::Relaxed);
    let rate = if native != 0 { native } else if host != 0 { host } else { FALLBACK_TARGET_MILLIHZ };
    1_000_000_000u64.div_ceil(rate.clamp(1_000, 1_000_000) as u64)
}

pub fn display_metrics() -> kunxhal::display::protocol::DisplayMetrics {
    kunxhal::display::protocol::DisplayMetrics {
        generation: GENERATION.load(Ordering::Acquire), width: width(), height: height(), stride_bytes: stride(),
        density: DENSITY.load(Ordering::Relaxed),
        refresh_millihz: REFRESH_MILLIHZ.load(Ordering::Relaxed),
        target_millihz: (1_000_000_000 / crate::power::anim_frame_interval_us()) as u32,
        capabilities: (if RESIZE_ENABLED.load(Ordering::Relaxed) != 0 { 2 } else { 0 })
            | HOST_CURSOR.load(Ordering::Relaxed),
    }
}

/// 默认渲染分辨率（竖屏手机比例，540x960）。
pub const DEFAULT_WIDTH: u32 = 540;
pub const DEFAULT_HEIGHT: u32 = 960;

// ==================== fw_cfg 常量 ====================

/// 文件目录选择键（FW_CFG_FILE_DIR）。
const FW_CFG_FILE_DIR: u16 = 0x19;
/// 写通道标志（与选择键按位或）。
const FW_CFG_WRITE_CHANNEL: u16 = 0x4000;

// fw_cfg DMA 控制位
#[allow(dead_code)]
const DMA_CTL_ERROR: u32 = 0x01;
const DMA_CTL_READ: u32 = 0x02;
const DMA_CTL_SELECT: u32 = 0x08;
const DMA_CTL_WRITE: u32 = 0x10;

/// DRM_FORMAT_XRGB8888（fourcc "XR24"）。
const XRGB8888: u32 = 0x3432_5258;

// ==================== 全局状态 ====================

/// fw_cfg MMIO 基地址（默认 QEMU virt 值，可被设备树覆盖）。
static mut FWCFG_BASE: usize = 0x0902_0000;
/// `etc/ramfb` 的选择键（0 表示未找到设备）。
static mut RAMFB_SEL: u16 = 0;
/// 帧缓冲几何（由 init 设置）。
static mut WIDTH: u32 = 0;
static mut HEIGHT: u32 = 0;
static mut STRIDE: u32 = 0;

/// fw_cfg DMA 访问结构（QEMU 在传输完成后回写 control 字段）。
#[repr(C)]
struct DmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

static mut DMA: DmaAccess = DmaAccess { control: 0, length: 0, address: 0 };
/// ramfb 配置写缓冲（`RAMFBCfg`，28 字节，字段大端）。
static mut CFG: [u8; 28] = [0; 28];

// ==================== fw_cfg MMIO 原语 ====================

/// 选择 fw_cfg 条目。
///
/// # 安全
/// 需要有效映射的 `FWCFG_BASE`。
unsafe fn fwcfg_select(key: u16) {
    // selector 寄存器为 DEVICE_BIG_ENDIAN：写大端 16 位
    //（QEMU fw_cfg MMIO 布局：data@+0 / selector@+8 / dma@+0x10）
    write_volatile((FWCFG_BASE + 0x08) as *mut u16, key.swap_bytes());
}

/// 从数据寄存器读取一个字节（流中的下一个字节）。
///
/// # 安全
/// 必须先选择条目。
unsafe fn fwcfg_read_byte() -> u8 {
    read_volatile(FWCFG_BASE as *const u8)
}

/// 读取当前选中条目的 `n` 个字节到 `out`。
///
/// # 安全
/// 调用前必须先 `fwcfg_select`。
unsafe fn fwcfg_read(out: &mut [u8]) {
    for b in out.iter_mut() {
        *b = fwcfg_read_byte();
    }
}

/// 通过 fw_cfg DMA 接口读写条目。
///
/// `key` 为选择键，`buf_pa` 为数据缓冲区物理地址，`len` 为字节数，
/// `write` 为真时把缓冲区内容写入条目（触发写回调），否则从条目读入缓冲区。
/// 返回是否成功（control 被回写为 0）。
///
/// # 安全
/// `buf_pa` 必须指向可被 QEMU DMA 访问的物理内存。
unsafe fn fwcfg_dma(key: u16, buf_pa: usize, len: usize, write: bool) -> bool {
    let mut ctl = DMA_CTL_SELECT | ((key as u32) << 16);
    ctl |= if write { DMA_CTL_WRITE } else { DMA_CTL_READ };

    let initial = ctl.to_be();
    DMA.control = initial;
    DMA.length = (len as u32).to_be();
    DMA.address = (buf_pa as u64).to_be();
    // 保证 QEMU DMA 能看到写入的结构
    asm!("fence iorw, iorw");

    // 触发传输：把 DMA 结构的物理地址（大端）写入 DMA 寄存器
    let dma_pa = addr_of!(DMA) as usize;
    write_volatile((FWCFG_BASE + 0x10) as *mut u64, (dma_pa as u64).swap_bytes());

    // 等待 QEMU 回写 control（成功为 0，失败带 error 位）；加超时防护，
    // 避免设备异常时（MMIO 未映射/无 fw_cfg）死循环。
    let mut spins = 0usize;
    while read_volatile(&DMA.control) == initial {
        core::hint::spin_loop();
        spins += 1;
        if spins > 200_000_000 {
            return false; // 超时（约数百 ms）
        }
    }
    asm!("fence iorw, iorw");
    read_volatile(&DMA.control) == 0
}

// ==================== ramfb ====================

/// 在 fw_cfg 文件目录中查找 `etc/ramfb`，返回其选择键（未找到返回 0）。
unsafe fn find_ramfb_selector() -> u16 {
    fwcfg_select(FW_CFG_FILE_DIR);

    // 目录头：条目数（大端 u32）
    let mut cbuf = [0u8; 4];
    fwcfg_read(&mut cbuf);
    let count = u32::from_be_bytes(cbuf) as usize;

    for _ in 0..count {
        // 每条目 64 字节：size(u32) + select(u16) + reserved(u16) + name[56]
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
unsafe fn write_ramfb_cfg(pa: usize, width: u32, height: u32, stride: u32) -> bool {
    // RAMFBCfg：addr(be64) fourcc(be32) flags(be32) width(be32) height(be32) stride(be32)
    CFG[0..8].copy_from_slice(&(pa as u64).to_be_bytes());
    CFG[8..12].copy_from_slice(&XRGB8888.to_be_bytes());
    CFG[12..16].copy_from_slice(&0u32.to_be_bytes()); // flags
    CFG[16..20].copy_from_slice(&width.to_be_bytes());
    CFG[20..24].copy_from_slice(&height.to_be_bytes());
    CFG[24..28].copy_from_slice(&stride.to_be_bytes());

    // 写通道：selector | WRITE_CHANNEL，经 DMA 写入触发 QEMU 更新扫描输出
    fwcfg_dma(RAMFB_SEL | FW_CFG_WRITE_CHANNEL, addr_of!(CFG) as usize, 28, true)
}

/// 帧缓冲是否需要显式 clean（dc cvac）后才能翻转。
///
/// 只有真实硬件（Pi4：显示控制器从物理 RAM 扫描输出）才必须在翻转前把写入
/// clean 回内存；QEMU 的 ramfb 是共享 RAM（TCG 无脏实现、KVM 也经宿主共享
/// 内存），clean 纯属开销——整屏脏区时一次 `present` 要执行 ~3.2 万条 `dc cvac`
/// + `dsb`，在慢宿主上每帧都是可见的固定成本。按平台名称跳过，不改变 540x960
/// 双缓冲翻转的语义（QEMU 侧缓存一致性由宿主机保证）。
/// 平台判定缓存：present_rects 每矩形各调一次，此前每次都做平台名
/// 字符串 contains 扫描。
fn clean_needed() -> bool {
    // 0=未知 1=不需要 2=需要（avoid bool 静态初始化歧义）
    static mut CLEAN_NEEDED: i8 = 0;
    unsafe {
        if CLEAN_NEEDED == 0 {
            CLEAN_NEEDED = if kunxhal::current_platform().name().contains("QEMU") {
                1
            } else {
                2
            };
        }
        CLEAN_NEEDED == 2
    }
}

/// 按虚拟地址清理数据缓存（写回内存，使扫描输出/DMA 可见）。
///
/// 真实硬件（KVM/Pi4）上帧缓冲写入会被缓存，翻转前必须 clean；QEMU 下跳过
/// （见 [`clean_needed`]）。
fn clean_dcache(va: usize, len: usize) {
    if !clean_needed() {
        return;
    }
    let mut a = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while a < end {
        unsafe {
            asm!("fence iorw, iorw");
        }
        a += 64;
    }
    unsafe {
        asm!("fence iorw, iorw");
    }
}

// ==================== 公共接口 ====================

/// 初始化帧缓冲驱动。
///
/// * `fwcfg_base` - fw_cfg MMIO 基地址（设备树解析所得，缺失时回退默认值）。
///
/// 优先探测 virtio-gpu，失败时使用 ramfb；均不可用时返回 `None`。
/// 成功返回 `(宽, 高)`，应用的 framebuffer ABI 与后端无关。
pub fn init(fwcfg_base: usize, devs: &[crate::net_virtio::MmioDev]) -> Option<(u32, u32)> {
    unsafe {
        FWCFG_BASE = fwcfg_base;
        RAMFB_SEL = find_ramfb_selector();
    }
    let virtio = FORCE_RAMFB.load(Ordering::Relaxed) == 0 && crate::virtio_gpu::init(devs);
    if !virtio && unsafe { RAMFB_SEL } == 0 {
        return None;
    }
    BACKEND.store(if virtio { 2 } else { 1 }, Ordering::Relaxed);
    unsafe {
        WIDTH = DEFAULT_WIDTH;
        HEIGHT = DEFAULT_HEIGHT;
        STRIDE = DEFAULT_WIDTH * 4;
    }
    Some((DEFAULT_WIDTH, DEFAULT_HEIGHT))
}

/// 当前帧缓冲宽度。
pub fn width() -> u32 {
    unsafe { WIDTH }
}

/// 当前帧缓冲高度。
pub fn height() -> u32 {
    unsafe { HEIGHT }
}

/// 当前帧缓冲 stride（字节）。
#[allow(dead_code)]
pub fn stride() -> u32 {
    unsafe { STRIDE }
}

/// 帧缓冲占用的物理页数（一屏）。
pub fn fb_pages() -> usize {
    let bytes = backing_len();
    bytes.div_ceil(allocator::PAGE_SIZE)
}

/// 分配一整屏连续、页对齐的帧缓冲，返回其物理地址（恒等映射下即虚拟地址）。
pub fn alloc_fb() -> usize {
    let pages = fb_pages();
    // 启动期降级：帧缓冲内存不足不再 panic 挂死——返回 0 表示无显存，
    // fb_init/compositor 据此跳过 GUI，系统以控制台模式继续运行。
    match allocator::alloc_pages_contig(pages) {
        Some(pa) => pa,
        None => {
            crate::health::report(
                crate::health::SEV_CRITICAL,
                "display",
                "帧缓冲内存不足，图形界面已禁用（控制台模式运行）",
                "请重启设备；若反复出现，请检查内存占用或增加系统内存",
            );
            0
        }
    }
}

/// 提交扫描缓冲；绑定未改变时仅发布像素写入，不重建宿主显示表面。
pub fn present(pa: usize) {
    let bytes = unsafe { (WIDTH * HEIGHT * 4) as usize };
    clean_dcache(pa, bytes);
    unsafe {
        bind_scanout(pa, Some(crate::virtio_gpu_protocol::Damage {
            x: 0, y: 0, w: WIDTH, h: HEIGHT,
        }));
    }
}

/// 发布脏矩形对应的像素写入；绑定未改变时复用宿主显示表面。
/// `rects` 为空时回退为整屏缓存清理（与 `present` 等价）。
pub fn present_rects(pa: usize, rects: &[crate::compositor::Rect]) {
    if rects.is_empty() {
        present(pa);
        return;
    }
    let sw = unsafe { WIDTH } as i32;
    let sh = unsafe { HEIGHT } as i32;
    let stride = unsafe { STRIDE } as usize;
    let mut damage: Option<crate::virtio_gpu_protocol::Damage> = None;
    for r in rects {
        if let Some(d) = crate::virtio_gpu_protocol::Damage::clip(r.x, r.y, r.w, r.h, sw as u32, sh as u32) {
            damage = Some(damage.map_or(d, |old| old.union(d)));
        }
        let x0 = r.x.max(0);
        let y0 = r.y.max(0);
        let x1 = r.right().min(sw);
        let y1 = r.bottom().min(sh);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        // 该矩形覆盖的字节区间 [行首, 行尾)：clean 只触及这些缓存行
        let start = y0 as usize * stride + x0 as usize * 4;
        let end = (y1 - 1) as usize * stride + x1 as usize * 4;
        clean_dcache(pa + start, end - start);
    }
    unsafe {
        bind_scanout(pa, damage);
    }
}

// ==================== /dev/fb0 支撑接口 ====================

/// 帧缓冲物理基址（0 表示尚未分配）。
static mut FB_BASE: usize = 0;

/// 分配一整屏帧缓冲并立即翻转为扫描输出，返回其物理基址。
///
/// 供内核启动时初始化 `/dev/fb0`：显存就绪后，用户态 launcher 可直接
/// open/mmap `/dev/fb0` 并写入像素，无需经过 displayd 合成器。
pub fn fb_init() -> usize {
    let pa = alloc_fb();
    if pa == 0 {
        return 0; // 无显存：不 present、不登记，fb0 缺席（launcher 会干净退出）
    }
    present(pa);
    unsafe { FB_BASE = pa; }
    pa
}

/// 接管既有的帧缓冲（如 bootloader 渲染的启动画面）作为 `/dev/fb0` 显存。
///
/// 只登记基址、不重新分配、不翻转扫描输出，保证 bootloader 阶段绘制的
/// 画面在切换期间保持显示；像素几何由 `init` 确定的默认值提供。
pub fn fb_adopt(pa: usize) -> usize {
    unsafe { FB_BASE = pa; }
    pa
}

/// 当前帧缓冲物理基址。
pub fn fb_base() -> usize {
    unsafe { FB_BASE }
}

/// 帧缓冲字节长度（一屏）。
pub fn fb_len() -> usize {
    unsafe { (WIDTH * HEIGHT * 4) as usize }
}

/// 像素格式 fourcc（XRGB8888）。
pub fn fb_format() -> u32 {
    XRGB8888
}
