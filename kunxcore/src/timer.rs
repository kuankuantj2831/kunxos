//! ARM Generic Timer 抽象（虚拟定时器 CNTV）。
//!
//! 提供周期定时器：`timer_init` 初始化、`timer_set_interval` 设置周期（微秒）、
//! `timer_register_handler` 注册到期回调。计数频率从 `CNTFRQ_EL0` 读取，
//! 因此无需依赖平台硬编码。

use core::arch::asm;

/// 虚拟定时器 PPI 中断号（GIC PPI 11 → ID 27，架构定义，非设备树描述）。
pub const TIMER_IRQ: u32 = 27;

/// 定时器到期回调类型。
pub type TimerHandler = fn();

/// 支持的最大回调数（调度器 tick + 动画帧 tick 等）。
const MAX_TIMER_HANDLERS: usize = 4;

/// 当前注册的回调（按注册顺序调用）。
static mut HANDLERS: [Option<TimerHandler>; MAX_TIMER_HANDLERS] = [None; MAX_TIMER_HANDLERS];

/// 当前周期（微秒）。
static mut INTERVAL_US: u64 = 0;

/// 读取计数频率 `CNTFRQ_EL0`。
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn read_cntfrq() -> u64 {
    let v: u64;
    asm!("mrs {}, cntfrq_el0", out(reg) v);
    v
}

/// 读取虚拟计数器 `CNTVCT_EL0` 当前值（单调递增）。
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn read_cntvct() -> u64 {
    let v: u64;
    asm!("mrs {}, cntvct_el0", out(reg) v);
    v
}

/// 当前时刻（微秒，基于虚拟计数器），用于网络超时等需要真实时间的场景。
///
/// ★ 此前 `freq/1_000_000` 先截断再除计数器：CNTFRQ=62.5MHz 时 d=62，
/// 每秒慢 ~0.8%（perf 帧统计 / 交互 deadline / 网络超时全部同比偏大）。
/// 改 `(cnt/freq)*1e6 + (cnt%freq)*1e6/freq`，全程 u64 无溢出。
pub fn now_us() -> u64 {
    let freq = unsafe { read_cntfrq() };
    if freq == 0 {
        return 0;
    }
    let cnt = unsafe { read_cntvct() };
    let secs = cnt / freq;
    let frac = cnt % freq;
    secs.saturating_mul(1_000_000)
        .saturating_add(frac.saturating_mul(1_000_000) / freq)
}

/// 写 `CNTV_TVAL_EL0`（虚拟定时器递减初值）。
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn write_cntv_tval(val: u64) {
    asm!("msr cntv_tval_el0, {}", in(reg) val);
}

/// 写 `CNTV_CTL_EL0`（bit0 使能，bit1 屏蔽中断）。
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn write_cntv_ctl(val: u64) {
    asm!("msr cntv_ctl_el0, {}", in(reg) val);
}

/// 按当前周期重装定时器：写入递减初值并使能，清除 pending 状态并重新计时。
fn reload() {
    let freq = unsafe { read_cntfrq() };
    // tval = freq * us / 1_000_000（向上取整，避免周期比预期更短）。
    // freq 或周期为 0 时 tval 会变成 0 → 定时器立即重触发 → IRQ 风暴
    // （按优先级饿死 UART 等低优先级中断），此处改用最大间隔兜底。
    let tval = if freq == 0 || unsafe { INTERVAL_US } == 0 {
        u64::MAX
    } else {
        unsafe { INTERVAL_US }
            .checked_mul(freq)
            .map(|v| v.div_ceil(1_000_000))
            .unwrap_or(u64::MAX)
    };
    unsafe {
        write_cntv_tval(tval);
        write_cntv_ctl(1); // ENABLE=1, IMASK=0
    }
}

/// 定时器中断确认：仅重装定时器（清除中断源），供 IRQ 分发层在写 EOI **之前**
/// 调用，避免「EOI 时中断线仍有效 → GIC 立即重触发」造成的中断风暴
/// （该风暴会按优先级饿死 UART RX 等低优先级中断）。
pub fn timer_ack() {
    reload();
}

/// 初始化定时器，使用默认 10ms 周期。
pub fn timer_init() {
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

/// 定时器中断处理：重装定时器后调用所有已注册回调。
///
/// 注意：回调可能不返回（如调度器的上下文切换），此时本函数直到
/// 该任务被再次调度时才返回。
///
/// `#[inline(never)]`：防止本函数与回调（尤其是 `sched_cfs::tick`）被内联合并。
/// 一旦 tick 被内联进本函数，tick 内上下文切换后的恢复点会落在本函数的中段，
/// 其后的 `pop x30; ret` 弹出的是错误栈槽位，导致跳转垃圾地址（EL1 PC=0 崩溃）。
#[inline(never)]
pub fn timer_handle_irq() {
    // 重装定时器，清除中断并重新开始计时
    reload();
    unsafe {
        for h in HANDLERS.iter() {
            if let Some(h) = h {
                h();
            }
        }
    }
}

// ==================== 动画专用定时器（CNTP，独立于调度 tick） ====================
//
// 动画帧推进与调度器 tick 解耦：调度 tick 走虚拟定时器 CNTV（IRQ 27，动态
// 10ms 周期），动画帧推进走**非安全 EL1 物理定时器 CNTP**（IRQ 30，固定
// 8.33ms 周期 = 120Hz，与 docs/animations.md 的帧时钟规范一致）。两条硬件
// 通道互不干扰，动画帧间隔不再被调度器上下文切换抖动污染，也不再被 100Hz
// 上限限制（120Hz 帧预算 8.33ms，留 ~1ms 余量给合成/翻转）。

/// 物理定时器 CNTP PPI 中断号（GIC PPI 14 → ID 30，QEMU virt DTB 确认）。
pub const TIMER2_IRQ: u32 = 30;

/// 动画定时器回调（当前仅 `anim::frame_tick`，永不切换上下文）。
static mut HANDLERS2: [Option<TimerHandler>; 2] = [None; 2];

/// 动画定时器周期（微秒）。
static mut INTERVAL2_US: u64 = 0;

/// 读取物理计数器 `CNTPCT_EL0`。
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn read_cntpct() -> u64 {
    let v: u64;
    asm!("mrs {}, cntpct_el0", out(reg) v);
    v
}

/// 写 `CNTP_TVAL_EL0`（物理定时器递减初值）。
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn write_cntp_tval(val: u64) {
    asm!("msr cntp_tval_el0, {}", in(reg) val);
}

/// 写 `CNTP_CTL_EL0`（bit0 使能，bit1 屏蔽中断）。
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn write_cntp_ctl(val: u64) {
    asm!("msr cntp_ctl_el0, {}", in(reg) val);
}

/// 按周期重装动画定时器。
fn reload2() {
    let freq = unsafe { read_cntfrq() };
    let tval = if freq == 0 || unsafe { INTERVAL2_US } == 0 {
        u64::MAX
    } else {
        unsafe { INTERVAL2_US }
            .checked_mul(freq)
            .map(|v| v.div_ceil(1_000_000))
            .unwrap_or(u64::MAX)
    };
    unsafe {
        write_cntp_tval(tval);
        write_cntp_ctl(1); // ENABLE=1, IMASK=0
    }
}

/// 设置动画定时器周期（微秒）并立即生效。
pub fn timer2_set_interval(us: u64) {
    unsafe {
        INTERVAL2_US = us;
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

/// 动画定时器中断确认：仅重装（清除中断源），供 IRQ 分发层在写 EOI 之前调用，
/// 避免电平触发重触发风暴（与 `timer_ack` 同理）。
pub fn timer2_ack() {
    reload2();
}

/// 停止动画定时器（CNTP 关使能）：无活动动画时停掉 120Hz 恒定唤醒。
///
/// 动画定时器是系统空转开销的大头（120 次/秒 IRQ 进出 + GIC EOI，TCG 慢宿主
/// 下每 8.33ms 一次异常往返）。`frame_tick` 在最后一个动画完成时调用本函数；
/// 下次 `anim::tween`/`anim::spring` 启动动画时通过 [`timer2_start`] 重新点亮。
pub fn timer2_stop() {
    unsafe {
        // CNTP_CTL：bit0 = ENABLE。写 0 即停止计数与中断触发（不清除重装值）。
        asm!("msr cntp_ctl_el0, {}", in(reg) 0u64);
        asm!("isb");
    }
}

/// 按当前周期重装并启动动画定时器（CNTP）：供动画启动路径在「无动画停表」后
/// 重新点亮。已在运行的定时器重复调用等价于一次平滑 reload，无副作用。
pub fn timer2_start() {
    reload2();
}

/// 动画定时器中断处理：重装后调用回调（`frame_tick` 不切换上下文，安全）。
#[inline(never)]
pub fn timer2_handle_irq() {
    reload2();
    unsafe {
        for h in HANDLERS2.iter() {
            if let Some(h) = h {
                h();
            }
        }
    }
}
