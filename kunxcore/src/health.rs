//! 系统健康事件（用户可见的故障告知 + 修复指引）。
//!
//! 目标：「出了问题不让用户蒙在鼓里」——故障遏制/服务监督/启动降级/看门狗
//! 把系统保住的同时，把发生了什么、可能怎么修记录下来：
//!   1. 环形缓冲（最近 32 条）：shell `health` 命令随时查看；
//!   2. 控制台即时打印（[health] 前缀，附修复建议）；
//!   3. 结构化日志（log_write）；
//!   4. 落盘 /var/run/health.log（best-effort；VFS 不可用/IRQ 上下文跳过，
//!      GUI 启动时读取此文件向用户展示系统问题横幅与修复指引）。
//!
//! 文案要求：msg 说清「哪个子系统出了什么事」，fix 给出「用户能做的
//! 修复动作」（重启该服务 / 重启设备 / 清除数据 / 查看崩溃报告）。

use crate::log;

// ==================== 事件与环形缓冲 ====================

/// 严重级别（数值越小越严重）。
pub const SEV_CRITICAL: u8 = 0;
pub const SEV_WARN: u8 = 1;
pub const SEV_INFO: u8 = 2;

/// 单条事件：定长缓冲（无堆），msg/fix 均以 NUL 结尾截断存储。
struct HealthEvent {
    t_ms: u64,
    sev: u8,
    subsys: &'static str,
    msg: [u8; 96],
    fix: [u8; 160],
}

const EVT_CAP: usize = 32;
const EMPTY_EVT: HealthEvent = HealthEvent {
    t_ms: 0,
    sev: SEV_INFO,
    subsys: "",
    msg: [0; 96],
    fix: [0; 160],
};

static mut RING: [HealthEvent; EVT_CAP] = [EMPTY_EVT; EVT_CAP];
static mut RING_N: usize = 0;
/// 累计 CRITICAL 事件数（遏制风暴等系统性故障的粗粒度信号）。
static mut CRIT_COUNT: u32 = 0;

fn sev_name(sev: u8) -> &'static str {
    match sev {
        SEV_CRITICAL => "严重",
        SEV_WARN => "警告",
        _ => "提示",
    }
}

fn copy_str(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
    dst[n] = 0;
}

// IRQ 屏蔽窗口（各模块私有同款实现；S 态 mie 位）
unsafe fn irq_save() -> u64 {
    let mut m: u64;
    unsafe { core::arch::asm!("csrrc {}, sstatus, 2", out(reg) m) };
    m
}

unsafe fn irq_restore(saved: u64) {
    if saved & 2 != 0 {
        unsafe { core::arch::asm!("csrsi sstatus, 2") };
    }
}

fn evt_str(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

// ==================== 上报入口 ====================

/// 记录一条健康事件：环形缓冲 + 控制台 + 结构化日志 + 落盘（尽力而为）。
///
/// `irq_ctx` = 调用来自中断上下文（此时禁止 VFS 落盘，只留内存/控制台）。
pub fn report_irq(irq_ctx: bool, sev: u8, subsys: &'static str, msg: &str, fix: &str) {
    // IRQ 屏蔽窗口（L-4）：IRQ 上下文（mem_pressure 经 tick / wdt / 遏制）
    // 与进程上下文并发上报时，单核 IRQ 可打断 copy_str 中途 → 事件撕裂、
    // RING_N 丢计数。log_write 同款做法：写入环形槽全程关中断。
    let saved = unsafe { irq_save() };
    let evt = unsafe {
        let idx = RING_N % EVT_CAP;
        RING[idx].t_ms = crate::timer::now_us() / 1000;
        RING[idx].sev = sev;
        RING[idx].subsys = subsys;
        copy_str(&mut RING[idx].msg, msg);
        copy_str(&mut RING[idx].fix, fix);
        RING_N += 1;
        if sev == SEV_CRITICAL {
            CRIT_COUNT = CRIT_COUNT.saturating_add(1);
        }
        &RING[idx]
    };
    unsafe { irq_restore(saved) };
    // 控制台：用户可见的「出了什么事 + 怎么修」
    crate::uart_send_str("[health][");
    crate::uart_send_str(sev_name(sev));
    crate::uart_send_str("][");
    crate::uart_send_str(subsys);
    crate::uart_send_str("] ");
    crate::uart_send_str(evt_str(&evt.msg));
    if !evt_str(&evt.fix).is_empty() {
        crate::uart_send_str("\n[health] 修复建议：");
        crate::uart_send_str(evt_str(&evt.fix));
    }
    crate::uart_send_str("\n");
    // 结构化日志（log 快照会进 tombstone）
    log::log_write(
        if sev == SEV_CRITICAL { log::LOG_CRIT } else { log::LOG_WARNING },
        "health",
        evt_str(&evt.msg),
    );
    // 落盘（非 IRQ 上下文才允许碰 VFS）
    if !irq_ctx {
        append_file(evt);
    }
}

/// 常规上下文上报（进程上下文/启动阶段）。
pub fn report(sev: u8, subsys: &'static str, msg: &str, fix: &str) {
    report_irq(false, sev, subsys, msg, fix);
}

/// 遍历健康事件（旧→新）。shell `health` 命令与内部诊断用。
pub fn for_each(f: &mut dyn FnMut(u64, u8, &str, &str, &str)) {
    let n = unsafe { RING_N };
    for k in 0..n.min(EVT_CAP) {
        let idx = if n > EVT_CAP { (n - EVT_CAP + k) % EVT_CAP } else { k };
        let e = unsafe { &RING[idx] };
        f(e.t_ms, e.sev, e.subsys, evt_str(&e.msg), evt_str(&e.fix));
    }
}

/// 累计 CRITICAL 事件数。
pub fn critical_count() -> u32 {
    unsafe { CRIT_COUNT }
}

// ==================== 落盘 ====================

/// 追加一行到 /var/run/health.log（best-effort，失败静默——内存环形缓冲
/// 与控制台已有副本）。格式：`<t_ms>ms <sev> <subsys> | <msg> | <fix>`。
fn append_file(evt: &HealthEvent) {
    let mut line = [0u8; 300];
    let mut n = 0usize;
    let push = |s: &str, line: &mut [u8; 300], n: &mut usize| {
        for &c in s.as_bytes() {
            if *n < line.len() - 1 {
                line[*n] = c;
                *n += 1;
            }
        }
    };
    // 手工拼行（无 fmt 依赖）：时间戳
    let mut tb = [0u8; 20];
    let mut i = tb.len();
    let mut v = evt.t_ms;
    loop {
        i -= 1;
        tb[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    push(core::str::from_utf8(&tb[i..]).unwrap_or(""), &mut line, &mut n);
    push("ms ", &mut line, &mut n);
    push(sev_name(evt.sev), &mut line, &mut n);
    push(" ", &mut line, &mut n);
    push(evt.subsys, &mut line, &mut n);
    push(" | ", &mut line, &mut n);
    push(evt_str(&evt.msg), &mut line, &mut n);
    let fx = evt_str(&evt.fix);
    if !fx.is_empty() {
        push(" | 修复: ", &mut line, &mut n);
        push(fx, &mut line, &mut n);
    }
    push("\n", &mut line, &mut n);

    let fd = crate::vfs::open(
        "/var/run/health.log",
        crate::vfs::O_CREAT | crate::vfs::O_WRONLY | crate::vfs::O_APPEND,
    );
    if fd < 0 {
        return; // /var/run 未就绪（早期启动）：内存+控制台副本足够
    }
    let mut off = 0usize;
    while off < n {
        let w = crate::vfs::write(fd, &line[off..n]);
        if w <= 0 {
            break;
        }
        off += w as usize;
    }
    crate::vfs::close(fd);
}

/// 清空健康日志文件（`health clear` 命令；环形缓冲同步复位）。
pub fn clear() {
    unsafe {
        RING_N = 0;
        RING = [EMPTY_EVT; EVT_CAP];
    }
    let fd = crate::vfs::open(
        "/var/run/health.log",
        crate::vfs::O_CREAT | crate::vfs::O_WRONLY | crate::vfs::O_TRUNC,
    );
    if fd >= 0 {
        crate::vfs::close(fd);
    }
}
