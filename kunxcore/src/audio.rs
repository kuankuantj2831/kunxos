//! 音频核心：内核侧的低延迟播放路径与 `/dev/audio` 字符设备。
//!
//! 架构（用户态 audiod ↔ 内核 audio ↔ virtio-sound 硬件）：
//!
//! ```text
//!   应用 A ─┐                      ┌─ 软件环形缓冲（2 × 10ms 块）
//!   应用 B ─┼─ 客户端协议 → audiod ─┼─ /dev/audio write ──┐
//!   应用 C ─┘  （混音+焦点）        │                     ▼
//!                               ┌──┴──┐   IRQ 驱动补充   ┌─────────┐
//!                               │audio│ ───────────────► │virtio-  │
//!                               │core │ ◄── used ring ─── │sound    │
//!                               └─────┘    （IRQ）        └─────────┘
//! ```
//!
//! 低延迟设计（延迟预算 20ms，见 docs/audio/architecture.md）：
//!   - 周期 10ms（480 帧 @ 48kHz，16bit 立体声），单次 write 即一块
//!   - 软件环形缓冲 2 块（20ms 抗调度抖动），硬件在途 1 块（10ms）：
//!     平均流水线深度 ≈ 10ms(环) + 10ms(设备) + 2ms(调度) ≈ 20ms
//!   - IRQ 驱动补充（非轮询）：used ring 更新 → 中断里回收缓冲并把软件
//!     环形缓冲 → 硬件 的补充动作做完，中断路径仅 memcpy + 描述符操作
//!   - 最小拷贝：用户态 → 内核环形缓冲（系统调用拷贝）→ DMA 缓冲（一次
//!     显式拷贝），无额外中间缓冲
//!   - underrun 时以静音填充保持流连续并计数；空闲时静音块不重复投递
//!     （避免 QEMU wav/none 后端“即取即消”造成的 IRQ 风暴）
//!
//! `/dev/audio` 接口（字符设备，主设备号 14）：
//!   - `write(fd, pcm, n)`：把 PCM 追加到环形缓冲（首写自动启动播放）
//!   - ioctl：GETPARAMS / START / STOP / STATS / RT_BOOST
//!
//! 实时保障：audiod 先 ioctl RT_BOOST 把自身提升到调度器 Realtime 类，
//! 保证 10ms 混音周期的调度抖动最小化。

use core::arch::asm;

use crate::sched_cfs::PriorityClass;

// ==================== 参数常量 ====================

/// 采样率（Hz）。
pub const SAMPLE_RATE: u32 = 48000;
/// 声道数。
pub const CHANNELS: u32 = 2;
/// 采样位深。
pub const SAMPLE_BITS: u32 = 16;
/// 每周期帧数（10ms @ 48kHz）。
pub const CHUNK_FRAMES: usize = 480;
/// 每周期字节数（480 帧 × 2 声道 × 2 字节）。
pub const CHUNK_BYTES: usize = CHUNK_FRAMES * 4;

// ---- 低延迟配置（总延迟预算 20ms，见 docs/audio/architecture.md） ----
//
//   RING_CHUNKS × 10ms（软件环形缓冲，写入端到补充之间的缓冲）
//   + 设备在途 10ms（virtio-sound buffer = 1 × period）
//   + 调度/轮询 ≈ 2ms ⇒ 平均 ≈ 20ms，最坏 ≈ 32ms。
//
// 低延迟配置（默认）：RING_CHUNKS = 2 —— 保留一块抗抖动余量，平均缓冲保持
// 在 10 ~ 20ms；把两块都设成 1 可逼近硬 20ms 预算但 underrun 更敏感；稳健
// 配置可回退到 3/3（≈60ms，几乎无 underrun）。
pub const RING_CHUNKS: usize = 2;
/// 软件环形缓冲容量（字节）。
const RING_BYTES: usize = RING_CHUNKS * CHUNK_BYTES;

/// 上一次补充的是否为静音块（true 时静音循环不重复投递，避免空闲 IRQ 风暴；
/// 新数据到达会自动切换回真实块）。
static mut LAST_TX_SILENCE: bool = false;

/// /dev/audio 主设备号（Linux 传统 audio major，本内核无冲突）。
pub const AUDIO_MAJOR: u32 = 14;
/// /dev/audio 次设备号。
pub const AUDIO_MINOR: u32 = 0;

// ---- ioctl 请求码 ----
/// 查询当前参数（输出 {rate, channels, bits}，各 u32）。
pub const AUDIO_IOCTL_GETPARAMS: u32 = 0x4100;
/// 启动播放（首写会自动启动，显式调用用于预热）。
pub const AUDIO_IOCTL_START: u32 = 0x4101;
/// 停止播放（清空环形缓冲）。
pub const AUDIO_IOCTL_STOP: u32 = 0x4102;
/// 查询统计（输出 [`AudioStats`]，128 字节）。
pub const AUDIO_IOCTL_STATS: u32 = 0x4103;
/// 把调用进程提升到 Realtime 调度类（SCHED_FIFO 等价物）。
pub const AUDIO_IOCTL_RT: u32 = 0x4104;
/// 异步播放单音（入参 `[freq:u32, dur_ms:u32]`，8 字节）。不阻塞调用方：
/// 内核派生任务播放，适合通话提示音（拨号音 / 忙音 / 提示音）。
pub const AUDIO_IOCTL_TONE: u32 = 0x4105;
/// 开始循环铃声（入参 `[freq:u32, 0]`）：1 秒响 / 2 秒停，直到 RING_STOP。
pub const AUDIO_IOCTL_RING_START: u32 = 0x4106;
/// 停止循环铃声。
pub const AUDIO_IOCTL_RING_STOP: u32 = 0x4107;
/// 请求音频焦点（入参 `[owner:u32]`）。owner：1 = 通话（可抢占音乐），
/// 2 = 音乐。通话焦点存在时音乐申请返回 -16（EBUSY）。
pub const AUDIO_IOCTL_ACQUIRE: u32 = 0x4108;
/// 释放音频焦点（入参 `[owner:u32]`）。
pub const AUDIO_IOCTL_RELEASE: u32 = 0x4109;
/// 查询焦点与路由（出参 `[focus:u32, route:u32]`）。
pub const AUDIO_IOCTL_GET_STATE: u32 = 0x410A;
/// 设置音频路由（入参 `[route:u32]`）：0 = 听筒，1 = 扬声器，2 = 蓝牙。
/// 真机侧对应音频通路选择；QEMU 无实体外放通路，仅记录状态供上层查询。
pub const AUDIO_IOCTL_SET_ROUTE: u32 = 0x410B;

/// 音频焦点所有者：无。
pub const FOCUS_NONE: u32 = 0;
/// 音频焦点所有者：通话（电话/短信子系统）。
pub const FOCUS_CALL: u32 = 1;
/// 音频焦点所有者：音乐（媒体播放）。
pub const FOCUS_MUSIC: u32 = 2;

/// 音频路由：听筒。
pub const ROUTE_EARPIECE: u32 = 0;
/// 音频路由：扬声器。
pub const ROUTE_SPEAKER: u32 = 1;
/// 音频路由：蓝牙（A2DP/HFP 预留）。
pub const ROUTE_BLUETOOTH: u32 = 2;

// ==================== 统计结构 ====================

/// 音频统计（ioctl AUDIO_IOCTL_STATS 输出，布局与用户态 audiod/audiolat 一致）。
#[repr(C)]
pub struct AudioStats {
    /// 播放是否运行中。
    pub running: u32,
    /// 采样率。
    pub rate: u32,
    /// 声道数。
    pub channels: u32,
    /// 位深。
    pub bits: u32,
    /// 用户态写入的总帧数（写入位置）。
    pub sw_frames: u64,
    /// 硬件已消费的总帧数（播放位置）。
    pub hw_frames: u64,
    /// underrun 次数（环形缓冲空时以静音填充）。
    pub underruns: u64,
    /// 迟写次数（环形缓冲满时丢弃的写入）。
    pub late_writes: u64,
    /// 中断次数。
    pub irq_count: u64,
    /// IRQ 周期（us）：min / avg / max。
    pub period_min_us: u64,
    pub period_avg_us: u64,
    pub period_max_us: u64,
    /// IRQ 内补充耗时（us）：min / avg / max。
    pub refill_min_us: u64,
    pub refill_avg_us: u64,
    pub refill_max_us: u64,
    /// 提交 → 硬件消费（us，IRQ 到 DAC 路径）：min / avg / max。
    pub dma_min_us: u64,
    pub dma_avg_us: u64,
    pub dma_max_us: u64,
}

// ==================== 全局状态 ====================

/// 播放是否运行中。
static mut RUNNING: bool = false;
/// 后端（virtio-sound）是否就绪。
static mut BACKEND_READY: bool = false;

/// 软件环形缓冲（字节）。
static mut RING: [u8; RING_BYTES] = [0; RING_BYTES];
/// 环形缓冲读指针（下一个待播放字节）。
static mut RING_HEAD: usize = 0;
/// 环形缓冲可用字节数。
static mut RING_COUNT: usize = 0;
/// 补充用的暂存块（环形缓冲 → 硬件的一次拷贝中转）。
static mut SCRATCH: [u8; CHUNK_BYTES] = [0; CHUNK_BYTES];
/// 静音块（underrun 填充）。
static mut SILENCE: [u8; CHUNK_BYTES] = [0; CHUNK_BYTES];

static mut SW_FRAMES: u64 = 0;
static mut HW_FRAMES: u64 = 0;
static mut UNDERRUNS: u64 = 0;
static mut LATE_WRITES: u64 = 0;
static mut IRQ_COUNT: u64 = 0;
static mut LAST_IRQ_US: u64 = 0;

static mut PERIOD_MIN: u64 = u64::MAX;
static mut PERIOD_SUM: u64 = 0;
static mut PERIOD_MAX: u64 = 0;
static mut PERIOD_N: u64 = 0;

static mut REFILL_MIN: u64 = u64::MAX;
static mut REFILL_SUM: u64 = 0;
static mut REFILL_MAX: u64 = 0;
static mut REFILL_N: u64 = 0;

/// 提交时间戳队列（提交 → 消费延迟测量，环形，容量 8 = 80ms）。
const DMA_SLOTS: usize = 8;
static mut DMA_SUBMIT_US: [u64; DMA_SLOTS] = [0; DMA_SLOTS];
static mut DMA_HEAD: usize = 0;
static mut DMA_COUNT: usize = 0;
static mut DMA_MIN: u64 = u64::MAX;
static mut DMA_SUM: u64 = 0;
static mut DMA_MAX: u64 = 0;
static mut DMA_N: u64 = 0;

// ==================== 统计辅助 ====================

fn stats_acc(
    min: &mut u64,
    sum: &mut u64,
    max: &mut u64,
    n: &mut u64,
    sample: u64,
) {
    if sample < *min {
        *min = sample;
    }
    if sample > *max {
        *max = sample;
    }
    *sum += sample;
    *n += 1;
}

fn stats_avg(sum: u64, n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        sum / n
    }
}

// ==================== 环形缓冲 ====================

/// 屏蔽 IRQ（保护环形缓冲与补充路径的共享状态）。
fn irq_mask() {
    unsafe { asm!("msr daifset, #2") };
}
fn irq_unmask() {
    unsafe { asm!("msr daifclr, #2") };
}

/// 补充：把软件环形缓冲的数据推给硬件；不足一块时以静音填充保持连续。
///
/// 调用方必须已屏蔽 IRQ（或处于中断上下文）。
fn refill() {
    if !unsafe { RUNNING } || !unsafe { BACKEND_READY } {
        return;
    }
    loop {
        if !crate::virtio_snd::can_submit() {
            break;
        }
        unsafe {
            if RING_COUNT >= CHUNK_BYTES {
                // 真实数据块：入队一次拷贝后交给硬件
                LAST_TX_SILENCE = false;
                let head = RING_HEAD;
                let first = (RING_BYTES - head).min(CHUNK_BYTES);
                SCRATCH[..first].copy_from_slice(&RING[head..head + first]);
                if first < CHUNK_BYTES {
                    SCRATCH[first..].copy_from_slice(&RING[..CHUNK_BYTES - first]);
                }
                if crate::virtio_snd::submit(&SCRATCH) != 0 {
                    break;
                }
                RING_HEAD = (RING_HEAD + CHUNK_BYTES) % RING_BYTES;
                RING_COUNT -= CHUNK_BYTES;
                DMA_SUBMIT_US[(DMA_HEAD + DMA_COUNT) % DMA_SLOTS] = crate::timer::now_us();
                DMA_COUNT = (DMA_COUNT + 1).min(DMA_SLOTS);
            } else {
                // underrun：以静音填充；仅当确有客户端写入过才计数
                if SW_FRAMES > 0 {
                    UNDERRUNS += 1;
                }
                if LAST_TX_SILENCE {
                    break; // 空闲静音循环：不重复投递，避免 IRQ 风暴
                }
                if crate::virtio_snd::submit(&SILENCE) != 0 {
                    break;
                }
                LAST_TX_SILENCE = true;
                DMA_SUBMIT_US[(DMA_HEAD + DMA_COUNT) % DMA_SLOTS] = crate::timer::now_us();
                DMA_COUNT = (DMA_COUNT + 1).min(DMA_SLOTS);
            }
        }
    }
}

// ==================== 启动 / 停止 ====================

/// 启动播放：注册中断、PCM_START、预填静音块。
///
/// 幂等：已运行则直接返回。可在任意上下文调用（首次 write 自动触发）。
pub fn start() {
    irq_mask();
    start_locked();
    irq_unmask();
}

fn start_locked() {
    if !unsafe { BACKEND_READY } || unsafe { RUNNING } {
        return;
    }
    crate::virtio_snd::start();
    if !crate::virtio_snd::ready() {
        return;
    }
    unsafe { RUNNING = true };
    refill();
}

/// 停止播放：PCM_STOP 并清空环形缓冲。
pub fn stop() {
    irq_mask();
    if unsafe { RUNNING } {
        crate::virtio_snd::stop();
        unsafe {
            RUNNING = false;
            RING_HEAD = 0;
            RING_COUNT = 0;
            DMA_HEAD = 0;
            DMA_COUNT = 0;
            LAST_TX_SILENCE = false;
        }
    }
    irq_unmask();
}

/// 后端 IRQ 回调：硬件已消费 `n` 块（由 virtio_snd 中断处理调用）。
///
/// 统计 IRQ 周期与补充耗时，并推进硬件播放位置。
pub fn hw_consumed(n: u64) {
    let now = crate::timer::now_us();
    unsafe {
        IRQ_COUNT += 1;
        if LAST_IRQ_US != 0 {
            stats_acc(
                &mut PERIOD_MIN,
                &mut PERIOD_SUM,
                &mut PERIOD_MAX,
                &mut PERIOD_N,
                now - LAST_IRQ_US,
            );
        }
        LAST_IRQ_US = now;
        HW_FRAMES += n * CHUNK_FRAMES as u64;
        // 提交 → 消费延迟：弹出 n 个提交时间戳
        for _ in 0..n {
            if DMA_COUNT > 0 {
                let t = DMA_SUBMIT_US[DMA_HEAD];
                DMA_HEAD = (DMA_HEAD + 1) % DMA_SLOTS;
                DMA_COUNT -= 1;
                stats_acc(
                    &mut DMA_MIN,
                    &mut DMA_SUM,
                    &mut DMA_MAX,
                    &mut DMA_N,
                    now.saturating_sub(t),
                );
            }
        }
    }
    let t0 = crate::timer::now_us();
    refill();
    let dt = crate::timer::now_us() - t0;
    unsafe {
        stats_acc(
            &mut REFILL_MIN,
            &mut REFILL_SUM,
            &mut REFILL_MAX,
            &mut REFILL_N,
            dt,
        );
    }
}

// ==================== /dev/audio 字符设备 ====================

/// `write(fd, pcm, n)`：把 PCM 追加到环形缓冲；首写自动启动。
pub fn audio_write(_minor: u32, buf: &[u8]) -> i32 {
    irq_mask();
    let n = unsafe {
        if !BACKEND_READY {
            irq_unmask();
            return -19; // ENODEV
        }
        if !RUNNING {
            start_locked();
            if !RUNNING {
                irq_unmask();
                return -19;
            }
        }
        // 块对齐写入：refill 只按整块（CHUNK_BYTES）消费，环形缓冲必须保持
        // 块边界对齐。容纳得下整块才写入（含跨环回绕），否则整块丢弃并计数，
        // 避免「半块残留 + 下次补齐」导致两块 PCM 在边界被拼坏。
        if buf.len() == 0 || buf.len() % CHUNK_BYTES != 0 {
            irq_unmask();
            return -22; // EINVAL：仅接受整块写入
        }
        if buf.len() > RING_BYTES {
            irq_unmask();
            return -22;
        }
        if RING_COUNT + buf.len() > RING_BYTES {
            LATE_WRITES += 1;
            irq_unmask();
            return 0; // 环形缓冲满：整块丢弃（audiod 下一周期重试）
        }
        let n = buf.len();
        let pos = (RING_HEAD + RING_COUNT) % RING_BYTES;
        let first = (RING_BYTES - pos).min(n);
        RING[pos..pos + first].copy_from_slice(&buf[..first]);
        if first < n {
            RING[..n - first].copy_from_slice(&buf[first..]);
        }
        RING_COUNT += n;
        SW_FRAMES += (n / 4) as u64;
        n
    };
    refill();
    irq_unmask();
    n as i32
}

/// ioctl 分发（GETPARAMS / START / STOP / STATS / RT）。
pub fn audio_ioctl(_minor: u32, request: u32, out: &mut [u8]) -> i32 {
    match request {
        AUDIO_IOCTL_GETPARAMS => {
            if out.len() < 12 {
                return -1;
            }
            out[0..4].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
            out[4..8].copy_from_slice(&CHANNELS.to_le_bytes());
            out[8..12].copy_from_slice(&SAMPLE_BITS.to_le_bytes());
            12
        }
        AUDIO_IOCTL_START => {
            start();
            0
        }
        AUDIO_IOCTL_STOP => {
            stop();
            0
        }
        AUDIO_IOCTL_STATS => {
            if out.len() < core::mem::size_of::<AudioStats>() {
                return -1;
            }
            let s = AudioStats {
                running: unsafe { RUNNING as u32 },
                rate: SAMPLE_RATE,
                channels: CHANNELS,
                bits: SAMPLE_BITS,
                sw_frames: unsafe { SW_FRAMES },
                hw_frames: unsafe { HW_FRAMES },
                underruns: unsafe { UNDERRUNS },
                late_writes: unsafe { LATE_WRITES },
                irq_count: unsafe { IRQ_COUNT },
                period_min_us: if unsafe { PERIOD_MIN } == u64::MAX { 0 } else { unsafe { PERIOD_MIN } },
                period_avg_us: stats_avg(unsafe { PERIOD_SUM }, unsafe { PERIOD_N }),
                period_max_us: unsafe { PERIOD_MAX },
                refill_min_us: if unsafe { REFILL_MIN } == u64::MAX { 0 } else { unsafe { REFILL_MIN } },
                refill_avg_us: stats_avg(unsafe { REFILL_SUM }, unsafe { REFILL_N }),
                refill_max_us: unsafe { REFILL_MAX },
                dma_min_us: if unsafe { DMA_MIN } == u64::MAX { 0 } else { unsafe { DMA_MIN } },
                dma_avg_us: stats_avg(unsafe { DMA_SUM }, unsafe { DMA_N }),
                dma_max_us: unsafe { DMA_MAX },
            };
            let bytes = core::mem::size_of::<AudioStats>();
            let ptr = &s as *const AudioStats as *const u8;
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), bytes);
            }
            128
        }
        AUDIO_IOCTL_RT => {
            crate::sched_cfs::set_current_class(PriorityClass::Realtime);
            0
        }
        AUDIO_IOCTL_TONE => {
            // 入参 [freq:u32, dur_ms:u32]（sys_ioctl 已从用户缓冲拷贝）
            let freq = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
            let dur = u32::from_le_bytes([out[4], out[5], out[6], out[7]]);
            async_tone(freq, dur);
            0
        }
        AUDIO_IOCTL_RING_START => {
            let freq = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
            ring_start(freq);
            0
        }
        AUDIO_IOCTL_RING_STOP => {
            ring_stop();
            0
        }
        AUDIO_IOCTL_ACQUIRE => {
            let owner = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
            acquire_focus(owner)
        }
        AUDIO_IOCTL_RELEASE => {
            let owner = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
            release_focus(owner);
            0
        }
        AUDIO_IOCTL_GET_STATE => {
            if out.len() < 8 {
                return -1;
            }
            out[0..4].copy_from_slice(&unsafe { FOCUS }.to_le_bytes());
            out[4..8].copy_from_slice(&unsafe { ROUTE }.to_le_bytes());
            8
        }
        AUDIO_IOCTL_SET_ROUTE => {
            let route = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
            unsafe { ROUTE = route.min(ROUTE_BLUETOOTH) };
            // 真机接线：此处应切换外放 / 听筒 / 蓝牙通路；QEMU 仅记录。
            0
        }
        _ => -25, // ENOTTY
    }
}

// ==================== 音频焦点 / 路由 / 异步提示音 ====================

/// 当前音频焦点所有者（FOCUS_NONE / FOCUS_CALL / FOCUS_MUSIC）。
static mut FOCUS: u32 = FOCUS_NONE;
/// 当前音频路由（ROUTE_EARPIECE / ROUTE_SPEAKER / ROUTE_BLUETOOTH）。
static mut ROUTE: u32 = ROUTE_EARPIECE;
/// 铃声任务停止标志。
static mut RING_STOP: bool = false;
/// 铃声任务是否在运行。
static mut RING_RUNNING: bool = false;

/// 申请音频焦点。通话焦点可抢占音乐；音乐在通话焦点存在时被拒绝（EBUSY）。
fn acquire_focus(owner: u32) -> i32 {
    unsafe {
        match owner {
            FOCUS_CALL => {
                // 通话优先：抢占音乐焦点并停掉其播放
                if FOCUS == FOCUS_MUSIC {
                    stop();
                }
                FOCUS = FOCUS_CALL;
                0
            }
            FOCUS_MUSIC => {
                if FOCUS == FOCUS_CALL {
                    return -16; // EBUSY：通话中禁止媒体播放
                }
                FOCUS = FOCUS_MUSIC;
                0
            }
            _ => -22, // EINVAL
        }
    }
}

/// 释放音频焦点（仅所有者可释放）。
fn release_focus(owner: u32) {
    unsafe {
        if FOCUS == owner {
            FOCUS = FOCUS_NONE;
        }
    }
}

/// 单音任务：播放一段音调后退出（内核任务，不阻塞 ioctl 调用方）。
fn tone_task(arg: usize) -> ! {
    let freq = (arg >> 32) as u32;
    let dur = arg as u32;
    let _ = beep(freq, dur);
    crate::sched_cfs::exit_current()
}

/// 铃声任务：1 秒响 / 2 秒停循环，直到 RING_STOP。
fn ring_task(arg: usize) -> ! {
    let freq = arg as u32;
    unsafe { RING_STOP = false };
    loop {
        if unsafe { RING_STOP } {
            break;
        }
        let _ = beep(freq, 1000);
        // 停 2 秒（期间检查停止标志）
        let deadline = crate::timer::now_us() + 2_000_000;
        while crate::timer::now_us() < deadline {
            if unsafe { RING_STOP } {
                break;
            }
            crate::sched_cfs::sleep_until(deadline.min(crate::timer::now_us() + 50_000));
        }
        if unsafe { RING_STOP } {
            break;
        }
    }
    unsafe { RING_RUNNING = false };
    crate::sched_cfs::exit_current()
}

/// 异步播放单音：派生一个内核任务执行 `beep`，ioctl 立即返回。
fn async_tone(freq: u32, dur_ms: u32) {
    if freq == 0 || dur_ms == 0 {
        return;
    }
    let arg = ((freq as u64) << 32) | (dur_ms as u64);
    let pid = crate::process::spawn(
        "audio-tone",
        tone_task,
        arg as usize,
        crate::sched_cfs::PriorityClass::Normal,
        0,
    );
    if pid == u32::MAX {
        crate::uart_send_str("[audio] 单音任务派生失败\n");
    }
}

/// 开始循环铃声：停止旧铃声后派生铃声任务。
fn ring_start(freq: u32) {
    if freq == 0 {
        return;
    }
    unsafe {
        RING_STOP = true; // 让旧任务退出
        if RING_RUNNING {
            // 等待旧任务退出后重新派生（beep 内 yield，任务很快收敛）
            crate::uart_send_str("[audio] 铃声已在运行，重新启动\n");
        }
    }
    let pid = crate::process::spawn(
        "audio-ring",
        ring_task,
        freq as usize,
        crate::sched_cfs::PriorityClass::Normal,
        0,
    );
    if pid == u32::MAX {
        crate::uart_send_str("[audio] 铃声任务派生失败\n");
        return;
    }
    unsafe { RING_RUNNING = true };
}

/// 停止循环铃声（设置标志；任务在当前响铃结束后退出，延迟 ≤ 1 秒）。
fn ring_stop() {
    unsafe { RING_STOP = true };
}

// ==================== 初始化 ====================

/// 初始化音频子系统：探测 virtio-sound、注册 /dev/audio。
///
/// 返回是否有可用后端。播放的启动（PCM_START）延迟到首次 write / 显式
/// START（此时 GIC 与中断已就绪）。
pub fn init(devs: &[crate::net_virtio::MmioDev]) -> bool {
    // 1. 探测并配置后端（virtio-sound，modern/legacy 均可）
    let ok = crate::virtio_snd::init(devs);
    unsafe {
        BACKEND_READY = ok;
    }

    // 2. 注册字符设备
    let ops = crate::devfs::ChrOps {
        read: audio_read,
        write: audio_write,
        ioctl: audio_ioctl,
    };
    crate::devfs::register_chrdev(AUDIO_MAJOR, "audio", ops);
    crate::vfs::mknod("/dev/audio", crate::vfs::FileType::Chr, AUDIO_MAJOR, AUDIO_MINOR);

    if ok {
        crate::kinfo! {
            crate::uart_send_str("[audio] /dev/audio 就绪，后端=virtio-sound 48kHz/16bit/立体声\n");
        }
    } else {
        crate::kinfo! { crate::uart_send_str("[audio] 无 virtio-sound 设备，/dev/audio 未启用\n"); }
    }
    ok
}

/// 播放设备不可读（纯输出），返回 EOF。
fn audio_read(_minor: u32, _flags: u32, _offset: usize, _buf: &mut [u8]) -> i32 {
    -1
}

// ==================== 音调合成（启动提示音 / shell beep） ====================

/// 正弦表（1024 项，i16 满幅）。
const SINE_LEN: usize = 1024;
static mut SINE: [i16; SINE_LEN] = [0; SINE_LEN];
static mut SINE_READY: bool = false;

/// 构建正弦表（Bhaskara I 近似，无需 libm）。
fn build_sine() {
    unsafe {
        if SINE_READY {
            return;
        }
        let pi = core::f32::consts::PI;
        let two_pi = 2.0 * pi;
        for i in 0..SINE_LEN {
            let x = two_pi * i as f32 / SINE_LEN as f32;
            // x ∈ [0, 2π) → 折叠到 [0, π/2] 的 Bhaskara 近似
            let mut xx = x % two_pi;
            let neg = xx > pi;
            if neg {
                xx -= pi;
            }
            if xx > pi / 2.0 {
                xx = pi - xx;
            }
            let num = 16.0 * xx * (pi - xx);
            let den = 5.0 * pi * pi - 4.0 * xx * (pi - xx);
            let v = if den == 0.0 { 0.0 } else { num / den };
            let v = if neg { -v } else { v };
            SINE[i] = (v * 32767.0) as i16;
        }
        SINE_READY = true;
    }
}

/// 合成一块（10ms）正弦波（16bit 立体声），带首尾淡入淡出防爆音。
///
/// `chunk_idx` / `total_chunks`：全局淡入淡出跨整段音调。
fn synth_chunk(freq: u32, chunk_idx: u32, total_chunks: u32, out: &mut [u8]) {
    build_sine();
    let step: u32 = (freq as u64 * SINE_LEN as u64 / SAMPLE_RATE as u64) as u32;
    if step == 0 {
        out.fill(0);
        return;
    }
    let fade_chunks = (total_chunks / 20).max(1); // 5% 淡入淡出
    let phase_base = chunk_idx as u64 * CHUNK_FRAMES as u64 * step as u64;
    for i in 0..CHUNK_FRAMES {
        let phase = (phase_base + i as u64 * step as u64) & (SINE_LEN as u64 - 1);
        let mut v = unsafe { SINE[phase as usize] as i32 };
        // 0.4 幅度
        v = (v * 13107) >> 15;
        // 淡入淡出（基于块位置）
        if chunk_idx < fade_chunks {
            v = v * (chunk_idx + 1) as i32 / fade_chunks as i32;
        } else if chunk_idx >= total_chunks - fade_chunks {
            let remain = total_chunks - chunk_idx;
            v = v * remain as i32 / fade_chunks as i32;
        }
        let s = v.clamp(-32767, 32767) as i16;
        let off = i * 4;
        out[off] = (s as u16 & 0xFF) as u8;
        out[off + 1] = ((s as u16 >> 8) & 0xFF) as u8;
        out[off + 2] = (s as u16 & 0xFF) as u8;
        out[off + 3] = ((s as u16 >> 8) & 0xFF) as u8;
    }
}

/// 播放指定频率音调 `ms` 毫秒（同步：等待播放完成）。
///
/// 供启动提示音与 shell `beep` 使用。无后端返回 -1。
pub fn beep(freq: u32, ms: u32) -> i32 {
    if !unsafe { BACKEND_READY } {
        return -1;
    }
    let ms = ms.clamp(10, 4000);
    let total_chunks = ((ms + 9) / 10).max(1);
    let t0 = crate::timer::now_us();

    // 确保已启动
    start();

    // 逐块合成并写入环形缓冲
    for c in 0..total_chunks {
        let mut chunk = [0u8; CHUNK_BYTES];
        synth_chunk(freq, c, total_chunks, &mut chunk);
        let n = audio_write(0, &chunk);
        if n != CHUNK_BYTES as i32 {
            break; // 环形缓冲满：丢弃剩余（音量测试场景足够）
        }
    }

    // 等待播放完成（hw_frames 追平本次写入）
    let target = unsafe { SW_FRAMES };
    let deadline = t0 + ms as u64 * 1000 + 500_000;
    while unsafe { HW_FRAMES } < target {
        if crate::timer::now_us() >= deadline {
            break;
        }
        crate::sched_cfs::yield_now();
    }
    0
}

/// 启动提示音（init 进程内调用，880Hz / 120ms）。
pub fn boot_beep() {
    if unsafe { BACKEND_READY } {
        crate::kinfo! { crate::uart_send_str("[audio] 启动提示音\n"); }
        beep(880, 120);
    }
}

/// 当前是否有可用后端。
pub fn backend_ready() -> bool {
    unsafe { BACKEND_READY }
}

/// 取一份统计快照（shell `audiolat` 采样用）。
pub fn stats_snapshot() -> AudioStats {
    // 直接读取全局状态构造（与 ioctl AUDIO_IOCTL_STATS 的 128 字节布局一致）
    AudioStats {
        running: unsafe { RUNNING as u32 },
        rate: SAMPLE_RATE,
        channels: CHANNELS,
        bits: SAMPLE_BITS,
        sw_frames: unsafe { SW_FRAMES },
        hw_frames: unsafe { HW_FRAMES },
        underruns: unsafe { UNDERRUNS },
        late_writes: unsafe { LATE_WRITES },
        irq_count: unsafe { IRQ_COUNT },
        period_min_us: if unsafe { PERIOD_MIN } == u64::MAX { 0 } else { unsafe { PERIOD_MIN } },
        period_avg_us: stats_avg(unsafe { PERIOD_SUM }, unsafe { PERIOD_N }),
        period_max_us: unsafe { PERIOD_MAX },
        refill_min_us: if unsafe { REFILL_MIN } == u64::MAX { 0 } else { unsafe { REFILL_MIN } },
        refill_avg_us: stats_avg(unsafe { REFILL_SUM }, unsafe { REFILL_N }),
        refill_max_us: unsafe { REFILL_MAX },
        dma_min_us: if unsafe { DMA_MIN } == u64::MAX { 0 } else { unsafe { DMA_MIN } },
        dma_avg_us: stats_avg(unsafe { DMA_SUM }, unsafe { DMA_N }),
        dma_max_us: unsafe { DMA_MAX },
    }
}

/// 合成一块 10ms 正弦波到 `out`（shell `audiolat` / 内核测试用）。
pub fn beep_chunk(freq: u32, chunk_idx: u32, out: &mut [u8]) {
    synth_chunk(freq, chunk_idx, 1, out);
}

/// 打印音频状态与统计（shell `audio` 命令）。
pub fn stats_dump() {
    if !unsafe { BACKEND_READY } {
        crate::uart_send_str("audio: 无后端\n");
        return;
    }
    crate::uart_send_str("audio: 后端=virtio-sound ");
    crate::uart_send_str(if unsafe { RUNNING } { "运行中" } else { "已停止" });
    crate::uart_send_str("\n");
    crate::uart_send_str("  格式: ");
    crate::print_dec(SAMPLE_RATE as u64);
    crate::uart_send_str("Hz ");
    crate::print_dec(CHANNELS as u64);
    crate::uart_send_str("ch ");
    crate::print_dec(SAMPLE_BITS as u64);
    crate::uart_send_str("bit, chunk=");
    crate::print_dec(CHUNK_FRAMES as u64);
    crate::uart_send_str(" 帧（10ms）\n");
    crate::uart_send_str("  位置: sw=");
    crate::print_dec(unsafe { SW_FRAMES });
    crate::uart_send_str(" hw=");
    crate::print_dec(unsafe { HW_FRAMES });
    crate::uart_send_str(" 帧（缓冲 ");
    let buffered = unsafe { SW_FRAMES.saturating_sub(HW_FRAMES) };
    crate::print_dec(buffered * 1000 / SAMPLE_RATE as u64);
    crate::uart_send_str(" ms）\n");
    crate::uart_send_str("  underruns=");
    crate::print_dec(unsafe { UNDERRUNS });
    crate::uart_send_str(" late_writes=");
    crate::print_dec(unsafe { LATE_WRITES });
    crate::uart_send_str(" irq_count=");
    crate::print_dec(unsafe { IRQ_COUNT });
    crate::uart_send_str("\n");
    crate::uart_send_str("  IRQ 周期（us）min=");
    crate::print_dec(if unsafe { PERIOD_MIN } == u64::MAX { 0 } else { unsafe { PERIOD_MIN } });
    crate::uart_send_str(" avg=");
    crate::print_dec(stats_avg(unsafe { PERIOD_SUM }, unsafe { PERIOD_N }));
    crate::uart_send_str(" max=");
    crate::print_dec(unsafe { PERIOD_MAX });
    crate::uart_send_str("\n");
    crate::uart_send_str("  IRQ 补充（us）min=");
    crate::print_dec(if unsafe { REFILL_MIN } == u64::MAX { 0 } else { unsafe { REFILL_MIN } });
    crate::uart_send_str(" avg=");
    crate::print_dec(stats_avg(unsafe { REFILL_SUM }, unsafe { REFILL_N }));
    crate::uart_send_str(" max=");
    crate::print_dec(unsafe { REFILL_MAX });
    crate::uart_send_str("\n");
    crate::uart_send_str("  提交->消费（us）min=");
    crate::print_dec(if unsafe { DMA_MIN } == u64::MAX { 0 } else { unsafe { DMA_MIN } });
    crate::uart_send_str(" avg=");
    crate::print_dec(stats_avg(unsafe { DMA_SUM }, unsafe { DMA_N }));
    crate::uart_send_str(" max=");
    crate::print_dec(unsafe { DMA_MAX });
    crate::uart_send_str("\n");
    crate::uart_send_str("  IRQ->DAC 端到端 avg=");
    crate::print_dec(stats_avg(unsafe { REFILL_SUM }, unsafe { REFILL_N })
        + stats_avg(unsafe { DMA_SUM }, unsafe { DMA_N }));
    crate::uart_send_str(" us\n");
}
