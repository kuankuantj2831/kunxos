//! virtio-sound 音频控制器驱动（virtio-mmio 传输，VIRTIO 1.2 设备类型 25）。
//!
//! QEMU `-M virt` 把 virtio 设备以 `virtio,mmio` 节点暴露在设备树中；本驱动
//! 扫描所有节点并探测 `DeviceID == 25`（声卡）。virtio-sound 是 modern-only
//! 设备（QEMU 上报 Version=2），本驱动同时保留 legacy 路径（Version=1）以
//! 兼容其它实现。
//!
//! 队列布局（virtio-sound 规范，Q0 起）：
//!   - Q0 control：PCM 参数 / 启停等控制消息（请求 + 响应描述符链，同步等待）
//!   - Q1 event  ：jack 事件（本驱动不使用）
//!   - Q2 tx     ：播放 PCM 数据（核心路径，IRQ 驱动）
//!   - Q3 rx     ：录音（本驱动不使用）
//!
//! 播放路径（48kHz / 16bit / 立体声，10ms 周期 = 480 帧 = 1920 字节）：
//!   1. 探测设备，配置 control / tx 两个队列（desc+avail+used 单页布局）
//!   2. PCM_INFO 查询流 0（输出方向、格式/采样率能力位图）
//!   3. PCM_SET_PARAMS：period=1920B、buffer=1×1920B（10ms 在途）、S16、2ch、48k
//!   4. PCM_PREPARE →（GIC 就绪后）PCM_START
//!   5. tx 队列持续投递 10ms 块，每块为三描述符链：
//!        [virtio_snd_pcm_xfer(12B) → PCM(1920B) → virtio_snd_pcm_status(8B)]；
//!      used ring 更新触发 IRQ，中断里回收整条链并把「软件环形缓冲 → 硬件」
//!      的补充动作交给 audio 核心
//!   6. PCM_STOP → PCM_RELEASE（暂停 / 关闭）
//!
//! 环结构采用与 virtio-net 相同的单页布局（desc@0，avail@1024，used@1156，
//! 64 项）。设备 DMA 读描述符与 PCM 数据缓冲，恒等映射下虚拟地址即物理地址。

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use crate::allocator;
use crate::plic;
use crate::interrupt;

// ==================== 设备常量 ====================

/// virtio 设备类型：sound = 25。
const DEV_ID_SOUND: u32 = 25;

// ---- virtio-mmio 寄存器（与 net_virtio 相同布局） ----
const REG_MAGIC: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
#[allow(dead_code)]
const REG_DEV_FEATURES_SEL: usize = 0x014;
const REG_DRV_FEATURES: usize = 0x020;
const REG_DRV_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_ALIGN: usize = 0x03c;
const REG_QUEUE_PFN: usize = 0x040;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_IRQ_STATUS: usize = 0x060;
const REG_IRQ_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;
const REG_CONFIG: usize = 0x100;

// legacy 专用
const REG_GUEST_PAGE_SIZE: usize = 0x028;

// modern 专用
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_AVAIL_LOW: usize = 0x090;
const REG_QUEUE_AVAIL_HIGH: usize = 0x094;
const REG_QUEUE_USED_LOW: usize = 0x0a0;
const REG_QUEUE_USED_HIGH: usize = 0x0a4;

// ---- 设备状态 ----
const ST_ACK: u32 = 1;
const ST_DRIVER: u32 = 2;
const ST_DRIVER_OK: u32 = 4;
const ST_FEATURES_OK: u32 = 8;

/// 中断状态 bit0：used ring 更新。
const IRQ_USED: u32 = 1;

/// 现代接口必须协商的 VIRTIO_F_VERSION_1（bit 32，即 sel=1 的 bit0）。
const FEAT_VERSION_1: u32 = 1;

// ---- 队列参数（与 net_virtio 同布局） ----
const QUEUE_SIZE: usize = 64;
const DESC_OFF: usize = 0;
const AVAIL_OFF: usize = QUEUE_SIZE * 16; // 1024
const USED_OFF: usize = (AVAIL_OFF + 4 + 2 * QUEUE_SIZE + 3) & !3; // 1156
const DESC_F_WRITE: u16 = 2;

// ---- virtio-sound 控制消息码（规范 virtio-snd：PCM 段 0x0100+） ----
// 此前误用顺序 3..9——QEMU 拒收（"header not recognized: 3"）并以空负载
// 应答，PCM_INFO 全零 → 探测判「不可用」→ RISC-V 上 virtio-sound 自
// 架构切换起从未初始化成功（优化轮 020 实测抓出）。
const SND_R_JACK_INFO: u32 = 1;
#[allow(dead_code)]
const SND_R_JACK_REMAP: u32 = 2;
const SND_R_PCM_INFO: u32 = 0x0100;
const SND_R_PCM_SET_PARAMS: u32 = 0x0101;
const SND_R_PCM_PREPARE: u32 = 0x0102;
const SND_R_PCM_RELEASE: u32 = 0x0103;
const SND_R_PCM_START: u32 = 0x0104;
const SND_R_PCM_STOP: u32 = 0x0105;
#[allow(dead_code)]
const SND_R_CHMAP_INFO: u32 = 0x0200;

// ---- 响应状态码 ----
const SND_S_OK: u32 = 0;
#[allow(dead_code)]
const SND_S_BAD_MSG: u32 = 1;
const SND_S_NOT_SUPP: u32 = 2;
const SND_S_IO_ERR: u32 = 3;

// ---- PCM 能力位图（formats / rates） ----
/// S16 格式位（formats 位图 bit2）。
const FMT_S16: u64 = 1 << 2;
/// 32bit 格式位（formats 位图 bit4，96k/24bit 扩展预留）。
#[allow(dead_code)]
const FMT_S32: u64 = 1 << 4;
/// 48kHz 采样率位（rates 位图 bit6）。
const RATE_48000: u64 = 1 << 6;
/// 96kHz 采样率位（rates 位图 bit9）。
#[allow(dead_code)]
const RATE_96000: u64 = 1 << 9;

/// set_params 中 format 字段的枚举下标（S16 = 2）。
const FMT_IDX_S16: u8 = 2;
/// set_params 中 rate 字段的枚举下标（48000 = 6）。
const RATE_IDX_48000: u8 = 6;

// ---- 流参数（与 audio 核心一致） ----
/// 每周期字节数（10ms @ 48kHz，见 crate::audio）。
use crate::audio::CHUNK_BYTES;

/// 低延迟配置（延迟预算 20ms，见 docs/audio/architecture.md）：
/// 设备内缓冲 = 1 个周期（10ms 在途）。virtio-sound 规范要求
/// buffer_bytes 为 period_bytes 的整数倍且 ≥ period_bytes，1×period 合法；
/// QEMU 实现不额外限制。若选择稳健配置（BUFFER_PERIODS=2、RING 2 块），
/// 平均延迟增加约 10ms，但 underrun 容忍度更高。
const BUFFER_PERIODS: usize = 1;
/// tx 在途缓冲上限（与 BUFFER_PERIODS 一致）。
const TX_SLOTS: usize = BUFFER_PERIODS;

/// tx 消息链描述符数：virtio_snd_pcm_xfer(hdr) + PCM 数据 + virtio_snd_pcm_status。
const TX_DESC_PER_CHUNK: usize = 3;
/// virtio_snd_pcm_xfer 头长度（hdr.code 4 + stream_id 4 + padding 4 = 12）。
const TX_HDR_BYTES: usize = 12;
/// virtio_snd_pcm_status 响应长度（status 4 + latency_bytes 4 = 8）。
const TX_STATUS_BYTES: usize = 8;
/// 单块 DMA 缓冲总长（12 + 1920 + 8 = 1940，一页 4096 容纳）。
const TX_TOTAL_BYTES: usize = TX_HDR_BYTES + CHUNK_BYTES + TX_STATUS_BYTES;

/// PCM xfer 消息码（virtio-sound 规范：tx/rx 队列 I/O 消息）。
const SND_R_PCM_XFER: u32 = 10;

/// 流 0（第一个 PCM 流）即输出流。
const STREAM_ID: u32 = 0;

// ==================== 全局状态 ====================

static mut BASE: usize = 0;
static mut IRQ: u32 = 0;
static mut MODERN: bool = false;

/// control / tx 环所在物理页（恒等映射下即虚拟地址）。
static mut CTRL_RING: usize = 0;
static mut TX_RING: usize = 0;

/// tx 描述符对应的 DMA 缓冲（每块一页，用前 1920 字节）。
static mut TX_BUFS: [usize; QUEUE_SIZE] = [0; QUEUE_SIZE];
/// tx 描述符空闲链表头（0xFFFF 表示空）。
static mut TX_FREE_HEAD: u16 = 0xFFFF;
/// used ring 读取游标。
static mut TX_USED_SEEN: u16 = 0;
/// 已提交但未回收的块数（决定能否继续提交）。
static mut TX_INFLIGHT: usize = 0;

/// control 队列描述符空闲链表。
static mut CTRL_FREE_HEAD: u16 = 0xFFFF;
/// control used ring 游标。
static mut CTRL_USED_SEEN: u16 = 0;

/// 控制消息请求 / 响应静态缓冲（仅初始化阶段单线程使用）。
static mut CTRL_REQ: [u8; 64] = [0; 64];
static mut CTRL_RESP: [u8; 64] = [0; 64];

/// 播放是否已 START。
static mut RUNNING: bool = false;
/// 设备是否就绪（探测 + 队列配置 + 参数设置成功）。
static mut READY: bool = false;

// ==================== MMIO 原语 ====================

#[inline]
fn r32(off: usize) -> u32 {
    unsafe { read_volatile((BASE + off) as *const u32) }
}
#[inline]
fn w32(off: usize, v: u32) {
    unsafe { write_volatile((BASE + off) as *mut u32, v) }
}
#[inline]
fn r8(off: usize) -> u8 {
    unsafe { read_volatile((BASE + off) as *const u8) }
}

#[inline]
fn mb() {
    unsafe { asm!("fence rw, rw") }
}
#[inline]
fn dsb() {
    unsafe { asm!("fence iorw, iorw") }
}

/// 按缓存行清洗数据缓存到内存，使设备 DMA 可读到。
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

#[inline]
unsafe fn desc_addr(page: usize, i: usize) -> *mut u64 {
    (page + DESC_OFF + i * 16) as *mut u64
}
#[inline]
unsafe fn desc_len(page: usize, i: usize) -> *mut u32 {
    (page + DESC_OFF + i * 16 + 8) as *mut u32
}
#[inline]
unsafe fn desc_flags(page: usize, i: usize) -> *mut u16 {
    (page + DESC_OFF + i * 16 + 12) as *mut u16
}
#[inline]
unsafe fn desc_next(page: usize, i: usize) -> *mut u16 {
    (page + DESC_OFF + i * 16 + 14) as *mut u16
}
#[inline]
unsafe fn avail_idx(page: usize) -> u16 {
    read_volatile((page + AVAIL_OFF + 2) as *const u16)
}
#[inline]
unsafe fn set_avail_idx(page: usize, v: u16) {
    write_volatile((page + AVAIL_OFF + 2) as *mut u16, v)
}
#[inline]
unsafe fn avail_ring(page: usize, i: usize) -> *mut u16 {
    (page + AVAIL_OFF + 4 + i * 2) as *mut u16
}
#[inline]
unsafe fn used_idx(page: usize) -> u16 {
    read_volatile((page + USED_OFF + 2) as *const u16)
}
#[inline]
unsafe fn used_id(page: usize, i: usize) -> u32 {
    read_volatile((page + USED_OFF + 4 + i * 8) as *const u32)
}

// ==================== 描述符空闲链表 ====================

/// 从指定队列空闲链表取一个描述符，返回下标（0xFFFF 表示无空闲）。
unsafe fn alloc_desc(head: &mut u16, page: usize) -> u16 {
    let idx = *head;
    if idx != 0xFFFF {
        *head = *desc_next(page, idx as usize);
    }
    idx
}

/// 把描述符归还空闲链表。
unsafe fn free_desc(head: &mut u16, page: usize, idx: u16) {
    *desc_next(page, idx as usize) = *head;
    *head = idx;
}

// ==================== 队列配置 ====================

/// 配置队列 `qidx`，环内存位于 `page`（单页：desc/avail/used 同页布局）。
///
/// 要求设备支持至少 `QUEUE_SIZE` 的队列深度；不足返回 `false`，避免以错误
/// 的环大小运行导致设备写穿。
unsafe fn setup_queue(qidx: u32, page: usize) -> bool {
    w32(REG_QUEUE_SEL, qidx);
    let num_max = r32(REG_QUEUE_NUM_MAX);
    if (num_max as usize) < QUEUE_SIZE {
        return false;
    }
    w32(REG_QUEUE_NUM, QUEUE_SIZE as u32);
    if MODERN {
        w32(REG_QUEUE_DESC_LOW, (page + DESC_OFF) as u32);
        w32(REG_QUEUE_DESC_HIGH, ((page + DESC_OFF) >> 32) as u32);
        w32(REG_QUEUE_AVAIL_LOW, (page + AVAIL_OFF) as u32);
        w32(REG_QUEUE_AVAIL_HIGH, ((page + AVAIL_OFF) >> 32) as u32);
        w32(REG_QUEUE_USED_LOW, (page + USED_OFF) as u32);
        w32(REG_QUEUE_USED_HIGH, ((page + USED_OFF) >> 32) as u32);
        w32(REG_QUEUE_READY, 1);
    } else {
        w32(REG_QUEUE_ALIGN, 4);
        w32(REG_QUEUE_PFN, (page >> 12) as u32);
    }
    true
}

/// 初始化队列描述符空闲链表（0 → 1 → ... → N-1）。
unsafe fn ring_init(page: usize) {
    for i in 0..QUEUE_SIZE {
        *desc_next(page, i) = if i + 1 < QUEUE_SIZE { (i + 1) as u16 } else { 0xFFFF };
        *desc_flags(page, i) = 0;
        *desc_len(page, i) = 0;
        *desc_addr(page, i) = 0;
    }
    // 清零 avail/used 头字段
    *((page + AVAIL_OFF) as *mut u16) = 0; // flags
    *((page + AVAIL_OFF + 2) as *mut u16) = 0; // idx
    *((page + USED_OFF) as *mut u16) = 0; // flags
    *((page + USED_OFF + 2) as *mut u16) = 0; // idx
}

// ==================== 控制消息（同步） ====================

/// 提交一条控制请求（请求 + 响应双描述符链），轮询等待完成。
///
/// `req`：请求缓冲（含 hdr.code 等完整消息）；`resp_len`：响应缓冲长度。
/// 返回响应缓冲首 4 字节（状态码），超时返回 `None`。
fn ctl_send(req: &[u8], resp_len: usize) -> Option<u32> {
    unsafe {
        if req.len() > CTRL_REQ.len() || resp_len > CTRL_RESP.len() {
            return None;
        }
        CTRL_REQ[..req.len()].copy_from_slice(req);
        let req_page = CTRL_RING;
        let rq = alloc_desc(&mut CTRL_FREE_HEAD, req_page);
        if rq == 0xFFFF {
            return None;
        }
        let rs = alloc_desc(&mut CTRL_FREE_HEAD, req_page);
        if rs == 0xFFFF {
            free_desc(&mut CTRL_FREE_HEAD, req_page, rq);
            return None;
        }

        // 描述符链：rq（设备读，请求）→ rs（设备写，响应）
        *desc_addr(req_page, rq as usize) = CTRL_REQ.as_ptr() as usize as u64;
        *desc_len(req_page, rq as usize) = req.len() as u32;
        *desc_flags(req_page, rq as usize) = 0;
        *desc_next(req_page, rq as usize) = rs;
        *desc_addr(req_page, rs as usize) = CTRL_RESP.as_ptr() as usize as u64;
        *desc_len(req_page, rs as usize) = resp_len as u32;
        *desc_flags(req_page, rs as usize) = DESC_F_WRITE;
        *desc_next(req_page, rs as usize) = 0;
        dcache_clean(req_page + DESC_OFF + rq as usize * 16, 32);
        dcache_clean(CTRL_REQ.as_ptr() as usize, req.len());
        dcache_clean(CTRL_RESP.as_ptr() as usize, resp_len);

        let ai = avail_idx(req_page);
        *avail_ring(req_page, (ai % QUEUE_SIZE as u16) as usize) = rq;
        mb();
        set_avail_idx(req_page, ai.wrapping_add(1));
        dcache_clean(req_page + AVAIL_OFF, 6 + 2 * QUEUE_SIZE);
        dsb();
        w32(REG_QUEUE_NOTIFY, 0); // kick control 队列

        // 轮询 used ring 直到我们的描述符完成（初始化阶段单线程，无 IRQ）
        let deadline = crate::timer::now_us() + 2_000_000; // 2s 超时
        loop {
            dcache_invalidate(req_page + USED_OFF, 4 + QUEUE_SIZE * 8);
            let ui = used_idx(req_page);
            if CTRL_USED_SEEN != ui {
                let id = used_id(req_page, (CTRL_USED_SEEN % QUEUE_SIZE as u16) as usize);
                CTRL_USED_SEEN = CTRL_USED_SEEN.wrapping_add(1);
                // 完成项可能是 rq（链首）；回收 rq 与 rs 两个描述符
                if id as u16 == rq {
                    free_desc(&mut CTRL_FREE_HEAD, req_page, rq);
                    free_desc(&mut CTRL_FREE_HEAD, req_page, rs);
                    // 设备写入的响应：作废缓存后读取
                    dcache_invalidate(CTRL_RESP.as_ptr() as usize, resp_len);
                    let st = u32::from_le_bytes([
                        CTRL_RESP[0], CTRL_RESP[1], CTRL_RESP[2], CTRL_RESP[3],
                    ]);
                    return Some(st);
                }
                // 其它完成项（理论上控制队列只有一个在途请求）：继续轮询
            }
            if crate::timer::now_us() >= deadline {
                // 超时：回收描述符并报错
                free_desc(&mut CTRL_FREE_HEAD, req_page, rq);
                free_desc(&mut CTRL_FREE_HEAD, req_page, rs);
                return None;
            }
        }
    }
}

/// 便捷封装：发送单个 PCM 控制消息（PREPARE/START/STOP/RELEASE）。
///
/// virtio-sound 规范：这些请求体是 `virtio_snd_pcm_hdr` = {hdr.code(4), stream_id(4)}，
/// 共 8 字节；只发 code 会被设备判为 BAD_MSG。
fn ctl_simple(code: u32) -> Option<u32> {
    let mut req = [0u8; 8];
    req[..4].copy_from_slice(&code.to_le_bytes());
    req[4..8].copy_from_slice(&STREAM_ID.to_le_bytes());
    ctl_send(&req, 4)
}

// ==================== 控制消息构造 ====================

/// PCM_INFO：查询流 `id` 的能力，把输出流信息写入 `info`。
fn pcm_info(id: u32, info: &mut [u8; 32]) -> Option<u32> {
    unsafe {
        let req = CTRL_REQ.as_mut_ptr();
        // virtio_snd_query_info：hdr{code} + start_id + count + size
        (req as *mut u32).write_volatile(SND_R_PCM_INFO.to_le());
        (req.add(4) as *mut u32).write_volatile(id.to_le());
        (req.add(8) as *mut u32).write_volatile(1u32.to_le());
        (req.add(12) as *mut u32).write_volatile(32u32.to_le());
    }
    // 响应 = 状态头(4B) + 32B pcm_info 负载；设备校验 in_sg ≥ 4 + 32 = 36。
    let st = ctl_send(&unsafe { CTRL_REQ }[..16], 36)?;
    // [debug] PCM_INFO 原始响应前 40 字节（优化轮 020：修码后告警归零
    // 但负载仍全零，dump 定位响应头布局/描述符管道）。
    crate::kinfo! {
        crate::uart_send_str("[vsnd] PCM_INFO resp=");
        for i in 0..40 {
            if i > 0 { crate::uart_send_str(","); }
            crate::print_hex(unsafe { CTRL_RESP[i] } as u64);
        }
        crate::uart_send_str("\n");
    }
    unsafe {
        info.copy_from_slice(&CTRL_RESP[4..36]);
    }
    Some(st)
}

/// PCM_SET_PARAMS：设置流参数（period/buffer/格式/采样率/声道）。
fn pcm_set_params(
    stream_id: u32,
    buffer_bytes: u32,
    period_bytes: u32,
    channels: u8,
    format: u8,
    rate: u8,
) -> Option<u32> {
    unsafe {
        let req = CTRL_REQ.as_mut_ptr();
        // virtio_snd_pcm_set_params：pcm_hdr{code, stream_id}(8) + buffer(4) +
        // period(4) + features(4) + channels(1) + padding(5) + format(1) + rate(1)
        // + padding2(4) = 共 32 字节。code 必须在最前（当前 QEMU 校验消息长度恒为 32）。
        (req as *mut u32).write_volatile(SND_R_PCM_SET_PARAMS.to_le());
        (req.add(4) as *mut u32).write_volatile(stream_id.to_le());
        (req.add(8) as *mut u32).write_volatile(buffer_bytes.to_le());
        (req.add(12) as *mut u32).write_volatile(period_bytes.to_le());
        (req.add(16) as *mut u32).write_volatile(0u32.to_le()); // features
        *req.add(20) = channels;
        // padding[5] @21..25（保持零）
        for i in 21..26 {
            *req.add(i) = 0;
        }
        *req.add(26) = format;
        *req.add(27) = rate;
        for i in 28..32 {
            *req.add(i) = 0;
        }
    }
    ctl_send(&unsafe { CTRL_REQ }[..32], 4)
}

// ==================== 设备初始化 ====================

/// 探测并初始化 virtio-sound；成功返回 true（流已 PREPARE，未 START）。
pub fn init(devs: &[crate::net_virtio::MmioDev]) -> bool {
    unsafe {
        // 1. 探测声卡（DeviceID == 25）
        let mut found = false;
        for d in devs {
            if read_volatile((d.base + REG_MAGIC) as *const u32) != 0x7472_6976 {
                continue;
            }
            if read_volatile((d.base + REG_DEVICE_ID) as *const u32) != DEV_ID_SOUND {
                continue;
            }
            BASE = d.base;
            IRQ = d.irq;
            MODERN = read_volatile((d.base + REG_VERSION) as *const u32) == 2;
            found = true;
            break;
        }
        if !found {
            return false;
        }
        crate::kinfo! {
            crate::uart_send_str("[vsnd] 找到 virtio-sound");
            if MODERN {
                crate::uart_send_str(" (modern)\n");
            } else {
                crate::uart_send_str(" (legacy)\n");
            }
        }

        // 2. 复位 + ACK + DRIVER
        w32(REG_STATUS, 0);
        w32(REG_STATUS, ST_ACK);
        w32(REG_STATUS, ST_ACK | ST_DRIVER);

        // 3. 特性协商：modern 必须接受 VIRTIO_F_VERSION_1（bit32）；
        //    legacy 显式写 0（不启用任何特性）。
        if MODERN {
            w32(REG_DRV_FEATURES_SEL, 0);
            w32(REG_DRV_FEATURES, 0);
            w32(REG_DRV_FEATURES_SEL, 1);
            w32(REG_DRV_FEATURES, FEAT_VERSION_1);
            w32(REG_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK);
            // 校验设备接受特性
            let st = r32(REG_STATUS);
            if st & ST_FEATURES_OK == 0 {
                crate::uart_send_str("[vsnd] FEATURES_OK 被拒绝\n");
                return false;
            }
        } else {
            w32(REG_GUEST_PAGE_SIZE, allocator::PAGE_SIZE as u32);
            w32(REG_DRV_FEATURES_SEL, 0);
            w32(REG_DRV_FEATURES, 0);
        }

        // 4. 分配并清零环页
        // 启动期降级：环页不足返回 false（音频设备缺席，应用静音运行）
        let (Some(ctrl), Some(tx)) = (allocator::alloc_page(), allocator::alloc_page()) else {
            crate::health::report(
                crate::health::SEV_WARN,
                "audio",
                "virtio-snd 环内存不足，音频已禁用",
                "系统静音运行；请重启设备恢复",
            );
            return false;
        };
        CTRL_RING = ctrl;
        TX_RING = tx;
        for i in 0..allocator::PAGE_SIZE / 8 {
            *((ctrl + i * 8) as *mut u64) = 0;
            *((tx + i * 8) as *mut u64) = 0;
        }
        ring_init(ctrl);
        ring_init(tx);
        CTRL_FREE_HEAD = 0;
        TX_FREE_HEAD = 0;
        CTRL_USED_SEEN = 0;
        TX_USED_SEEN = 0;
        TX_INFLIGHT = 0;
        for i in 0..QUEUE_SIZE {
            TX_BUFS[i] = 0;
        }

        // 5. 建立队列（control=0，tx=2；event/rx 本驱动不使用）
        if !setup_queue(0, ctrl) || !setup_queue(2, tx) {
            crate::uart_send_str("[vsnd] 队列深度 < 64 不支持\n");
            return false;
        }

        // 6. DRIVER_OK
        if MODERN {
            w32(REG_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
        } else {
            w32(REG_STATUS, ST_ACK | ST_DRIVER | ST_DRIVER_OK);
        }

        // 7. 查询 PCM 能力（流 0）
        let mut info = [0u8; 32];
        let st = match pcm_info(STREAM_ID, &mut info) {
            Some(s) => s,
            None => {
                crate::uart_send_str("[vsnd] PCM_INFO 超时\n");
                return false;
            }
        };
        if st != SND_S_OK {
            crate::uart_send_str("[vsnd] PCM_INFO 失败, 状态=");
            crate::print_dec(st as u64);
            crate::uart_send_str("\n");
            return false;
        }
        let formats = u64::from_le_bytes(info[8..16].try_into().unwrap());
        let rates = u64::from_le_bytes(info[16..24].try_into().unwrap());
        let direction = info[24];
        let ch_min = info[25];
        let ch_max = info[26];
        crate::kinfo! {
            crate::uart_send_str("[vsnd] stream0 dir=");
            crate::print_dec(direction as u64);
            crate::uart_send_str(" ch=");
            crate::print_dec(ch_min as u64);
            crate::uart_send_str("..");
            crate::print_dec(ch_max as u64);
            crate::uart_send_str(" fmt=0x");
            crate::print_hex(formats);
            crate::uart_send_str(" rates=0x");
            crate::print_hex(rates);
            crate::uart_send_str("\n");
        }
        if direction != 0 || ch_min > 2 || ch_max < 2 {
            crate::kinfo! {
                crate::uart_send_str("[vsnd] stream0 不可用于立体声输出 (raw=");
                for (i, b) in info.iter().enumerate() {
                    if i > 0 {
                        crate::uart_send_str(",");
                    }
                    crate::print_hex(*b as u64);
                }
                crate::uart_send_str(")\n");
            }
            return false;
        }

        // 8. SET_PARAMS：period=10ms、buffer=1×period（低延迟在途 10ms）、S16、2ch、48k
        let buffer_bytes = (CHUNK_BYTES * BUFFER_PERIODS) as u32;
        let st = match pcm_set_params(
            STREAM_ID,
            buffer_bytes,
            CHUNK_BYTES as u32,
            2,
            FMT_IDX_S16,
            RATE_IDX_48000,
        ) {
            Some(s) => s,
            None => {
                crate::uart_send_str("[vsnd] SET_PARAMS 超时\n");
                return false;
            }
        };
        if st != SND_S_OK {
            crate::uart_send_str("[vsnd] SET_PARAMS 失败, 状态=");
            crate::print_dec(st as u64);
            crate::uart_send_str("\n");
            return false;
        }

        // 9. PREPARE（进入就绪；START 在 GIC/IRQ 就绪后调用）
        let st = match ctl_simple(SND_R_PCM_PREPARE) {
            Some(s) => s,
            None => {
                crate::uart_send_str("[vsnd] PREPARE 超时\n");
                return false;
            }
        };
        if st != SND_S_OK {
            crate::uart_send_str("[vsnd] PREPARE 失败, 状态=");
            crate::print_dec(st as u64);
            crate::uart_send_str("\n");
            return false;
        }

        READY = true;
        crate::kinfo! {
            crate::uart_send_str("[vsnd] 参数: 48kHz S16 立体声, period=10ms, buffer=10ms (低延迟配置)\n");
        }
        true
    }
}

// ==================== 启动 / 停止 ====================

/// 注册中断并启动播放（在 GIC 初始化之后调用）。
pub fn start() {
    if !unsafe { READY } || unsafe { RUNNING } {
        return;
    }
    // 注册中断并启用 GIC
    interrupt::register_handler(unsafe { IRQ }, irq_handler);
    plic::plic_enable_irq(unsafe { IRQ });
    plic::plic_set_priority(unsafe { IRQ }, 0x80);
    plic::plic_set_target(unsafe { IRQ }, 0x01);

    unsafe {
        let st = ctl_simple(SND_R_PCM_START);
        if st != Some(SND_S_OK) {
            crate::uart_send_str("[vsnd] PCM_START 失败\n");
            return;
        }
        RUNNING = true;
    }
    crate::kinfo! { crate::uart_send_str("[vsnd] 播放已启动\n"); }
}

/// 停止播放。
pub fn stop() {
    if !unsafe { RUNNING } {
        return;
    }
    unsafe {
        RUNNING = false;
        let _ = ctl_simple(SND_R_PCM_STOP);
    }
}

/// 释放流（关闭设备）。
#[allow(dead_code)]
pub fn release() {
    unsafe {
        if READY {
            let _ = ctl_simple(SND_R_PCM_RELEASE);
            READY = false;
        }
    }
}

// ==================== tx 提交 / 回收 ====================

/// 提交一块 PCM（`CHUNK_BYTES`）到 tx 队列；成功返回 0，无空闲描述符返回 -1。
///
/// 每块构造规范的三描述符链（一次拷贝，随后设备 DMA 读取）：
///   desc[h]: [12B virtio_snd_pcm_xfer{hdr.code=XFER, stream_id}]
///   desc[d]: [1920B PCM 数据]（紧随 xfer 头）
///   desc[s]: [8B virtio_snd_pcm_status]，设备写回 status+latency
///
/// 页面布局：buf[0..12)=xfer 头，buf[12..1932)=PCM，buf[1932..1940)=status。
pub fn submit(data: &[u8]) -> i32 {
    if !(unsafe { READY } && unsafe { RUNNING }) {
        return -1;
    }
    if data.len() != CHUNK_BYTES {
        return -1;
    }
    unsafe {
        // 保存-恢复（而非出口无条件 csrsi 重开）：本函数也被 IRQ 路径共享
        // （irq_handler → reclaim → hw_consumed → refill → submit）。中断处理
        // 中途重开 SIE 会允许嵌套中断，reclaim 与嵌套 IRQ 重入 TX 环撕裂
        // TX_USED_SEEN/TX_FREE_HEAD，音频流可能永久停摆。
        let saved: u64;
        core::arch::asm!("csrr {}, sstatus", out(reg) saved);
        core::arch::asm!("csrci sstatus, 2");
        if TX_INFLIGHT >= TX_SLOTS {
            core::arch::asm!("csrw sstatus, {}", in(reg) saved);
            return -1;
        }
        let h = alloc_desc(&mut TX_FREE_HEAD, TX_RING);
        if h == 0xFFFF {
            core::arch::asm!("csrw sstatus, {}", in(reg) saved);
            return -1;
        }
        let d = alloc_desc(&mut TX_FREE_HEAD, TX_RING);
        if d == 0xFFFF {
            free_desc(&mut TX_FREE_HEAD, TX_RING, h);
            core::arch::asm!("csrw sstatus, {}", in(reg) saved);
            return -1;
        }
        let s = alloc_desc(&mut TX_FREE_HEAD, TX_RING);
        if s == 0xFFFF {
            free_desc(&mut TX_FREE_HEAD, TX_RING, d);
            free_desc(&mut TX_FREE_HEAD, TX_RING, h);
            core::arch::asm!("csrw sstatus, {}", in(reg) saved);
            return -1;
        }
        let buf = allocator::alloc_page();
        let buf = match buf {
            Some(b) => b,
            None => {
                free_desc(&mut TX_FREE_HEAD, TX_RING, s);
                free_desc(&mut TX_FREE_HEAD, TX_RING, d);
free_desc(&mut TX_FREE_HEAD, TX_RING, h as u16);
                core::arch::asm!("csrw sstatus, {}", in(reg) saved);
                return -1;
            }
        };
        TX_BUFS[h as usize] = buf;

        // ---- xfer 头（12B）：code=XFER、stream_id、padding 清零 ----
        core::ptr::write_bytes(buf as *mut u8, 0, TX_HDR_BYTES);
        *(buf as *mut u32) = SND_R_PCM_XFER.to_le();
        *((buf + 4) as *mut u32) = STREAM_ID.to_le();
        // ---- PCM 数据 ----
        core::ptr::copy_nonoverlapping(data.as_ptr(), (buf + TX_HDR_BYTES) as *mut u8, CHUNK_BYTES);
        // ---- status 区（8B）由设备写回，先清零避免陈旧内容 ----
        core::ptr::write_bytes((buf + TX_HDR_BYTES + CHUNK_BYTES) as *mut u8, 0, TX_STATUS_BYTES);
        dcache_clean(buf, TX_TOTAL_BYTES);

        // ---- 三描述符链 ----
        *desc_addr(TX_RING, h as usize) = buf as u64;
        *desc_len(TX_RING, h as usize) = TX_HDR_BYTES as u32;
        *desc_flags(TX_RING, h as usize) = 0;
        *desc_next(TX_RING, h as usize) = d;
        *desc_addr(TX_RING, d as usize) = (buf + TX_HDR_BYTES) as u64;
        *desc_len(TX_RING, d as usize) = CHUNK_BYTES as u32;
        *desc_flags(TX_RING, d as usize) = 0;
        *desc_next(TX_RING, d as usize) = s;
        *desc_addr(TX_RING, s as usize) = (buf + TX_HDR_BYTES + CHUNK_BYTES) as u64;
        *desc_len(TX_RING, s as usize) = TX_STATUS_BYTES as u32;
        *desc_flags(TX_RING, s as usize) = DESC_F_WRITE;
        *desc_next(TX_RING, s as usize) = 0;
        dcache_clean(TX_RING + DESC_OFF + h as usize * 16, 48);

        let ai = avail_idx(TX_RING);
        *avail_ring(TX_RING, (ai % QUEUE_SIZE as u16) as usize) = h;
        mb();
        set_avail_idx(TX_RING, ai.wrapping_add(1));
        dcache_clean(TX_RING + AVAIL_OFF, 6 + 2 * QUEUE_SIZE);
        TX_INFLIGHT += 1;
        dsb();
        w32(REG_QUEUE_NOTIFY, 2); // kick tx 队列
        core::arch::asm!("csrw sstatus, {}", in(reg) saved);
        0
    }
}

/// 回收 used ring 中已完成的 tx 块（中断 / 轮询共用），返回回收块数。
///
/// 每回收一块即释放 DMA 缓冲并归还描述符；`hw_frames` 推进由 audio 核心
/// 依据本函数返回值累计（QEMU 顺序消费，每块 = `CHUNK_FRAMES` 帧）。
fn reclaim() -> usize {
    unsafe {
        let mut n = 0usize;
        loop {
            dcache_invalidate(TX_RING + USED_OFF, 4 + QUEUE_SIZE * 8);
            let ui = used_idx(TX_RING);
            if TX_USED_SEEN == ui {
                break;
            }
            let h = used_id(TX_RING, (TX_USED_SEEN % QUEUE_SIZE as u16) as usize);
            if (h as usize) >= QUEUE_SIZE {
                // 设备回报越界 id：与 net_virtio 同型防护——中断上下文里
                // TX_BUFS[h] 野索引会直接 panic abort，禁用驱动并退出。
                crate::uart_send_str("[snd] virtio-sound：TX 已用 id 越界，禁用音频\n");
                READY = false;
                RUNNING = false;
                return n;
            }
            let buf = TX_BUFS[h as usize];
            if buf != 0 {
                allocator::free_page(buf);
                TX_BUFS[h as usize] = 0;
            }
            // 释放整条三描述符链：h → d → s（链尾 next 为 0）。
            let d = *desc_next(TX_RING, h as usize);
            let s = *desc_next(TX_RING, d as usize);
            free_desc(&mut TX_FREE_HEAD, TX_RING, s);
            free_desc(&mut TX_FREE_HEAD, TX_RING, d);
            free_desc(&mut TX_FREE_HEAD, TX_RING, h as u16);
            TX_USED_SEEN = TX_USED_SEEN.wrapping_add(1);
            if TX_INFLIGHT > 0 {
                TX_INFLIGHT -= 1;
            }
            n += 1;
        }
        n
    }
}

/// 中断处理入口（此时 IRQ 已被硬件屏蔽）。
fn irq_handler() {
    let st = r32(REG_IRQ_STATUS);
    if st & IRQ_USED != 0 {
        w32(REG_IRQ_ACK, IRQ_USED);
        let n = reclaim();
        if n > 0 {
            // 通知 audio 核心：硬件已消费 n 块，可继续补充
            crate::audio::hw_consumed(n as u64);
        }
    }
}

// ==================== 查询接口 ====================

/// 设备是否就绪。
pub fn ready() -> bool {
    unsafe { READY }
}

/// 当前是否可提交（在途缓冲未满）。
pub fn can_submit() -> bool {
    unsafe { TX_INFLIGHT < TX_SLOTS }
}

/// 设备名。
pub fn name() -> &'static str {
    "virtio-sound"
}
