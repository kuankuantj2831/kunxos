//! RISC-V 监督定时器抽象（SBI TIME 扩展 + `time` CSR）。
//!
//! aarch64 有两条独立硬件通道（CNTV 调度 tick / CNTP 动画帧），Sv39 S 模式
//! 只有一条监督定时器（经 SBI `set_timer` 编程）。本模块以**双逻辑通道
//! 复用单硬件通道**保持原 API：两条通道各自维护周期与到期时刻，硬件始终
//! 编程到最近到期者；到期中断按「谁到期谁回调」分发。计数频率取自设备树
//! `/cpus/timebase-frequency`（QEMU virt RISC-V = 10MHz），缺失回退 10MHz。

use core::arch::asm;

use crate::sbi;

// ==================== sstc（stimecmp 直编程） ====================
//
// SBI set_timer 每次重装都走 ecall → M 态 OpenSBI → 返回（TCG 下一次
// 往返数 µs，100Hz tick + 120Hz 动画 = 220 次/秒）。QEMU virt 的 CPU 带
// sstc 扩展（DTB riscv,isa 含 sstc）时可直接写 S 态 stimecmp CSR，写入
// 即清挂起并重装，语义与 SBI set_timer 等价。
// 探测：临时把 stvec 换到恢复桩后 csrr stimecmp —— 非法指令则陷入桩内
// 继续（SIE 本为 0、sepc 会在下次陷入被重写，无副作用）；成功则置标志。
// 要求：调用点处于 IRQ 屏蔽的内核上下文（timer_init 满足）。

/// stimecmp 可用标志（0=未探测/不可用，1=可用）。
static mut STIMECMP_OK: u8 = 0;

/// 安全探测 stimecmp（见上）。# 安全：须 IRQ 屏蔽。
unsafe fn probe_stimecmp() {
    asm!(
        "csrr t0, stvec",
        "la t1, 8f",
        "csrw stvec, t1",
        "csrr t1, 0x14d",        // stimecmp；无 sstc → illegal instruction → 陷入 8f
        "li t2, 1",
        "sb t2, 0({ok})",
        "j 8f",                // 成功路径跳过对齐垫片
        ".balign 4",           // stvec 目标必须 4 字节对齐（不会顺序执行）
        "8:",
        "csrw stvec, t0",
        ok = in(reg) core::ptr::addr_of_mut!(STIMECMP_OK) as usize,
        out("t0") _,
        out("t1") _,
        out("t2") _,
        options(nostack),
    );
}

/// 编程下一个定时器到期：stimecmp 快路径，SBI 回退。
#[inline]
fn program_deadline(stime: u64) {
    if unsafe { STIMECMP_OK } != 0 {
        unsafe { asm!("csrw 0x14d, {t}", t = in(reg) stime, options(nostack)) }; // stimecmp
    } else {
        sbi::set_timer(stime);
    }
}

/// 调度 tick 逻辑通道的 PLIC 外部中断号占位。
///
/// 监督定时器中断不走 PLIC（scause=5，cause 分发），此常量仅维持
/// `register_handler(TIMER_IRQ, …)` 调用点的 ABI 兼容，实际不会出现在
/// PLIC claim 结果中。
pub const TIMER_IRQ: u32 = u32::MAX;
/// 动画帧逻辑通道的占位中断号（同上）。
pub const TIMER2_IRQ: u32 = u32::MAX - 1;

/// 定时器到期回调类型。
pub type TimerHandler = fn();

/// 支持的最大回调数（调度器 tick + 动画帧 tick 等）。
const MAX_TIMER_HANDLERS: usize = 4;

/// 当前注册的回调（按注册顺序调用）。
static mut HANDLERS: [Option<TimerHandler>; MAX_TIMER_HANDLERS] = [None; MAX_TIMER_HANDLERS];

/// 当前周期（微秒）。
static mut INTERVAL_US: u64 = 0;
/// 下次到期时刻（`time` 计数）。
static mut DEADLINE: u64 = u64::MAX;

/// 动画定时器回调（当前仅 `anim::frame_tick`，永不切换上下文）。
static mut HANDLERS2: [Option<TimerHandler>; 2] = [None; 2];
/// 动画定时器周期（微秒）。
static mut INTERVAL2_US: u64 = 0;
/// 动画通道下次到期时刻（`time` 计数）。
static mut DEADLINE2: u64 = u64::MAX;
/// 动画通道是否点亮（stop 后置 false，start 恢复）。
static mut CH2_ENABLED: bool = false;

/// timebase（Hz），`set_timebase` 由启动路径从 DTB 写入。
static mut TIMEBASE: u64 = 10_000_000;

/// 写入 timebase 频率（启动路径解析 `/cpus/timebase-frequency` 后调用）。
pub fn set_timebase(hz: u64) {
    if hz > 0 {
        unsafe { TIMEBASE = hz };
    }
}

/// 读取 timebase 频率。
pub fn frequency() -> u64 {
    unsafe { TIMEBASE }
}

/// 读取 `time` 计数器当前值（单调递增）。
#[inline]
pub fn read_time() -> u64 {
    let v: u64;
    unsafe { asm!("csrr {}, time", out(reg) v, options(nostack)) };
    v
}

/// 当前时刻（微秒，基于 `time` 计数器），用于网络超时等需要真实时间的场景。
///
/// ★ 保持 aarch64 版本的防截断公式：`(cnt/freq)*1e6 + (cnt%freq)*1e6/freq`，
/// 全程 u64 无溢出（timebase=10MHz 时每秒 1e7 计数，乘 1e6 仍在 u64 内）。
pub fn now_us() -> u64 {
    let freq = frequency();
    if freq == 0 {
        return 0;
    }
    let cnt = read_time();
    let secs = cnt / freq;
    let frac = cnt % freq;
    secs.saturating_mul(1_000_000)
        .saturating_add(frac.saturating_mul(1_000_000) / freq)
}

/// 周期（微秒）→ `time` 计数（溢出/非法回退 u64::MAX，定时器停摆由
/// program 兜底为极远未来，避免立即重触发风暴）。
fn us_to_ticks(us: u64) -> u64 {
    let freq = frequency();
    if freq == 0 || us == 0 {
        u64::MAX
    } else {
        us.checked_mul(freq).map(|v| v.div_ceil(1_000_000)).unwrap_or(u64::MAX)
    }
}

/// 把硬件定时器编程到两通道最近到期者。
fn program() {
    let now = read_time();
    let mut due = u64::MAX;
    unsafe {
        if INTERVAL_US > 0 {
            due = due.min(DEADLINE);
        }
        if CH2_ENABLED && INTERVAL2_US > 0 {
            due = due.min(DEADLINE2);
        }
    }
    // 已过期（如停表期间错过）：立刻到期，由分发路径补推进
    let target = due.max(now);
    program_deadline(target);
}

/// 重装调度 tick 通道：从当前时刻起按周期重新计到。
fn reload() {
    let ticks = us_to_ticks(unsafe { INTERVAL_US });
    unsafe {
        DEADLINE = if ticks == u64::MAX { u64::MAX } else { read_time().saturating_add(ticks) };
    }
    program();
}

/// 定时器中断确认：重装调度通道并重编程硬件（供需要显式应答的路径）。
pub fn timer_ack() {
    reload();
}

/// 初始化定时器，使用默认 10ms 周期。
pub fn timer_init() {
    unsafe { probe_stimecmp() };
    if unsafe { STIMECMP_OK } != 0 {
        crate::kinfo! { crate::uart_send_str("[timer] sstc stimecmp 直编程已启用（绕过 SBI ecall）\n"); }
    }
    timer_set_interval(10_000);
}

/// 设置定时器周期（微秒）并立即生效。
pub fn timer_set_interval(us: u64) {
    unsafe {
        INTERVAL_US = us;
    }
    reload();
}

/// 注册定时器到期回调（追加到回调列表，超出容量则忽略）。
pub fn timer_register_handler(handler: TimerHandler) {
    unsafe {
        for h in HANDLERS.iter_mut() {
            if h.is_none() {
                *h = Some(handler);
                return;
            }
        }
    }
}

/// 定时器中断处理：推进到期时刻并重编程硬件后，调用所有已注册回调。
///
/// 注意：回调可能不返回（如调度器的上下文切换），此时本函数直到
/// 该任务被再次调度时才返回。重编程必须在回调前完成（SBI 应答语义：
/// 旧到期已消费，不重设会立刻重触发风暴）。
///
/// `#[inline(never)]`：防止本函数与回调（尤其是 `sched_cfs::tick`）被内联合并。
/// 一旦 tick 被内联进本函数，tick 内上下文切换后的恢复点会落在本函数的中段，
/// 其后的栈弹出会取错误槽位，导致跳转垃圾地址（PC=0 崩溃）。
#[inline(never)]
pub fn timer_handle_irq() {
    let now = read_time();
    let ticks = us_to_ticks(unsafe { INTERVAL_US });
    unsafe {
        // 补推进：停表/长回调期间错过的周期一次跳过（避免追帧风暴）
        if ticks != u64::MAX {
            while DEADLINE <= now {
                DEADLINE = DEADLINE.saturating_add(ticks);
            }
            if DEADLINE == u64::MAX {
                DEADLINE = now.saturating_add(ticks);
            }
        }
    }
    program();
    unsafe {
        for h in HANDLERS.iter() {
            if let Some(h) = h {
                h();
            }
        }
    }
}

// ==================== 动画专用通道（逻辑复用，独立于调度 tick） ====================
//
// 动画帧推进与调度器 tick 解耦：aarch64 上走独立硬件通道 CNTP（固定
// 8.33ms = 120Hz，docs/animations.md 的帧时钟规范）；RISC-V 单监督定时器
// 下本通道与调度 tick 复用同一硬件到期，周期与回调独立维护。

/// 按周期重装动画通道。
fn reload2() {
    let ticks = us_to_ticks(unsafe { INTERVAL2_US });
    unsafe {
        DEADLINE2 = if ticks == u64::MAX { u64::MAX } else { read_time().saturating_add(ticks) };
    }
    program();
}

/// 设置动画定时器周期（微秒）并立即生效。
pub fn timer2_set_interval(us: u64) {
    unsafe {
        INTERVAL2_US = us;
        CH2_ENABLED = true;
    }
    reload2();
}

/// 注册动画定时器回调。
pub fn timer2_register_handler(handler: TimerHandler) {
    unsafe {
        for h in HANDLERS2.iter_mut() {
            if h.is_none() {
                *h = Some(handler);
                return;
            }
        }
    }
}

/// 动画定时器中断确认：仅重装（供外部应答路径）。
pub fn timer2_ack() {
    reload2();
}

/// 停止动画定时器：无活动动画时停掉 120Hz 恒定唤醒。
///
/// 动画定时器是系统空转开销的大头（120 次/秒 IRQ 进出，TCG 慢宿主下每
/// 8.33ms 一次异常往返）。`frame_tick` 在最后一个动画完成时调用本函数；
/// 下次 `anim::tween`/`anim::spring` 启动动画时通过 [`timer2_start`] 重新点亮。
pub fn timer2_stop() {
    unsafe {
        CH2_ENABLED = false;
    }
    program();
}

/// 按当前周期重装并启动动画定时器：供动画启动路径在「无动画停表」后
/// 重新点亮。已在运行的定时器重复调用等价于一次平滑 reload，无副作用。
pub fn timer2_start() {
    unsafe {
        CH2_ENABLED = true;
    }
    reload2();
}

/// 动画定时器是否到期（供统一分发路径判定）。
pub fn ch2_due() -> bool {
    unsafe { CH2_ENABLED && INTERVAL2_US > 0 && DEADLINE2 <= read_time() + 1 }
}

/// 动画定时器中断处理：推进并重编程后调用回调（`frame_tick` 不切换上下文，
/// 安全）。
#[inline(never)]
pub fn timer2_handle_irq() {
    let now = read_time();
    let ticks = us_to_ticks(unsafe { INTERVAL2_US });
    unsafe {
        if ticks != u64::MAX {
            while DEADLINE2 <= now {
                DEADLINE2 = DEADLINE2.saturating_add(ticks);
            }
            if DEADLINE2 == u64::MAX {
                DEADLINE2 = now.saturating_add(ticks);
            }
        }
    }
    program();
    unsafe {
        for h in HANDLERS2.iter() {
            if let Some(h) = h {
                h();
            }
        }
    }
}
