//! TTY / 伪终端核心（Phase 21）。
//!
//! 提供虚拟终端会话抽象：`/dev/ttyN`（N=1..MAX_TTY）为图形终端应用（主侧）
//! 的字符设备接口；内核 Shell 绑定会话 0（从侧），等价于 Unix PTY 中
//! "fork 出的 shell 子进程"（本内核的 shell 是内核进程，故从侧直接内建于
//! 内核）。`/dev/ptmx` 预留下一会话分配（多终端）。
//!
//! 数据流（PTY 主从对）：
//! ```text
//!   图形终端（EL0，主）                      内核 Shell（EL1，从）
//!   write(/dev/ttyN, 键码) ──► in_q ────────► read_line() 行编辑/信号
//!   read(/dev/ttyN)    ◄── out_q ◄──────────  uart_send_char()（屏幕输出+回显）
//! ```
//!
//! - 行编辑（退格/左右移动/历史/Tab 补全）、Ctrl+C/D/Z 信号由 Shell 的
//!   `read_line` 完成（见 shell.rs），本模块只承担两侧队列与会话管理。
//! - 输出镜像：`main.rs::uart_send_char` 末尾调用 [`echo_mirror`]，把内核
//!   串口输出（含 boot 日志与 Shell 运行结果）镜像到活动会话的 out_q，
//!   由图形终端渲染 —— 未附接时为零开销的布尔判断。
//! - 会话容量：`MAX_TTY` 个槽位；初始范围单终端（会话 0 常用且内核 Shell
//!   绑定于其上），多终端（会话 1..N）经 `/dev/ptmx` 预留。

use core::arch::asm;

/// 会话槽位数量（/dev/tty1..ttyN；tty0 为 UART 保留）。
pub const MAX_TTY: usize = 4;
/// 每次主侧写允许的最大字节数（读入缓冲）。
const IN_Q_SIZE: usize = 4096;
/// 输出镜像缓冲大小（屏幕字节流；溢出时置 overflow 标志并丢弃）。
const OUT_Q_SIZE: usize = 16384;

/// 单个 TTY 会话。
#[derive(Clone, Copy)]
struct Session {
    /// 主侧→从侧：图形终端写入的原始键码。
    in_q: [u8; IN_Q_SIZE],
    in_head: usize,
    in_tail: usize,
    in_count: usize,
    /// 从侧→主侧：内核输出（Shell 结果 / 回显）字节流。
    out_q: [u8; OUT_Q_SIZE],
    out_head: usize,
    out_tail: usize,
    out_count: usize,
    /// 主侧是否已附接（GUI 终端打开）。
    attached: bool,
    /// 输出溢出（GUI 读得慢或未读时丢弃）。
    overflow: bool,
    /// 窗口尺寸（行列）。
    cols: u16,
    rows: u16,
}

const EMPTY_SESSION: Session = Session {
    in_q: [0; IN_Q_SIZE],
    in_head: 0,
    in_tail: 0,
    in_count: 0,
    out_q: [0; OUT_Q_SIZE],
    out_head: 0,
    out_tail: 0,
    out_count: 0,
    attached: false,
    overflow: false,
    cols: 0,
    rows: 0,
};

static mut SESSIONS: [Session; MAX_TTY] = [EMPTY_SESSION; MAX_TTY];

/// 屏蔽 IRQ（镜像可能来自中断被压栈的打印路径；环形操作需原子）。
unsafe fn irq_save() -> u64 {
    let d: u64;
    asm!("csrr {}, sstatus", out(reg) d);
    asm!("csrci sstatus, 2");
    d
}

unsafe fn irq_restore(d: u64) {
    asm!("csrw sstatus, {}", in(reg) d);
}

fn minor_to_idx(minor: u32) -> usize {
    // minor 0（/dev/tty，UART 直通）按历史行为映射到会话 0（devfs 对
    // TIOCGWINSZ 等未特判 minor 0，映射到 Shell 会话尺寸是合理回退）。
    // minor 1..=MAX_TTY 对应会话 0..=MAX_TTY-1；再大的 minor 返回哨兵值
    // （idx_ok 恒 false 拒绝）：此前钳位到合法会话，会把任意 /dev/ttyN 的
    // 读写路由到最后一个会话（会话隔离被破坏）。
    match minor {
        0 => 0,
        m @ 1.. => match (m - 1) as usize {
            i if i < MAX_TTY => i,
            _ => MAX_TTY,
        },
    }
}

fn idx_ok(i: usize) -> bool {
    i < MAX_TTY
}

// ==================== 初始化 ====================

/// 初始化所有会话（幂等；可在 devfs 注册前调用）。
pub fn init() {
    unsafe {
        for s in SESSIONS.iter_mut() {
            *s = EMPTY_SESSION;
        }
        // 会话 0 为默认（Shell 绑定）；宽高按 540x960 终端默认值。
        SESSIONS[0].cols = 80;
        SESSIONS[0].rows = 24;
    }
}

// ==================== 会话管理 ====================

/// 附接主侧（图形终端打开 /dev/ttyN）。重复附接无副作用。
pub fn attach(minor: u32) -> bool {
    let i = minor_to_idx(minor);
    if !idx_ok(i) {
        return false;
    }
    unsafe {
        let s = &mut SESSIONS[i];
        s.attached = true;
        s.overflow = false;
    }
    true
}

/// 解除附接（图形终端退出/关闭）。
pub fn detach(minor: u32) {
    let i = minor_to_idx(minor);
    if !idx_ok(i) {
        return;
    }
    unsafe {
        SESSIONS[i].attached = false;
    }
}

/// 会话是否已附接主侧。
pub fn attached(minor: u32) -> bool {
    let i = minor_to_idx(minor);
    if !idx_ok(i) {
        return false;
    }
    unsafe { SESSIONS[i].attached }
}

/// 记录输出溢出已被图形终端消费（渲染"截断"标记后调用清除）。
pub fn overflow_take(minor: u32) -> bool {
    let i = minor_to_idx(minor);
    if !idx_ok(i) {
        return false;
    }
    unsafe {
        let o = SESSIONS[i].overflow;
        SESSIONS[i].overflow = false;
        o
    }
}

/// 设置会话窗口尺寸（TIOCSWINSZ）；返回是否成功。
pub fn set_win_size(minor: u32, cols: u16, rows: u16) -> bool {
    let i = minor_to_idx(minor);
    if !idx_ok(i) {
        return false;
    }
    unsafe {
        SESSIONS[i].cols = if cols == 0 { 80 } else { cols };
        SESSIONS[i].rows = if rows == 0 { 24 } else { rows };
    }
    true
}

/// 会话窗口尺寸（TIOCGWINSZ 用）。
pub fn win_size(minor: u32) -> (u16, u16) {
    let i = minor_to_idx(minor);
    if !idx_ok(i) {
        return (0, 0);
    }
    unsafe { (SESSIONS[i].cols, SESSIONS[i].rows) }
}

// ==================== 主侧接口（图形终端） ====================

/// 主侧读：取出从侧（Shell）写入 out_q 的屏幕输出字节。
pub fn screen_get(minor: u32, out: &mut [u8]) -> usize {
    let i = minor_to_idx(minor);
    if !idx_ok(i) || out.is_empty() {
        return 0;
    }
    unsafe {
        let _d = irq_save();
        let s = &mut SESSIONS[i];
        let n = out.len().min(s.out_count);
        for k in 0..n {
            out[k] = s.out_q[s.out_head];
            s.out_head = (s.out_head + 1) % OUT_Q_SIZE;
            s.out_count -= 1;
        }
        irq_restore(_d);
        n
    }
}

/// 主侧写：把图形终端的键码/按键序列写入 in_q（供 Shell 行编辑读取）。
pub fn screen_put(minor: u32, data: &[u8]) -> usize {
    let i = minor_to_idx(minor);
    if !idx_ok(i) {
        return 0;
    }
    unsafe {
        let _d = irq_save();
        let s = &mut SESSIONS[i];
        let mut n = 0usize;
        for &c in data {
            if s.in_count >= IN_Q_SIZE {
                break;
            }
            s.in_q[s.in_tail] = c;
            s.in_tail = (s.in_tail + 1) % IN_Q_SIZE;
            s.in_count += 1;
            n += 1;
        }
        irq_restore(_d);
        n
    }
}

// ==================== 从侧接口（Shell） ====================

/// Shell 取一个原始键码：优先从活动会话（0）in_q，附接时不再读 UART。
pub fn keystroke() -> Option<u8> {
    if !attached(1) {
        return None;
    }
    unsafe {
        let _d = irq_save();
        let s = &mut SESSIONS[0];
        let c = if s.in_count > 0 {
            let v = s.in_q[s.in_head];
            s.in_head = (s.in_head + 1) % IN_Q_SIZE;
            s.in_count -= 1;
            Some(v)
        } else {
            None
        };
        irq_restore(_d);
        c
    }
}

/// Shell 组合输入源：GUI 会话附接时只取 TTY 键码（不读 UART，避免 GUI 会话
/// 激活期间串口键盘事件被 shell 静默吞掉）；否则回退 UART（串口控制台）。
pub fn shell_input() -> Option<u8> {
    if attached(1) {
        keystroke()
    } else {
        crate::uart_try_read_char()
    }
}

/// 输出镜像：`uart_send_char` 末尾调用，把串口输出字符推入会话 0 的 out_q。
///
/// 未附接时零开销；满时丢弃并置 overflow（GUI 屏显"输出截断"提示）。
pub fn echo_mirror(c: u8) {
    let i = 0usize;
    unsafe {
        if !SESSIONS[i].attached {
            return;
        }
        let _d = irq_save();
        let s = &mut SESSIONS[i];
        if s.out_count >= OUT_Q_SIZE {
            s.overflow = true;
            irq_restore(_d);
            return;
        }
        s.out_q[s.out_tail] = c;
        s.out_tail = (s.out_tail + 1) % OUT_Q_SIZE;
        s.out_count += 1;
        irq_restore(_d);
    }
}

// ==================== /dev/ptmx（PTY 分配器，多终端预留） ====================

/// ptmx 当前分配的会话（单实例限制：初始范围单终端，多终端预留）。
static mut PTMX_MINOR: u32 = 1;

/// ptmx open 时预留下一个空闲会话，返回其 minor（1..=MAX_TTY），全满返回 0。
///
/// 预留即置 `attached`，后续 `ptmx_alloc` 会跳过它；GUI 打开对应 `/dev/ttyN`
/// 时 `attach` 幂等重申附接。
pub fn ptmx_alloc() -> u32 {
    unsafe {
        for i in 0..MAX_TTY {
            if !SESSIONS[i].attached {
                let minor = (i + 1) as u32;
                PTMX_MINOR = minor;
                SESSIONS[i].attached = true; // 预留会话
                return minor;
            }
        }
    }
    0
}

/// ptmx 当前预留的会话 minor（TIOCGPTN 返回）。
pub fn ptmx_minor() -> u32 {
    unsafe { PTMX_MINOR }
}