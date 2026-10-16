//! virtio-net 网卡驱动（virtio-mmio 传输，兼容 legacy 与 modern 接口）。
//!
//! QEMU `-M virt` 把 virtio 设备以 `virtio,mmio` 节点暴露在设备树中，本驱动
//! 扫描所有节点并探测 `DeviceID == 1`（网络卡）。QEMU 默认上报 legacy 接口
//! （Version=1），使用 QueuePFN 描述环；modern（Version=2）使用 64 位分离地址。
//!
//! 环结构采用 legacy 分体布局（单页内 desc + avail + used），每队列一页。
//! RX 队列预填 2048 字节缓冲；收到的包被搬入软件接收队列（`RX_POOL`），
//! 由上层轮询取走。TX 复用描述符空闲链表，发送后由 used ring 回收缓冲。
//!
//! 中断：virtio 产生 IRQ 时表示 used ring 更新，中断处理仅做环回收（搬包到
//! 软件队列），协议处理在 `net_stack::poll` 中进行（避免在中断上下文做复杂逻辑）。

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use crate::allocator;
use crate::plic;
use crate::heap;
use crate::interrupt;

// ==================== 设备发现 ====================

/// 一个待探测的 virtio-mmio 设备（基地址 + GIC 中断号）。
#[derive(Clone, Copy)]
pub struct MmioDev {
    pub base: usize,
    pub irq: u32,
}

impl MmioDev {
    pub const fn empty() -> Self {
        MmioDev { base: 0, irq: 0 }
    }
}

// ==================== 寄存器偏移 ====================

const VIRTIO_MAGIC: u32 = 0x7472_6976; // "virt"
const DEV_ID_NET: u32 = 1;

// legacy / modern 公共寄存器
const REG_MAGIC: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
#[allow(dead_code)]
const REG_VENDOR_ID: usize = 0x00c;
#[allow(dead_code)]
const REG_DEV_FEATURES: usize = 0x010;
#[allow(dead_code)]
const REG_DEV_FEATURES_SEL: usize = 0x014;
const REG_DRV_FEATURES: usize = 0x020;
const REG_DRV_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_IRQ_STATUS: usize = 0x060;
const REG_IRQ_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;
const REG_CONFIG: usize = 0x100;

// legacy 专用
#[allow(dead_code)]
const REG_GUEST_PAGE_SIZE: usize = 0x028;
#[allow(dead_code)]
const REG_QUEUE_ALIGN: usize = 0x03c;
#[allow(dead_code)]
const REG_QUEUE_PFN: usize = 0x040;

// modern 专用
#[allow(dead_code)]
const REG_QUEUE_READY: usize = 0x044;
#[allow(dead_code)]
const REG_QUEUE_DESC_LOW: usize = 0x080;
#[allow(dead_code)]
const REG_QUEUE_DESC_HIGH: usize = 0x084;
#[allow(dead_code)]
const REG_QUEUE_AVAIL_LOW: usize = 0x090;
#[allow(dead_code)]
const REG_QUEUE_AVAIL_HIGH: usize = 0x094;
#[allow(dead_code)]
const REG_QUEUE_USED_LOW: usize = 0x0a0;
#[allow(dead_code)]
const REG_QUEUE_USED_HIGH: usize = 0x0a4;

// ==================== 状态位 ====================

const ST_ACK: u32 = 1;
const ST_DRIVER: u32 = 2;
const ST_DRIVER_OK: u32 = 4;
const ST_FEATURES_OK: u32 = 8;

/// 中断状态 bit0：used ring 更新。
const IRQ_USED: u32 = 1;

// ==================== 队列参数 ====================

const QUEUE_SIZE: usize = 64;
const RX_BUF_LEN: usize = 2048;
/// legacy virtio-net 报文头部长度（struct virtio_net_hdr，10 字节）。
const VNET_HDR_LEN: usize = 10;

/// 环内偏移：desc 表（16B/项）→ avail（2B flag + 2B idx + 2B*N）→ used（4 对齐）。
const DESC_OFF: usize = 0;
const AVAIL_OFF: usize = QUEUE_SIZE * 16; // 1024
const USED_OFF: usize = (AVAIL_OFF + 4 + 2 * QUEUE_SIZE + 3) & !3; // 1156

/// 描述符 flags：WRITE 表示设备写入该缓冲。
const DESC_F_WRITE: u16 = 2;

/// 软件接收队列容量（与环深 QUEUE_SIZE 一致，避免突发时丢包）。
const RX_SLOTS: usize = 64;

// ==================== 全局状态 ====================

/// 网络卡 MMIO 基地址（0 表示未初始化）。
static mut BASE: usize = 0;
/// 网络卡 GIC 中断号。
static mut IRQ: u32 = 0;
/// 是否为 modern 接口（Version=2）。
static mut MODERN: bool = false;
/// 从配置空间读到的 MAC 地址。
static mut MAC: [u8; 6] = [0; 6];

/// RX / TX 环所在物理页（恒等映射下即虚拟地址）。
static mut RX_RING: usize = 0;
static mut TX_RING: usize = 0;

/// RX 描述符对应的 DMA 缓冲地址（设备写入）。
static mut RX_BUFS: [usize; QUEUE_SIZE] = [0; QUEUE_SIZE];
/// TX 描述符对应的缓冲地址（我们写入，回收时释放）。
static mut TX_BUFS: [usize; QUEUE_SIZE] = [0; QUEUE_SIZE];

/// TX 描述符空闲链表头（0xFFFF 表示空）。
static mut TX_FREE_HEAD: u16 = 0xFFFF;

/// used ring 读取游标（避免重复处理）。
static mut RX_USED_SEEN: u16 = 0;
static mut TX_USED_SEEN: u16 = 0;

/// 软件接收队列（环形）。
static mut RX_POOL: [[u8; RX_BUF_LEN]; RX_SLOTS] = [[0; RX_BUF_LEN]; RX_SLOTS];
static mut RX_POOL_LEN: [usize; RX_SLOTS] = [0; RX_SLOTS];
static mut RX_POOL_HEAD: usize = 0;
static mut RX_POOL_COUNT: usize = 0;

// ==================== MMIO 原语 ====================

/// 读 32 位 MMIO 寄存器。
fn r32(off: usize) -> u32 {
    unsafe { read_volatile((BASE + off) as *const u32) }
}

/// 写 32 位 MMIO 寄存器。
fn w32(off: usize, v: u32) {
    unsafe { write_volatile((BASE + off) as *mut u32, v) }
}

/// 读配置空间 1 字节。
fn r8(off: usize) -> u8 {
    unsafe { read_volatile((BASE + off) as *const u8) }
}

// ==================== 内存屏障 / 缓存维护 ====================

/// 全内存屏障（普通内存写对设备可见前使用）。
#[inline]
fn mb() {
    unsafe { asm!("fence rw, rw") }
}

/// 数据同步屏障（写 MMIO 通知寄存器前使用）。
#[inline]
fn dsb() {
    unsafe { asm!("fence iorw, iorw") }
}

/// 按缓存行（64B）清洗数据缓存到内存，使设备 DMA 可读到。
///
/// 在 TCG 下为无害操作；在真实硬件（KVM）上必须调用。
#[inline]
unsafe fn dcache_clean(va: usize, len: usize) {
    let mut a = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while a < end {
        asm!("fence iorw, iorw");
        a += 64;
    }
    asm!("fence iorw, iorw");
}

/// 按缓存行作废数据缓存，使后续读取直接取内存（设备写入的数据可见）。
#[inline]
unsafe fn dcache_invalidate(va: usize, len: usize) {
    let mut a = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while a < end {
        asm!("fence iorw, iorw");
        a += 64;
    }
    asm!("fence iorw, iorw");
}

// ==================== 环访问辅助 ====================

/// 描述符地址字段（u64）。
#[inline]
unsafe fn desc_addr(page: usize, i: usize) -> *mut u64 {
    (page + DESC_OFF + i * 16) as *mut u64
}

/// 描述符长度字段（u32）。
#[inline]
unsafe fn desc_len(page: usize, i: usize) -> *mut u32 {
    (page + DESC_OFF + i * 16 + 8) as *mut u32
}

/// 描述符 flags 字段（u16）。
#[inline]
unsafe fn desc_flags(page: usize, i: usize) -> *mut u16 {
    (page + DESC_OFF + i * 16 + 12) as *mut u16
}

/// 描述符 next 字段（u16）。
#[inline]
unsafe fn desc_next(page: usize, i: usize) -> *mut u16 {
    (page + DESC_OFF + i * 16 + 14) as *mut u16
}

/// avail ring 的 idx（u16）。
#[inline]
unsafe fn avail_idx(page: usize) -> u16 {
    read_volatile((page + AVAIL_OFF + 2) as *const u16)
}

/// 写 avail ring 的 idx。
#[inline]
unsafe fn set_avail_idx(page: usize, v: u16) {
    write_volatile((page + AVAIL_OFF + 2) as *mut u16, v)
}

/// avail ring 第 i 项（描述符下标）。
#[inline]
unsafe fn avail_ring(page: usize, i: usize) -> *mut u16 {
    (page + AVAIL_OFF + 4 + i * 2) as *mut u16
}

/// used ring 的 idx（u16）。
#[inline]
unsafe fn used_idx(page: usize) -> u16 {
    read_volatile((page + USED_OFF + 2) as *const u16)
}

/// used ring 第 i 项的描述符 id。
#[inline]
unsafe fn used_id(page: usize, i: usize) -> u32 {
    read_volatile((page + USED_OFF + 4 + i * 8) as *const u32)
}

/// used ring 第 i 项的长度。
#[inline]
unsafe fn used_len(page: usize, i: usize) -> u32 {
    read_volatile((page + USED_OFF + 4 + i * 8 + 4) as *const u32)
}

// ==================== 描述符空闲链表 ====================

/// 从 TX 空闲链表取一个描述符，返回下标（0xFFFF 表示无空闲）。
unsafe fn tx_alloc_desc() -> u16 {
    let idx = TX_FREE_HEAD;
    if idx != 0xFFFF {
        TX_FREE_HEAD = *desc_next(TX_RING, idx as usize);
    }
    idx
}

/// 把描述符归还 TX 空闲链表。
unsafe fn tx_free_desc(idx: u16) {
    *desc_next(TX_RING, idx as usize) = TX_FREE_HEAD;
    TX_FREE_HEAD = idx;
}

// ==================== 环回收（中断 / 轮询共用） ====================

/// 回收 used ring：RX 包搬入软件队列并重新投递缓冲；TX 释放缓冲、归还描述符。
unsafe fn drain_impl() {
    // 设备被禁用（used id 越界等致命错误）后不再访问 MMIO/环，防重复故障
    if BASE == 0 {
        return;
    }
    // ---- TX 回收 ----
    loop {
        // 使设备写入的 used ring 对 CPU 可见（KVM/真机有真实缓存时必需；
        // 此前只 invalidate RX 数据缓冲，used ring 陈旧导致 TX 永不回收、
        // RX 永不收包）
        dcache_invalidate(TX_RING + USED_OFF, 4 + QUEUE_SIZE * 8);
        let ui = used_idx(TX_RING);
        if TX_USED_SEEN == ui {
            break;
        }
        let id = used_id(TX_RING, (TX_USED_SEEN % QUEUE_SIZE as u16) as usize);
        // 设备回报的描述符下标必须在本队范围：越界 id 若被用来索引 TX_BUFS /
        // 归还空闲链表会破坏描述符环与堆状态，直接禁用网卡（宁停勿错）。
        if (id as usize) >= QUEUE_SIZE {
            crate::uart_send_str("[net] virtio-net：TX 已用 id 越界，禁用网卡\n");
            BASE = 0;
            return;
        }
        let buf = TX_BUFS[id as usize];
        if buf != 0 {
            heap::kfree(buf as *mut u8);
            TX_BUFS[id as usize] = 0;
        }
        tx_free_desc(id as u16);
        TX_USED_SEEN = TX_USED_SEEN.wrapping_add(1);
    }

    // ---- RX 回收 ----
    loop {
        dcache_invalidate(RX_RING + USED_OFF, 4 + QUEUE_SIZE * 8);
        let ui = used_idx(RX_RING);
        if RX_USED_SEEN == ui {
            break;
        }
        let id = used_id(RX_RING, (RX_USED_SEEN % QUEUE_SIZE as u16) as usize);
        // 同上：RX id 越界则无法安全索引 RX_BUFS / 描述符环并重投递，
        // 直接禁用网卡，避免向环外地址写描述符。
        if (id as usize) >= QUEUE_SIZE {
            crate::uart_send_str("[net] virtio-net：RX 已用 id 越界，禁用网卡\n");
            BASE = 0;
            return;
        }
        let len = used_len(RX_RING, (RX_USED_SEEN % QUEUE_SIZE as u16) as usize) as usize;
        let buf = RX_BUFS[id as usize];

        // 先作废缓存，确保读到设备本次 DMA 写入的新数据（而非上一帧的陈旧缓存行）。
        // 关键顺序：必须在 CPU 读取之前 invalidate（KVM/真机有真实缓存时必现）。
        dcache_invalidate(buf, RX_BUF_LEN);

        // 搬入软件接收队列（若满则丢弃）；剥离 10 字节 virtio_net_hdr
        if RX_POOL_COUNT < RX_SLOTS && len >= VNET_HDR_LEN {
            let slot = (RX_POOL_HEAD + RX_POOL_COUNT) % RX_SLOTS;
            let src = (buf + VNET_HDR_LEN) as *const u8;
            let dst = RX_POOL[slot].as_mut_ptr();
            let n = (len - VNET_HDR_LEN).min(RX_BUF_LEN - VNET_HDR_LEN);
            core::ptr::copy_nonoverlapping(src, dst, n);
            RX_POOL_LEN[slot] = n;
            RX_POOL_COUNT += 1;
        }

        // 重新投递该 RX 缓冲（设备继续写入）
        *desc_addr(RX_RING, id as usize) = buf as u64;
        *desc_len(RX_RING, id as usize) = RX_BUF_LEN as u32;
        *desc_flags(RX_RING, id as usize) = DESC_F_WRITE;
        dcache_clean(RX_RING + DESC_OFF + id as usize * 16, 16);

        let idx = avail_idx(RX_RING);
        *avail_ring(RX_RING, (idx % QUEUE_SIZE as u16) as usize) = id as u16;
        mb();
        set_avail_idx(RX_RING, idx.wrapping_add(1));
        dcache_clean(RX_RING + AVAIL_OFF, 6 + 2 * QUEUE_SIZE);

        RX_USED_SEEN = RX_USED_SEEN.wrapping_add(1);
    }

    // 有新增 RX 缓冲时 kick RX 队列，通知设备继续接收
    dsb();
    w32(REG_QUEUE_NOTIFY, 0);
}

/// 进程上下文调用的回收入口（屏蔽 IRQ 保护共享状态）。
pub fn drain() {
    unsafe { asm!("csrci sstatus, 2") };
    unsafe { drain_impl() };
    unsafe { asm!("csrsi sstatus, 2") };
}

/// 中断处理入口（此时 IRQ 已被硬件屏蔽）。
fn irq_handler() {
    // ★ 设备被禁用（drain_impl 对故障置 BASE=0）后 GIC 该 IRQ 仍使能、迟到的
    // 中断仍可能到达：此前不检查 BASE 就读 REG_IRQ_STATUS(0x60)/写 ACK(0x64)，
    // 地址 0x60 未映射 → IRQ 上下文 data abort 直接挂死内核。禁用态直接吞掉。
    if unsafe { BASE } == 0 {
        return;
    }
    // ★ 无条件 ACK 全部 cause：virtio-mmio InterruptStatus bit0=used ring、
    // bit1=config change。此前只 ACK bit0 —— 设备发纯 config-change
    // （st=0b10）时永不写 ACK 寄存器，电平中断被 GIC 立即重投递 → 风暴
    // 护栏触发把整块网卡禁掉。drain 仍只在真有 used 更新时执行。
    let st = r32(REG_IRQ_STATUS);
    if st != 0 {
        w32(REG_IRQ_ACK, st);
    }
    if st & IRQ_USED != 0 {
        unsafe { drain_impl() };
    }
}

// ==================== 初始化 ====================

/// 初始化网络卡：从 `devs` 中探测 DeviceID==1 的设备并完成配置。
///
/// 成功返回 `true` 并注册中断处理、使能 GIC 中断；找不到网络卡返回 `false`。
pub fn init(devs: &[MmioDev]) -> bool {
    // 1. 探测网络设备
    for d in devs {
        unsafe {
            if read_volatile((d.base + REG_MAGIC) as *const u32) != VIRTIO_MAGIC {
                continue;
            }
            if read_volatile((d.base + REG_DEVICE_ID) as *const u32) != DEV_ID_NET {
                continue;
            }
            BASE = d.base;
            IRQ = d.irq;
            MODERN = read_volatile((d.base + REG_VERSION) as *const u32) == 2;
        }
        break;
    }
    if unsafe { BASE == 0 } {
        return false;
    }

    unsafe {
        // 2. 复位 + ACK + DRIVER
        w32(REG_STATUS, 0);
        w32(REG_STATUS, ST_ACK);
        w32(REG_STATUS, ST_ACK | ST_DRIVER);

        // 3. 特性协商（不启用任何特性，驱动自行计算校验和）
        if MODERN {
            w32(REG_DRV_FEATURES_SEL, 0);
            w32(REG_DRV_FEATURES, 0);
            w32(REG_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK);
        } else {
            w32(REG_GUEST_PAGE_SIZE, allocator::PAGE_SIZE as u32);
            // legacy 亦需显式写入 guest features=0（否则不启用任何特性）
            w32(REG_DRV_FEATURES_SEL, 0);
            w32(REG_DRV_FEATURES, 0);
        }

        // 4. 分配并清零环页
        // 启动期降级：环页不足返回 false（网络缺席，浏览器等联网应用报错）
        let (Some(rx), Some(tx)) = (allocator::alloc_page(), allocator::alloc_page()) else {
            crate::health::report(
                crate::health::SEV_WARN,
                "network",
                "virtio-net 环内存不足，网络已禁用",
                "联网功能不可用；请重启设备恢复",
            );
            return false;
        };
        RX_RING = rx;
        TX_RING = tx;
        for i in 0..allocator::PAGE_SIZE / 8 {
            *((RX_RING + i * 8) as *mut u64) = 0;
            *((TX_RING + i * 8) as *mut u64) = 0;
        }

        // 5. 建立两个队列
        if !setup_queue(0, RX_RING) || !setup_queue(1, TX_RING) {
            crate::uart_send_str("[net] virtio-net：队列深度 < 64 不受支持\n");
            BASE = 0; // 失败复位：残留 BASE 会让 ready() 误报就绪
            return false;
        }

        // 6. 初始化 RX / TX 环内容
        if !rx_ring_fill() {
            BASE = 0; // 失败复位：残留 BASE 会让 ready() 误报就绪
            return false;
        }
        tx_ring_init();

        // 7. DRIVER_OK
        if MODERN {
            w32(REG_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
        } else {
            w32(REG_STATUS, ST_ACK | ST_DRIVER | ST_DRIVER_OK);
        }

        // 8. 读取 MAC（配置空间偏移 0 起 6 字节）
        for i in 0..6 {
            MAC[i] = r8(REG_CONFIG + i);
        }
    }

    // 9. 注册中断并启用 GIC
    interrupt::register_handler(unsafe { IRQ }, irq_handler);
    plic::plic_enable_irq(unsafe { IRQ });
    plic::plic_set_priority(unsafe { IRQ }, 0x80);
    plic::plic_set_target(unsafe { IRQ }, 0x01);

    true
}

/// 配置队列 `qidx`，环内存位于 `page`。
///
/// 环内 desc/avail/used 的偏移按 `QUEUE_SIZE`（64）布局，因此要求设备
/// 支持至少 64 的队列深度；`QUEUE_NUM_MAX` 不足时返回 `false`（初始化失败），
/// 避免以错误的环大小运行导致设备写穿。
unsafe fn setup_queue(qidx: u32, page: usize) -> bool {
    w32(REG_QUEUE_SEL, qidx);
    let num_max = r32(REG_QUEUE_NUM_MAX);
    if (num_max as usize) < QUEUE_SIZE {
        return false;
    }
    let num = QUEUE_SIZE as u32;
    w32(REG_QUEUE_NUM, num);
    if MODERN {
        w32(REG_QUEUE_DESC_LOW, (page + DESC_OFF) as u32);
        w32(REG_QUEUE_DESC_HIGH, ((page + DESC_OFF) >> 32) as u32);
        w32(REG_QUEUE_AVAIL_LOW, (page + AVAIL_OFF) as u32);
        w32(REG_QUEUE_AVAIL_HIGH, ((page + AVAIL_OFF) >> 32) as u32);
        w32(REG_QUEUE_USED_LOW, (page + USED_OFF) as u32);
        w32(REG_QUEUE_USED_HIGH, ((page + USED_OFF) >> 32) as u32);
        w32(REG_QUEUE_READY, 1);
    } else {
        // QueueAlign 决定 used ring 的对齐：设为 4 使 desc/avail/used 同页
        //（QEMU 按该值对齐 used ring，见 virtio_queue_update_rings）。
        w32(REG_QUEUE_ALIGN, 4);
        w32(REG_QUEUE_PFN, (page >> 12) as u32);
    }
    true
}

/// 预填 RX 队列：每个描述符指向一个 2048 字节缓冲，供设备写入。
///
/// `kmalloc` 失败（理论上仅在堆耗尽 panic 前可发生）时返回 `false`，
/// 由调用方中止初始化，避免向地址 0 投放描述符。
unsafe fn rx_ring_fill() -> bool {
    for i in 0..QUEUE_SIZE {
        let buf = heap::kmalloc(RX_BUF_LEN);
        if buf.is_null() {
            crate::uart_send_str("[net] virtio-net：RX 缓冲分配失败\n");
            return false;
        }
        let buf = buf as usize;
        RX_BUFS[i] = buf;
        *desc_addr(RX_RING, i) = buf as u64;
        *desc_len(RX_RING, i) = RX_BUF_LEN as u32;
        *desc_flags(RX_RING, i) = DESC_F_WRITE;
        *desc_next(RX_RING, i) = 0;
        *avail_ring(RX_RING, i) = i as u16;
    }
    mb();
    set_avail_idx(RX_RING, QUEUE_SIZE as u16);
    // 设备 DMA 读 desc/avail：写回缓存（真机冷启动时缓存可能持有陈旧内容，
    // 不 clean 的话设备可能读到 desc addr=0 而 DMA 到空地址）
    dcache_clean(RX_RING + DESC_OFF, QUEUE_SIZE * 16);
    dcache_clean(RX_RING + AVAIL_OFF, 6 + 2 * QUEUE_SIZE);
    RX_USED_SEEN = 0;
    dsb();
    w32(REG_QUEUE_NOTIFY, 0); // kick RX 队列
    true
}

/// 初始化 TX 描述符空闲链表（0 → 1 → ... → N-1）。
unsafe fn tx_ring_init() {
    TX_FREE_HEAD = 0;
    for i in 0..QUEUE_SIZE {
        *desc_next(TX_RING, i) = if i + 1 < QUEUE_SIZE { (i + 1) as u16 } else { 0xFFFF };
        *desc_flags(TX_RING, i) = 0;
        *desc_len(TX_RING, i) = 0;
        *desc_addr(TX_RING, i) = 0;
    }
    TX_USED_SEEN = 0;
}

// ==================== 收发接口 ====================

/// 网卡是否就绪。
pub fn ready() -> bool {
    unsafe { BASE != 0 }
}

/// 本机 MAC 地址。
pub fn mac() -> [u8; 6] {
    unsafe { MAC }
}

/// 发送一帧以太网数据；成功返回 0，无空闲描述符返回 -1。
pub fn send(pkt: &[u8]) -> i32 {
    if unsafe { BASE == 0 } || pkt.is_empty() {
        return -1;
    }
    unsafe {
        // 屏蔽 IRQ：与中断内 drain_impl 的 TX 回收（kfree/tx_free_desc）互斥，
        // 避免 slab 空闲链表与 TX_FREE_HEAD 被并发改写。
        asm!("csrci sstatus, 2");
        let idx = tx_alloc_desc();
        if idx == 0xFFFF {
            asm!("csrsi sstatus, 2");
            return -1;
        }
        // legacy virtio-net 在每个报文前需 10 字节 virtio_net_hdr（全零）
        let total = pkt.len() + VNET_HDR_LEN;
        let buf = heap::kmalloc(total);
        if buf.is_null() {
            // 分配失败：归还描述符后返回（不发送）
            tx_free_desc(idx);
            asm!("csrsi sstatus, 2");
            return -1;
        }
        let buf = buf as usize;
        for k in 0..VNET_HDR_LEN {
            *((buf + k) as *mut u8) = 0;
        }
        core::ptr::copy_nonoverlapping(pkt.as_ptr(), (buf + VNET_HDR_LEN) as *mut u8, pkt.len());
        TX_BUFS[idx as usize] = buf;
        dcache_clean(buf, total);

        *desc_addr(TX_RING, idx as usize) = buf as u64;
        *desc_len(TX_RING, idx as usize) = total as u32;
        *desc_flags(TX_RING, idx as usize) = 0;
        *desc_next(TX_RING, idx as usize) = 0;
        dcache_clean(TX_RING + DESC_OFF + idx as usize * 16, 16);

        let ai = avail_idx(TX_RING);
        *avail_ring(TX_RING, (ai % QUEUE_SIZE as u16) as usize) = idx;
        mb();
        set_avail_idx(TX_RING, ai.wrapping_add(1));
        dcache_clean(TX_RING + AVAIL_OFF, 6 + 2 * QUEUE_SIZE);

        dsb();
        w32(REG_QUEUE_NOTIFY, 1); // kick TX 队列
        asm!("csrsi sstatus, 2");
        0
    }
}

/// 从软件接收队列取一帧到 `buf`，返回帧长；队列空返回 `None`。
pub fn recv_into(buf: &mut [u8]) -> Option<usize> {
    unsafe {
        asm!("csrci sstatus, 2");
        let r = if RX_POOL_COUNT > 0 {
            let slot = RX_POOL_HEAD;
            let n = RX_POOL_LEN[slot].min(buf.len());
            buf[..n].copy_from_slice(&RX_POOL[slot][..n]);
            RX_POOL_HEAD = (RX_POOL_HEAD + 1) % RX_SLOTS;
            RX_POOL_COUNT -= 1;
            Some(n)
        } else {
            None
        };
        asm!("csrsi sstatus, 2");
        r
    }
}

// ==================== DMA 缓存行隔离诊断（netdiag 观测点，只读） ====================
//
// 技术债「DMA 64B 缓存行隔离（Pi4/KVM）」的确定性观测：dcache_clean /
// dcache_invalidate 按整条 64B 行生效（首末行向下/向上取整）。若维护区间
// 与「不参与本次维护的所有者」共享行，真实缓存平台（Pi4 / ARM KVM）上：
//   - 环内：avail 尾部与 used 头部共享行时（USED_OFF 非 64 对齐），
//     clean(avail) 的行回写会把 CPU 缓存中陈旧的 used 头字节写回内存，
//     覆盖设备 DMA 写入的 used ring → TX 回收/RX 收包丢失（TCG 无缓存
//     仿真，不可复现；KVM/Pi4 必现前提）；
//   - 缓冲：kmalloc 默认 16B 对齐的 RX 缓冲首末行可能与相邻分配共享，
//     invalidate(RX buf) 会丢弃共享行上相邻所有者的脏数据 → 堆内存损坏。
// 本诊断输出「隔离前提是否成立」的运行时事实（任何平台可跑），修复决策
// 以此为据，不做盲改。

/// DMA 缓存行隔离审计快照（`cache_line_diag` 产出，shell `netdiag` 格式化）。
pub struct CacheLineDiag {
    /// 网卡是否就绪（未就绪时其余字段为 0）。
    pub ready: bool,
    /// RX/TX 环基址（页分配，恒等映射）。
    pub rx_ring: usize,
    pub tx_ring: usize,
    /// used ring 在环页内偏移（当前布局常量 USED_OFF）。
    pub used_off: usize,
    /// used 头部是否与 avail 尾部共享 64B 行（USED_OFF % 64 != 0）。
    pub used_line_shared: bool,
    /// clean(avail) 区间按行取整后是否越过 avail 末尾侵入 used 头部。
    pub avail_clean_overreach: bool,
    /// 投递中的 RX DMA 缓冲总数 / 其中 64B 对齐的数量。
    pub rx_buf_total: usize,
    pub rx_buf_aligned: usize,
    /// 投递中的 TX DMA 缓冲总数 / 其中 64B 对齐的数量。
    pub tx_buf_total: usize,
    pub tx_buf_aligned: usize,
}

/// 采集缓存行隔离审计快照（关中断，避免与 IRQ 侧回收竞态读到中间态）。
pub fn cache_line_diag() -> CacheLineDiag {
    let mut d = CacheLineDiag {
        ready: ready(),
        rx_ring: 0,
        tx_ring: 0,
        used_off: USED_OFF,
        used_line_shared: false,
        avail_clean_overreach: false,
        rx_buf_total: 0,
        rx_buf_aligned: 0,
        tx_buf_total: 0,
        tx_buf_aligned: 0,
    };
    if !d.ready {
        return d;
    }
    unsafe {
        let saved: u64;
        asm!("csrr {}, sstatus", out(reg) saved);
        asm!("csrci sstatus, 2");
        d.rx_ring = RX_RING;
        d.tx_ring = TX_RING;
        // avail 占 [AVAIL_OFF, AVAIL_OFF+6+2*Q)，used 从 USED_OFF 起；
        // 二者共享行 ⟺ USED_OFF 不在 64B 边界上（avail 恰结束于 USED_OFF）。
        d.used_line_shared = USED_OFF & 63 != 0;
        // clean(AVAIL_OFF, 6+2*Q) 末尾按行上取整：(avail_end + 63) & !63
        let avail_end = AVAIL_OFF + 6 + 2 * QUEUE_SIZE;
        d.avail_clean_overreach = ((avail_end + 63) & !63) > USED_OFF;
        for i in 0..QUEUE_SIZE {
            let rb = RX_BUFS[i];
            if rb != 0 {
                d.rx_buf_total += 1;
                if rb & 63 == 0 {
                    d.rx_buf_aligned += 1;
                }
            }
            let tb = TX_BUFS[i];
            if tb != 0 {
                d.tx_buf_total += 1;
                if tb & 63 == 0 {
                    d.tx_buf_aligned += 1;
                }
            }
        }
        asm!("csrw sstatus, {}", in(reg) saved);
    }
    d
}
