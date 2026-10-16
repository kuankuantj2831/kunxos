#![no_std]  // 裸机环境没有标准库
#![no_main] // 不使用标准 main 入口，改用自定义 _start
// 内核全局状态以 static mut 组织，经 unsafe 访问（单核 + 中断屏蔽临界区），
// 此处显式放行 Rust 2021 的 static_mut_refs 提示。
#![allow(static_mut_refs)]
// 各 Phase 的子系统（图形/UI/输入等）按阶段演进，部分接口在 demo 流程替换后暂未
// 被引用，这里统一放行 dead_code，避免构建日志被大量提示淹没。
#![allow(dead_code)]

extern crate alloc;

// ==================== 内存原语（编译器内建 memcpy/memset 实现） ====================
// no_std 内核必须提供这些符号：编译器对 96KB+ 大对象（Process/ProcPageTable）
// 的 move/清零会生成 memcpy/memset 调用，缺失或不可靠时静默损坏内存。

/// 字节拷贝（src/dst 不重叠）。
///
/// 性能：帧缓冲脏区行拷贝 / ELF 段加载等大块拷贝经此符号，纯字节循环在
/// QEMU 软件渲染与慢速核心上不足以喂满 120Hz 帧预算（540×960 整屏 = 2MB
/// 拷贝）。这里在两端 8 字节对齐时用 RISC-V `ld/sd` 8 字节宽通道，尾部与
/// 未对齐场景回退字节循环 —— 任意对齐都正确。
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut d = dest as usize;
    let mut s = src as usize;
    let end = d.saturating_add(n);
    // 两端 8 字节对齐：每迭代 16B（2×ld + 2×sd，减半循环开销；无向量
    // 扩展时这是基础 ISA 的拷贝极限）
    if (d & 7) == 0 && (s & 7) == 0 {
        while end.saturating_sub(d) >= 16 {
            asm!(
                "ld {v0}, 0({s})",
                "ld {v1}, 8({s})",
                "sd {v0}, 0({d})",
                "sd {v1}, 8({d})",
                "addi {s}, {s}, 16",
                "addi {d}, {d}, 16",
                v0 = out(reg) _,
                v1 = out(reg) _,
                s = inout(reg) s,
                d = inout(reg) d,
                options(nostack),
            );
        }
        while end.saturating_sub(d) >= 8 {
            asm!(
                "ld {v}, 0({s})",
                "sd {v}, 0({d})",
                "addi {s}, {s}, 8",
                "addi {d}, {d}, 8",
                v = out(reg) _,
                s = inout(reg) s,
                d = inout(reg) d,
                options(nostack),
            );
        }
    }
    // 尾部 / 未对齐：字节循环（保持任意对齐语义正确）
    while d < end {
        *((d) as *mut u8) = *((s) as *const u8);
        d += 1;
        s += 1;
    }
    dest
}

/// 字节拷贝（允许重叠）。无重叠/dst 更低走 memcpy 的宽通道，重叠时反向。
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dest as usize) < (src as usize)
        || (dest as usize) >= (src as usize).saturating_add(n)
    {
        memcpy(dest, src, n);
    } else {
        // dst > src 且重叠：反向逐字节保序（volatile 免疫循环惯用语变换）。
        let mut i = n;
        while i > 0 {
            i -= 1;
            (dest.add(i)).write_volatile(src.add(i).read_volatile());
        }
    }
    dest
}

/// 字节填充：8B `sd` 宽通道（页清零 / 画布填充走此路径）。
#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut u8, c: i32, n: usize) -> *mut u8 {
    let mut p = s as usize;
    let end = p.saturating_add(n);
    let v = (c as u8) as u64;
    let word = u64::from_ne_bytes([v as u8; 8]);
    while p < end && (p & 7) != 0 {
        (p as *mut u8).write_volatile(v as u8);
        p += 1;
    }
    while end.saturating_sub(p) >= 16 {
        asm!(
            "sd {w}, 0({p})",
            "sd {w}, 8({p})",
            "addi {p}, {p}, 16",
            w = in(reg) word,
            p = inout(reg) p,
            options(nostack),
        );
    }
    while end.saturating_sub(p) >= 8 {
        asm!(
            "sd {w}, 0({p})",
            "addi {p}, {p}, 8",
            w = in(reg) word,
            p = inout(reg) p,
            options(nostack),
        );
    }
    while p < end {
        (p as *mut u8).write_volatile(v as u8);
        p += 1;
    }
    s
}

use core::arch::{asm, global_asm};
use core::fmt::Write;
use core::panic::PanicInfo;
use core::ptr::{addr_of, read_volatile, write_volatile};

mod allocator;
mod anim;
mod audio;
mod bionic_shim;
mod blkdev;
mod compositor;
mod devfs;
mod modem;
mod elf;
mod fdt;
mod fat32;
mod fs_simple;
mod fsops;
mod plic;
mod sbi;
mod gpu_simple;
mod gesture;
mod heap;
mod health;
mod input;

mod interrupt;
mod ipc;
mod linux_compat;
mod mem_pressure;
mod mmu;
mod net_stack;
mod net_virtio;
mod ota;
mod pci;
mod ac97;
mod pkg;
mod power;
mod gdbstub;
mod process;
mod procfs;
mod tmpfs;
mod signal;
mod rtc;
mod sched_cfs;
mod sensor_dev;
mod smp;
mod shell;
mod socket;
mod syscall_table;
mod thread;
mod timer;
mod tty;
mod ui_framework;
mod vfs;
mod virtio_blk;
mod virtio_input;
mod virtio_rng;
mod virtio_snd;
mod vm;
mod wifi;
// ===== 开发者工具链模块（日志 / 崩溃报告 / 性能统计） =====
mod crash;
mod crash_screen;
mod log;
mod perf;

// ==================== 启动信息 ====================

/// QEMU 通过 x0 寄存器传入的设备树（DTB）地址，由启动汇编保存于此。
#[no_mangle]
#[used]
static mut DTB_ADDR: usize = 0;
// [诊断] sync_entry 入口保存的故障点寄存器 x0-x30（x11 槽=基址标记）
#[no_mangle]
static mut PANIC_REGS: [u64; 31] = [0; 31];

// ==================== 链接脚本符号 ====================

extern "C" {
    /// 内核镜像末尾（`_kernel_end`，4KB 对齐）
    static _kernel_end: u8;
}

/// 返回内核镜像末尾物理地址（供页分配器标记内核区域已用）
pub fn kernel_end() -> usize {
    addr_of!(_kernel_end) as usize
}

// ==================== NS16550 UART 寄存器 ====================

/// NS16550 寄存器布局（偏移以字节为单位；DLAB=0 视图）。
///
/// QEMU virt RISC-V 的串口是 ns16550a（DTB /soc/serial@10000000，IRQ 10），
/// 寄存器宽度 1 字节（reg-space 0x100，按字节访问）。
#[repr(C, align(4))]
struct Ns16550 {
    thr_rbr: u8,    // 0x00 发送保持（写）/ 接收缓冲（读）
    ier: u8,        // 0x01 中断使能（DLAB=0）
    iir_fcr: u8,    // 0x02 中断识别（读）/ FIFO 控制（写）
    lcr: u8,        // 0x03 行控制（含 DLAB）
    mcr: u8,        // 0x04 Modem 控制
    lsr: u8,        // 0x05 线状态（THRE / DR）
    _msr: u8,       // 0x06 Modem 状态
    _scr: u8,       // 0x07 暂存
    _reserved: [u8; 0x100 - 8],
}

/// UART MMIO 基地址（由设备树解析得到，默认 QEMU virt RISC-V 值）
static mut UART_BASE: usize = 0x1000_0000;

/// 获取 UART 设备引用
///
/// # 安全
/// `UART_BASE` 指向的 MMIO 地址处始终存在一个 ns16550a 设备。
fn uart() -> &'static mut Ns16550 {
    unsafe { &mut *(UART_BASE as *mut Ns16550) }
}

/// 线状态寄存器位定义
const LSR_THRE: u8 = 1 << 5; // 发送保持寄存器空（可写下一字节）
const LSR_DR: u8 = 1 << 0; // 接收数据就绪（可读）

/// 行控制：8N1 + DLAB（访问除数锁存）
const LCR_8N1: u8 = 0b0000_0011;
const LCR_DLAB: u8 = 1 << 7;

/// FIFO 控制：FIFO 使能 + 清空 RX/TX + RX 触发深度 14 字节（0xC7）
const FCR_ENABLE: u8 = 0xC7;

/// 中断使能：接收数据可用（IER bit0）；16550 的接收超时随 FIFO 使能生效
const IER_RX: u8 = 0b0000_0001;

/// 初始化 ns16550：`base` 为 MMIO 基地址，`clock` 为 UART 时钟（Hz）。
/// 波特率 115200 8N1，使能 FIFO，使能接收中断。
fn uart_init(base: usize, clock: u32) {
    unsafe {
        UART_BASE = base;
    }
    let uart = uart();
    unsafe {
        // 关中断，才能安全修改寄存器
        write_volatile(&mut uart.ier, 0);
        // DLAB=1 设置除数（divisor = clock / (16*115200)，至少 1）
        write_volatile(&mut uart.lcr, LCR_DLAB);
        let div = (clock / (16 * 115200)).clamp(1, 255) as u8;
        write_volatile(&mut uart.thr_rbr, div); // DLL（除数低字节）
        write_volatile(&mut uart.ier, 0); // DLM（除数高字节，时钟 < 470MHz 时恒 0）
        // 8N1（DLAB=0）
        write_volatile(&mut uart.lcr, LCR_8N1);
        // 使能并复位 FIFO（QEMU 16550 忽略触发深度差异，行为兼容）
        write_volatile(&mut uart.iir_fcr, FCR_ENABLE);
        // 使能接收中断（RX 数据可用 + FIFO 模式接收超时）
        write_volatile(&mut uart.ier, IER_RX);
    }
}

/// 阻塞发送单个字符
pub(crate) fn uart_send_char(c: u8) {
    kmsg_put(c);
    let uart = uart();
    // 等待发送保持寄存器空（THRE 置位）
    while unsafe { read_volatile(&uart.lsr) } & LSR_THRE == 0 {
        core::hint::spin_loop();
    }
    unsafe {
        write_volatile(&mut uart.thr_rbr, c);
    }
    // TTY/PTY 屏幕镜像：把串口输出同步到活动会话的 out_q（图形终端主侧读取）。
    // 未附接时是零开销布尔判断；附接后 GUI 终端因此能看到 Shell 提示符、回显
    // 与所有命令输出（包含 ANSI 光标/颜色序列）。
    crate::tty::echo_mirror(c);
}

/// UART 接收环形缓冲（中断处理例程写入，shell / /dev/tty 读出）。
/// 4096 字节：足以容纳一次粘贴的大段命令而不会在 shell 忙于执行时溢出丢弃。
const UART_RX_BUF_SIZE: usize = 4096;
static mut UART_RX_BUF: [u8; UART_RX_BUF_SIZE] = [0; UART_RX_BUF_SIZE];
static mut UART_RX_HEAD: usize = 0;
static mut UART_RX_TAIL: usize = 0;
static mut UART_RX_COUNT: usize = 0;
/// 缓冲满时被丢弃的字节数（溢出计数，供消费方提示用户）。
static mut UART_RX_DROPPED: usize = 0;

/// bootargs 是否含 "shell"（QEMU `-append "shell"` → /chosen/bootargs）。
/// 置位时 GUI 启动同时拉起内置 shell，供 UART 驱动测试。
static mut BOOT_SHELL: bool = false;

/// bootargs 是否含 "recovery"（bootloader 注入）：恢复模式启动。
static mut BOOT_RECOVERY: bool = false;

/// 安全启动链属性（bootloader 经 /chosen 注入）：
/// kunx,build-version / kunx,ramdisk-sha256 / kunx,recovery-ramdisk。
static mut SEC_BUILD_VERSION: u32 = 0;
static mut SEC_RAMDISK_SHA: [u8; 32] = [0xEE; 32]; // 0xEE.. = 属性缺失
static mut SEC_RECOVERY_RAMDISK: (usize, usize) = (0, 0);

/// 读取 bootargs shell 标志（供 init_main 决定是否并起 shell）。
pub(crate) fn bootargs_shell() -> bool {
    unsafe { BOOT_SHELL }
}

/// 读取 bootargs recovery 标志。
pub(crate) fn bootargs_recovery() -> bool {
    unsafe { BOOT_RECOVERY }
}

/// bootargs 是否含 "smp"：显式激活次核（hart_start）。次核调度路径的
/// SMP 正确性（锁交接/跨核切换）尚未硬化，默认关闭保持单核稳定；
/// 互监看门狗只在多核会话生效，用 `-append "smp"` 测试。
static mut BOOT_SMP: bool = false;

pub(crate) fn bootargs_smp() -> bool {
    unsafe { BOOT_SMP }
}

/// 读取安全启动链属性（内核 ota 模块 / 恢复挂载用）。
pub(crate) fn sec_build_version() -> u32 {
    unsafe { SEC_BUILD_VERSION }
}
pub(crate) fn sec_ramdisk_sha() -> &'static [u8; 32] {
    unsafe { &SEC_RAMDISK_SHA }
}
pub(crate) fn sec_recovery_ramdisk() -> (usize, usize) {
    unsafe { SEC_RECOVERY_RAMDISK }
}

/// bootargs 是否含 "wifitest"（无头 Wi-Fi 自检：扫描 → WPA2-PSK 4-way → 已连接）。
static mut BOOT_WIFITEST: bool = false;
pub(crate) fn bootargs_wifitest() -> bool {
    unsafe { BOOT_WIFITEST }
}

/// bootargs 是否含 "perftest"：启动期动画管线性能基线（anim::perf_test）。
/// 默认不执行——该测试每次启动固定消耗约 1.5s 来宾时间（1s 120Hz 看门狗 + A/B/C
/// 约 300 帧 composite/present 基准），属于开发期测量工具。需要时经
/// QEMU `-append "perftest"` 触发，运行时仍可用 shell `perf on|stats` 观察。
static mut BOOT_PERFTEST: bool = false;
pub(crate) fn bootargs_perftest() -> bool {
    unsafe { BOOT_PERFTEST }
}

/// bootargs 是否含 "selftest"：启动期内核自检（堆分配器 + 动画曲线 + 符号链接 +
/// 电源状态机）。默认不执行——这些是开发/CI 诊断，每次开机固定跑会拖慢启动并
/// 刷屏大量 [test]/[symlink]/[power-selftest] 日志。需要时经
/// QEMU `-append "selftest"` 触发；运行时仍可用 shell `power selftest` 单独执行。
static mut BOOT_SELFTEST: bool = false;
pub(crate) fn bootargs_selftest() -> bool {
    unsafe { BOOT_SELFTEST }
}

// ==================== 控制台安静模式（简洁 CLI） ====================

/// 控制台安静模式（默认开启）：内核启动期与运行期的信息性日志（[hal]/[FDT]/
/// [fs]/[gpu]/[net]/[diag]/[perf]/[pinit] 等）不再输出到串口，命令行界面只保留
/// 错误、崩溃报告与交互输出。恢复完整日志的途径：
/// - 启动参数 `-append "verbose"`；
/// - 运行时 shell `verbose on|off`。
/// 结构化日志（printk!）不受影响：始终写入环形缓冲，shell `log` 可查看。
pub(crate) static CONSOLE_QUIET: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(true);

/// 当前是否处于安静模式（信息日志不上串口）。
pub(crate) fn console_quiet() -> bool {
    CONSOLE_QUIET.load(core::sync::atomic::Ordering::Relaxed)
}

/// 切换安静模式（bootargs "verbose" / shell `verbose` 命令）。
pub(crate) fn console_set_quiet(quiet: bool) {
    CONSOLE_QUIET.store(quiet, core::sync::atomic::Ordering::Relaxed);
}

/// 启动期信息打印（整段输出共用一次开关判断）：安静模式下不上串口。
/// 仅用于信息性日志；错误 / 崩溃 / 自检 PASS-FAIL 等必须可见的输出不要包进来。
#[macro_export]
macro_rules! kinfo {
    ($($arg:tt)*) => {
        if !crate::console_quiet() { $($arg)* }
    };
}

// ==================== 内核消息环形缓冲（dmesg） ====================

/// 内核日志环形缓冲：捕获所有 uart_send_char 输出（含启动日志），供
/// shell `dmesg` 命令与后续 /dev/kmsg 访问。16KB 覆盖完整启动流程。
const KMSG_BUF_SIZE: usize = 16384;
static mut KMSG_BUF: [u8; KMSG_BUF_SIZE] = [0; KMSG_BUF_SIZE];
static mut KMSG_HEAD: usize = 0;
static mut KMSG_LEN: usize = 0;

/// 把字符写入内核消息缓冲（环形覆盖最旧内容）。调用方无需关心 IRQ 状态。
pub fn kmsg_put(c: u8) {
    // 保存/恢复 DAIF：可从 IRQ 处理、系统调用、panic 等任意上下文调用
    let saved: u64;
    unsafe { asm!("csrr {}, sstatus", out(reg) saved) };
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let i = (KMSG_HEAD + KMSG_LEN) % KMSG_BUF_SIZE;
        KMSG_BUF[i] = c;
        if KMSG_LEN == KMSG_BUF_SIZE {
            KMSG_HEAD = (KMSG_HEAD + 1) % KMSG_BUF_SIZE;
        } else {
            KMSG_LEN += 1;
        }
    }
    unsafe {
        asm!(
            "csrw sstatus, {}",
            in(reg) saved,
        );
    }
}

/// 把内核消息缓冲内容（按时间顺序）拷贝到 `out`，返回实际字节数。
pub fn kmsg_snapshot(out: &mut [u8]) -> usize {
    unsafe {
        let n = KMSG_LEN.min(out.len());
        for i in 0..n {
            out[i] = KMSG_BUF[(KMSG_HEAD + i) % KMSG_BUF_SIZE];
        }
        n
    }
}

// ==================== UART 驱动（初始化/发送见上方 Ns16550 节） ====================

/// 诊断打印（冷路径）：输出标记 + 两个十六进制值。
pub(crate) fn log_dbg(tag: &str, a: u64, b: u64) {
    uart_send_str(tag);
    let mut buf = [0u8; 16];
    for i in 0..16 {
        let nib = ((a >> (60 - i * 4)) & 0xF) as u8;
        buf[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
    }
    uart_send_str(core::str::from_utf8(&buf).unwrap_or(""));
    uart_send_str(" nr=");
    for i in 0..4 {
        let nib = ((b >> (28 - i * 4)) & 0xF) as u8;
        buf[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
    }
    uart_send_str(core::str::from_utf8(&buf[..4]).unwrap_or(""));
    uart_send_str("\n");
}

/// 发送字符串（把 \n 转换为 \r\n，保证终端正确换行）
pub(crate) fn uart_send_str(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            uart_send_char(b'\r');
        }
        uart_send_char(b);
    }
}

/// 直接从 ns16550 硬件接收 FIFO 读一个字节（绕过 IRQ/软件缓冲）。
/// 供 GDB 调试桩在屏蔽 IRQ 期间使用（此时 RX 中断不触发，软件缓冲不再增长）。
pub(crate) fn uart_read_hw() -> Option<u8> {
    let uart = uart();
    if unsafe { read_volatile(&uart.lsr) } & LSR_DR == 0 {
        return None; // FIFO 空
    }
    Some(unsafe { read_volatile(&uart.thr_rbr) })
}

/// 把 ns16550 硬件接收 FIFO 全部排空到软件缓冲（供 GDB 调试桩使用：
/// 断点期间 IRQ 被屏蔽，RX 中断不触发，必须主动轮询排空 16 字节硬件
/// FIFO，否则长报文在 FIFO 溢出时丢字节 → 校验和错误）。
pub(crate) fn uart_drain_hw() {
    let uart = uart();
    while unsafe { read_volatile(&uart.lsr) } & LSR_DR != 0 {
        let c = unsafe { read_volatile(&uart.thr_rbr) };
        unsafe {
            if UART_RX_COUNT < UART_RX_BUF_SIZE {
                UART_RX_BUF[UART_RX_TAIL] = c;
                UART_RX_TAIL = (UART_RX_TAIL + 1) % UART_RX_BUF_SIZE;
                UART_RX_COUNT += 1;
            } else {
                UART_RX_DROPPED = UART_RX_DROPPED.saturating_add(1);
            }
        }
    }
}

/// 非阻塞读取 UART 接收缓冲中的一个字节，**不触碰 DAIF**。
/// 供 GDB 调试桩使用：gdb 循环全程屏蔽 IRQ，普通版本内部的 daifclr
/// 会重开中断 → 定时器抢占 → 任务切换污染 gdb 会话（串口被其它输出打断）。
pub(crate) fn uart_try_read_char_masked() -> Option<u8> {
    unsafe {
        if UART_RX_COUNT == 0 {
            None
        } else {
            let b = UART_RX_BUF[UART_RX_HEAD];
            UART_RX_HEAD = (UART_RX_HEAD + 1) % UART_RX_BUF_SIZE;
            UART_RX_COUNT -= 1;
            Some(b)
        }
    }
}

/// UART 接收缓冲是否有待读字节（供 pselect6 就绪检测，非破坏性）。
pub(crate) fn uart_rx_pending() -> bool {
    unsafe { UART_RX_COUNT > 0 }
}

/// 非阻塞读取 UART 接收缓冲中的一个字节（供 /dev/tty 与 shell 使用）。
///
/// 返回 `None` 表示当前无待读字节。
pub(crate) fn uart_try_read_char() -> Option<u8> {
    unsafe { asm!("csrci sstatus, 2") };
    let c = unsafe {
        if UART_RX_COUNT == 0 {
            None
        } else {
            let b = UART_RX_BUF[UART_RX_HEAD];
            UART_RX_HEAD = (UART_RX_HEAD + 1) % UART_RX_BUF_SIZE;
            UART_RX_COUNT -= 1;
            Some(b)
        }
    };
    unsafe { asm!("csrsi sstatus, 2") };
    c
}

/// 读取并清零 UART 溢出丢弃计数（供 shell 提示用户输入被丢弃）。
pub(crate) fn uart_take_dropped() -> usize {
    unsafe { asm!("csrci sstatus, 2") };
    let d = unsafe { UART_RX_DROPPED };
    unsafe {
        UART_RX_DROPPED = 0;
    }
    unsafe { asm!("csrsi sstatus, 2") };
    d
}

// ==================== 数字打印 ====================

/// 打印无符号 64 位整数（十六进制，带 0x 前缀，固定 16 位）
pub(crate) fn print_hex(v: u64) {
    uart_send_str("0x");
    for i in (0..16).rev() {
        let nibble = ((v >> (i * 4)) & 0xF) as u8;
        let c = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
        uart_send_char(c);
    }
}

/// 打印无符号 64 位整数（十进制）
pub(crate) fn print_dec(mut v: u64) {
    if v == 0 {
        uart_send_char(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for &c in &buf[i..] {
        uart_send_char(c);
    }
}

// ==================== 系统寄存器访问 ====================

/// 置 sstatus.SIE，开启 IRQ
///
/// # 安全
/// 调用者必须已正确初始化 stvec 与 PLIC，否则开启 IRQ 会导致未定义行为。
pub(crate) unsafe fn enable_irq() {
    // 分源使能：sie.SSIE(bit1) | STIE(bit5) | SEIE(bit9) = 0x222。
    // sstatus.SIE 只是总开关——此前漏置 sie.SEIE，UART RX / virtio 等一切
    // PLIC 源的中断从不陷入（定时器经 SBI stimecmp 侥幸可用，掩盖了缺陷）。
    // 注意 STIE 是 bit5（0x20），不是 bit6（0x40 = VS 定时器）。
    asm!("csrsi sie, 0x222");
    asm!("csrsi sstatus, 2");
}

/// 等待中断
///
/// # 安全
/// wfi 是架构指令，任意时刻可执行。
pub(crate) unsafe fn wfi() {
    asm!("wfi");
}

/// 读取 scause（异常原因寄存器）
///
/// # 安全
/// 需要 S 模式权限。
unsafe fn read_scause() -> u64 {
    let v: u64;
    asm!("csrr {}, scause", out(reg) v);
    v
}

/// 读取 stval（异常有效值寄存器：故障地址 / 指令编码）
///
/// # 安全
/// 需要 S 模式权限。
unsafe fn read_stval() -> u64 {
    let v: u64;
    asm!("csrr {}, stval", out(reg) v);
    v
}

/// 读取 sepc（异常程序计数器，保存故障指令地址）
///
/// # 安全
/// 需要 S 模式权限。
unsafe fn read_sepc() -> u64 {
    let v: u64;
    asm!("csrr {}, sepc", out(reg) v);
    v
}

// ==================== UART 中断处理 ====================

/// UART 接收中断：读取所有可用字符并送入接收环形缓冲（不在此回显，
/// 回显由消费方如 shell 的行编辑完成）。
fn uart_handle_rx() {
    // 16550：读 RBR 即清接收数据中断；FIFO 模式下排空后接收超时中断自清
    unsafe { uart_rx_drain() };
}

/// 排空 UART 接收 FIFO 并按正常路径分发（输入子系统优先，其余入 tty 环形缓冲）。
///
/// 供两处调用：RX 中断处理本体，以及 IRQ 关闭的长 TX 路径（风暴转储等）
/// 的间隙轮询 —— PL011 RX FIFO 仅 16 字节（115200 下 ~1.4ms 填满），转储
/// 忙等超过该窗口会静默丢输入字节（终端粘贴/自动化测试注入被截断）。
pub(crate) unsafe fn uart_rx_drain() {
    let uart = uart();
    // 只要接收 FIFO 有数据（DR=1）就读取
    while read_volatile(&uart.lsr) & LSR_DR != 0 {
        let c = read_volatile(&uart.thr_rbr);
        // 触摸协议帧优先：被输入子系统消费的字节不再进入 tty/shell 缓冲
        if input::feed_uart_byte(c) {
            continue;
        }
        if UART_RX_COUNT < UART_RX_BUF_SIZE {
            UART_RX_BUF[UART_RX_TAIL] = c;
            UART_RX_TAIL = (UART_RX_TAIL + 1) % UART_RX_BUF_SIZE;
            UART_RX_COUNT += 1;
        } else {
            UART_RX_DROPPED = UART_RX_DROPPED.saturating_add(1);
        }
    }
}

// ==================== 内核主函数 ====================

/// 内核主入口，由启动汇编调用
#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    // ===== 1. 获取硬件平台（HAL：编译期 feature 选择 QEMU 或 Raspberry Pi 4） =====
    let platform = kunxhal::current_platform();

    // ===== 1.1 解析设备树（QEMU 通过 x0 传入 DTB 地址），缺失时回退平台默认值 =====
    let fdt = unsafe { fdt::Fdt::parse(DTB_ADDR) };

    // 设备参数默认值由平台后端提供
    let mut uart_base = platform.uart_base();
    let mut uart_irq = platform.uart_irq();
    let mut uart_clk = platform.uart_clock();
    let mut plic_base = platform.gicd_base(); // HAL 语义复用：gicd_base 返回中断控制器基址（RISC-V = PLIC）
    let mut mem_base = platform.ram_base();
    let mut mem_size = platform.ram_size();
    // fw_cfg 为 QEMU 特有设备（Pi 4 无此设备），保留默认值，由设备树覆盖
    let mut fwcfg_base: usize = 0x1010_0000;
    let mut stdout_path: Option<&str> = None;
    let mut boot_shell = false; // bootargs 含 "shell"：GUI 启动同时拉起内置 shell（QEMU 测试用）
    let mut boot_wifitest = false; // bootargs 含 "wifitest"：无头 Wi-Fi 自检
    let mut boot_perftest = false; // bootargs 含 "perftest"：启动期动画性能基线（默认跳过）
    let mut boot_selftest = false; // bootargs 含 "selftest"：启动期内核自检（默认跳过）
    let mut boot_verbose = false; // bootargs 含 "verbose"：完整启动日志（默认安静控制台）
    let mut boot_smp = false; // bootargs 含 "smp"：显式激活次核（SMP 调度未硬化，默认关）
    let mut virtio_devs: [net_virtio::MmioDev; 32] = [net_virtio::MmioDev::empty(); 32];
    let mut virtio_count = 0usize;

    if let Some(fdt) = fdt {
        // /chosen
        if let Some(chosen) = fdt.node("/chosen") {
            stdout_path = chosen.prop_str(&fdt, "stdout-path");
            // QEMU `-append "shell"` 会写入 /chosen/bootargs；含 "shell" 时
            // 在 /init 之外再拉起内置交互 shell（供 UART 驱动测试）。
            if let Some(args) = chosen.prop_str(&fdt, "bootargs") {
                boot_smp = args.split_whitespace().any(|w| w == "smp");
                boot_shell = args.split_whitespace().any(|w| w == "shell");
                boot_wifitest = args.split_whitespace().any(|w| w == "wifitest");
                boot_perftest = args.split_whitespace().any(|w| w == "perftest");
                boot_selftest = args.split_whitespace().any(|w| w == "selftest");
                boot_verbose = args.split_whitespace().any(|w| w == "verbose");
                // 恢复模式（bootloader 注入 "recovery"）
                if ota::has_recovery_arg(args) {
                    unsafe { BOOT_RECOVERY = true; }
                }
            }
            // 安全启动链属性
            if let Some(bv) = chosen.prop_u32(&fdt, "kunx,build-version", 0) {
                unsafe { SEC_BUILD_VERSION = bv; }
            }
            if let Some(raw) = chosen.prop(&fdt, "kunx,ramdisk-sha256") {
                if raw.value.len() == 32 {
                    unsafe {
                        SEC_RAMDISK_SHA.copy_from_slice(&raw.value);
                    }
                }
            }
            if let Some(addr) = chosen.prop_u32(&fdt, "kunx,recovery-ramdisk", 0) {
                if let Some(size) = chosen.prop_u32(&fdt, "kunx,recovery-ramdisk", 1) {
                    unsafe { SEC_RECOVERY_RAMDISK = (addr as usize, size as usize); }
                }
            }
        }
        // 记录 bootargs 标志供 init_main 使用（静态，跨线程）。
        unsafe {
            BOOT_SHELL = boot_shell;
            BOOT_WIFITEST = boot_wifitest;
            BOOT_PERFTEST = boot_perftest;
            BOOT_SELFTEST = boot_selftest;
            BOOT_SMP = boot_smp;
        }
        // verbose bootargs：关闭控制台安静模式（此时尚未开始打印，完整日志可见）。
        if boot_verbose {
            console_set_quiet(false);
        }
        // /memory（QEMU 用 device_type = "memory" 标识）
        if let Some(mem) = fdt.node("/memory").or_else(|| fdt.find_property("device_type", "memory"))
        {
            if let Some((base, size)) = mem.reg(&fdt, 0) {
                mem_base = base as usize;
                mem_size = size as usize;
            }
        }
        // UART：优先 stdout-path 指定的节点（QEMU virt RISC-V 为
        // /soc/serial@10000000，ns16550a），失败按 compatible 查找
        let uart = if let Some(sp) = stdout_path {
            fdt.node(sp).or_else(|| fdt.find_compatible("ns16550a"))
        } else {
            fdt.node("/soc/serial").or_else(|| fdt.find_compatible("ns16550a"))
        };
        if let Some(uart) = uart {
            if let Some((base, _)) = uart.reg(&fdt, 0) {
                uart_base = base as usize;
            }
            if let Some((t, n, _)) = uart.interrupt(&fdt, 0) {
                uart_irq = fdt::gic_irq(t, n);
            }
        }
        // UART 时钟：fixed-clock 节点；缺失时保持平台默认（QEMU 16550
        // 不校验波特率分频，时钟精度不影响可用性）
        if let Some(clk) = fdt.find_compatible("fixed-clock") {
            if let Some(freq) = clk.prop_u32(&fdt, "clock-frequency", 0) {
                uart_clk = freq;
            }
        }
        // PLIC：按 compatible 查找（RISC-V DTB 无 GIC 式多 reg 语义，单基址）
        if let Some(plic) = fdt
            .find_compatible("riscv,plic0")
            .or_else(|| fdt.find_compatible("sifive,plic-1.0.0"))
        {
            if let Some((base, _)) = plic.reg(&fdt, 0) {
                plic_base = base as usize;
            }
        }
        // timebase：/cpus/timebase-frequency（QEMU virt RISC-V = 10MHz）
        if let Some(cpus) = fdt.node("/cpus") {
            if let Some(hz) = cpus.prop_u32(&fdt, "timebase-frequency", 0) {
                if hz > 0 {
                    timer::set_timebase(hz as u64);
                }
            }
        }
        // fw_cfg（ramfb 帧缓冲配置通道）
        if let Some(fw) = fdt.find_compatible("qemu,fw-cfg-mmio") {
            if let Some((base, _)) = fw.reg(&fdt, 0) {
                fwcfg_base = base as usize;
            }
        }
        // virtio-mmio 设备（Phase 7 网卡等）
        let mut vnodes: [fdt::Node; 32] = [fdt::Node::empty(); 32];
        let n = fdt.find_all_compatible("virtio,mmio", &mut vnodes);
        for i in 0..n {
            if let Some((base, _)) = vnodes[i].reg(&fdt, 0) {
                let irq = vnodes[i]
                    .interrupt(&fdt, 0)
                    .map(|(t, num, _)| fdt::gic_irq(t, num))
                    .unwrap_or(0);
                if virtio_count < virtio_devs.len() {
                    virtio_devs[virtio_count] = net_virtio::MmioDev { base: base as usize, irq };
                    virtio_count += 1;
                }
            }
        }
    }

    // ===== 2. 用设备树信息初始化串口 =====
    uart_init(uart_base, uart_clk);
    // 电源控制钩子：HAL 平台后端经此取得 SBI 关机/重启实现（PSCI 的 RISC-V 等价物）
    kunxhal::qemu_power_set_hooks(sbi::shutdown, sbi::system_reset);
    uart_send_str("Hello from Rust kernel!\n");

    // ===== 2.05 SMP 多核：唤醒其余核心（QEMU PSCI） =====
    smp::smp_init();

    // ===== 2.1 打印平台信息（HAL） =====
    // 注意：display_init/net_init 有初始化副作用，必须在门控外调用。
    let hal_display = platform.display_init();
    let hal_net = platform.net_init();
    kinfo! {
        uart_send_str("[hal] platform=");
        uart_send_str(platform.name());
        uart_send_str("\n");
        uart_send_str("[hal] uart=");
        print_hex(platform.uart_base() as u64);
        uart_send_str(" irq=");
        print_dec(platform.uart_irq() as u64);
        uart_send_str("\n");
        uart_send_str("[hal] ram=");
        print_hex(platform.ram_base() as u64);
        uart_send_str(" size=");
        print_dec((platform.ram_size() / (1024 * 1024)) as u64);
        uart_send_str("MB\n");
        uart_send_str("[hal] gic gicd=");
        print_hex(platform.gicd_base() as u64);
        uart_send_str(" gicc=");
        print_hex(platform.gicc_base() as u64);
        uart_send_str("\n");
        uart_send_str("[hal] timer=");
        print_dec(platform.timer_freq());
        uart_send_str("Hz\n");
        uart_send_str("[hal] display=");
        match hal_display {
            Some(d) => {
                uart_send_str("显示 ");
                print_dec(d.width as u64);
                uart_send_str("x");
                print_dec(d.height as u64);
            }
            None => uart_send_str("无"),
        }
        uart_send_str("\n");
        uart_send_str("[hal] net=");
        match hal_net {
            Some(n) => {
                uart_send_str("virtio-net ");
                for i in 0..6 {
                    if i > 0 {
                        uart_send_char(b':');
                    }
                    print_hex_byte(n.mac[i]);
                }
            }
            None => uart_send_str("无"),
        }
        uart_send_str("\n");
    }

    // ===== 3. 打印设备树信息 =====
    kinfo! {
        uart_send_str("[FDT] 设备树 @ ");
        print_hex(unsafe { DTB_ADDR } as u64);
        uart_send_str("\n");
        uart_send_str("[FDT] /chosen stdout-path = ");
        match stdout_path {
            Some(p) => uart_send_str(p),
            None => uart_send_str("(无)"),
        }
        uart_send_str("\n");
        uart_send_str("[FDT] /memory base=");
        print_hex(mem_base as u64);
        uart_send_str(" size=");
        print_hex(mem_size as u64);
        uart_send_str(" (");
        print_dec((mem_size / (1024 * 1024)) as u64);
        uart_send_str(" MB)\n");
        uart_send_str("[FDT] uart base=");
        print_hex(uart_base as u64);
        uart_send_str(" irq=");
        print_dec(uart_irq as u64);
        uart_send_str(" clk=");
        print_dec(uart_clk as u64);
        uart_send_str("\n");
        uart_send_str("[FDT] plic base=");
        print_hex(plic_base as u64);
        uart_send_str("\n");
    }

    // ===== 4. 初始化物理页分配器 =====
    // 按 DTB 解析出的真实内存大小初始化分配器（QEMU 512MB），防超量分配
    allocator::init(kernel_end(), mem_size);

    // ===== 5. 初始化 MMU（恒等映射）并启用 =====
    mmu::init();
    // 信号蹦床页（所有用户空间共享的只读 sigreturn 存根）
    signal::trampoline_init();
    kinfo! {
        uart_send_str("MMU 已启用。\n");
        // PAN（Privileged Access Never）内存硬化：v8.1+ 核启用，v8.0 核跳过
        if mmu::pan_supported() {
            uart_send_str("[hardening] PAN 已启用（EL1 访问用户内存受限）\n");
        } else {
            uart_send_str("[hardening] 本 CPU 不支持 PAN（ARMv8.0），跳过\n");
        }
    }

    // ===== 5.5 启动期自检（bootargs 含 "selftest" 才执行；默认跳过） =====
    // 开发诊断：kmalloc/kfree 泄漏 + anim 曲线 + map/unmap + 符号链接 + 电源状态机。
    // 每次开机固定跑会拖慢启动并刷屏 [test]/[symlink]/[power-selftest] 日志，
    // 故与 perftest/wifitest 一样改由 QEMU `-append "selftest"` 触发。
    if crate::bootargs_selftest() {
        self_test();
    }

    // ===== 5.6 初始化文件系统（Phase 5：块设备 + VFS + 简单文件系统 + 设备文件） =====
    fs_boot();

    // ===== 5.65 持久化 /data：探测 virtio-blk、解析 MBR、挂载 data 分区 =====
    data_boot(&virtio_devs[..virtio_count]);

    // ===== 5.655 熵源：virtio-rng → /dev/urandom（无设备时跳过） =====
    virtio_rng::rng_boot(&virtio_devs[..virtio_count]);

    // ===== 5.66 procfs：/proc 只读虚拟文件（version/meminfo/uptime/cpuinfo/processes）=====
    procfs::init();

    // ===== 5.67 tmpfs：内存文件系统挂载 /tmp =====
    tmpfs::init();

    // ===== 5.675 符号链接自检（bootargs 含 "selftest" 才执行） =====
    if crate::bootargs_selftest() {
        symlink_selftest();
    }

    // ===== 5.68 音频子系统：virtio-sound（首选）→ AC97（遗留回退） =====
    // 新音频栈：/dev/audio + 软件环形缓冲 + IRQ 驱动补充（audio::init）。
    // virtio-sound 不可用时回退到遗留 AC97 轮询音调路径（仅启动提示音，
    // 不提供 /dev/audio 流式接口）。
    let audio_ready = audio::init(&virtio_devs[..virtio_count]);
    let mut ac97_tone_ok = false;
    if !audio_ready {
        if let Some(fdt) = fdt {
            if ac97::init(&fdt) {
                kinfo! { uart_send_str("[audio] AC97 回退已启用\n"); }
                // 启动提示音有副作用（AC97 播放），在门控外执行
                ac97_tone_ok = ac97::play_tone(880, 120) == 0;
            }
        }
    }
    kinfo! {
        if audio_ready {
            uart_send_str("[audio] 音频子系统就绪（virtio-sound 后端）\n");
        } else if ac97_tone_ok {
            uart_send_str("[audio] 启动提示音正常（AC97）\n");
        } else {
            uart_send_str("[audio] 无音频设备\n");
        }
    }

    // ===== 5.7 输入子系统（/dev/input/event0 + UART 触摸协议帧解析） =====
    input::init(platform.input_init());

    // ===== 5.75 传感器子系统（/dev/sensor0：IMU 原始样本） =====
    // QEMU：软件模拟 IMU（确定性剧本：平放 → 抬腕 → 航向扫描 → 行走计步）；
    // Pi4：BSC1 I2C + MPU6050/9250（探测失败回退模拟器）。
    sensor_dev::init(platform.sensor_init());

    // ===== 5.71 调制解调器子系统（/dev/modem + /dev/modemctl） =====
    // QEMU：内置虚拟调制解调器（模拟 AT 响应，支持来电/短信事件注入）；
    // Pi4：第二路 PL011（UART2）供外接 AT 调制解调器（如 Quectel EC25）。
    modem::init();

    // ===== 5.8 初始化图形（Phase 6：帧缓冲 + 合成器 + 输入 + UI） =====
    gfx_boot(fwcfg_base);

    // ===== 6. 初始化中断控制器与定时器 =====
    plic::plic_init(plic_base);
    let virtio_kbd = virtio_input::init(&virtio_devs[..virtio_count]);
    kinfo! {
        if virtio_kbd {
            uart_send_str("[input] virtio 键盘就绪\n");
        } else {
            uart_send_str("[input] 无 virtio 键盘（仅触摸）\n");
        }
    }
    timer::timer_init();
    timer::timer_set_interval(10_000); // 10ms 调度周期（CNTV，动态调整）
    // RTC：读取启动时刻的墙钟纪元秒（供 gettimeofday/date 使用）
    rtc::init(platform.rtc_base());
    // 调度 tick 只负责睡眠器唤醒与 CFS 切换
    timer::timer_register_handler(timer_rearm);
    timer::timer_register_handler(sched_cfs::tick);
    // 动画帧推进走独立硬件通道（CNTP 物理定时器，固定 8.33ms = 120Hz），
    // 与调度 tick 完全解耦：帧间隔不再被调度器上下文切换抖动污染；
    // 全屏手势动画依赖此通道驱动 anim::frame_tick（周期与文档一致：120Hz）
    timer::timer2_set_interval(crate::power::anim_frame_interval_us());
    timer::timer2_register_handler(anim::frame_tick);

    // ===== 7. 注册中断处理函数 =====
    interrupt::register_handler(timer::TIMER_IRQ, timer::timer_handle_irq);
    interrupt::register_handler(timer::TIMER2_IRQ, timer::timer2_handle_irq);
    interrupt::register_handler(uart_irq, uart_handle_rx);

    // ===== 8. 配置并使能 PLIC 中断 =====
    // 监督定时器（调度 tick + 动画帧）不走 PLIC（scause=5），由 SBI 编程；
    // 此处只使能外部设备中断（UART 等）。virtio 各设备 IRQ 由各自驱动
    // 探测时使能。
    plic::plic_enable_irq(uart_irq);
    plic::plic_set_priority(uart_irq, 0xA0);

    // ===== 8.5 启动期动画管线性能测试（仅 bootargs 含 "perftest" 时执行） =====
    // 默认跳过：该测试固定消耗约 1.5s 来宾时间（1s 120Hz 看门狗 + A/B/C 约 300 帧
    // composite/present 基准），拖慢每次启动。需要测量/回归时用
    // QEMU `-append "perftest"` 触发；运行时观察用 shell `perf on|stats`。
    if crate::bootargs_perftest() {
        anim::perf_test();
    }

    // ===== 8.5 初始化网络（Phase 7：virtio-net + TCP/IP） =====
    net_boot(&virtio_devs[..virtio_count]);

    // ===== 8.55 初始化 Wi-Fi（/dev/wifi：HAL 后端 + 扫描/握手状态机） =====
    // QEMU：SimWifi（真实 4-way 密码学链路）；Pi4：CYW43455 SDIO 骨架。
    // worker 线程在 init_main 中随调度器启动（service() 需调度器上下文）。
    wifi::init();

    // ===== 9. 初始化 Phase 4：调度器、电源管理、IPC、进程 =====
    sched_cfs::init();
    // 电源管理：注册到调度 tick（10ms），负责负载采样 / DVFS / 热节流 /
    // 电池模型。必须在调度器启动前调用（boot 上下文）。
    power::init();
    ipc::init();
    bionic_shim::init();
    kinfo! { uart_send_str("[boot] 进程初始化…\n"); }
    process::init(init_main);
    kinfo! { uart_send_str("[boot] 初始化完成，启动调度器…\n"); }
    crate::printk!(crate::log::LOG_INFO, "boot", "内核初始化完成，启动调度器");

    // ===== 10. 启动调度器（永不返回） =====
    // SMP：释放 secondary 核（它们等待此标志后初始化本核 GIC/定时器并
    // 进入调度器）；仅当确有多个 CPU 时才需要释放。
    let ncpu = smp::cpu_count();
    if ncpu > 1 && bootargs_smp() {
        // ★ 显式拉起次核：OpenSBI 只带 boot hart 到 _start，其余核停在
        // OpenSBI HSM 停车场。hart_start 使其进入内核 park（a0=hartid），
        // park 等 SCHED_READY 后走 smp_secondary_main 上线。
        extern "C" {
            static park: u8; // 汇编标签（仅取地址）
        }
        let park_addr = core::ptr::addr_of!(park) as u64;
        let boot = smp::cpu_id();
        // ★ 先置 SCHED_READY 再 hart_start：park 的等待是 wfi 轮询，新核
        // 尚无任何 pending 中断源（定时器在 secondary_main 里才编程），
        // 若到达时标志未置会 wfi 永眠——置位在前，到达即通过首次检查。
        smp::SCHED_READY.store(true, core::sync::atomic::Ordering::Release);
        for h in 0..ncpu {
            if h == boot {
                continue;
            }
            let r = sbi::hart_start(h as u64, park_addr, 0);
            uart_send_str("[smp] hart_start ");
            print_dec(h as u64);
            uart_send_str(" → park r=");
            print_dec(r as u64);
            uart_send_str("\n");
        }
    }
    sched_cfs::start();
}

// ==================== init 进程 ====================

/// 内核引导进程（pid 0）：加载用户态 `/init`（kunxinit）为 pid 1 并等待其
/// 退出；随后回退到内置交互式 Shell。
///
/// 多进程模型：`/init` 以真正的 `fork`/`execve` 拉起系统守护进程
/// （networkd / wifid / camerad / audiod / lifd / alarmd / sensord / rild
///  + launcher；displayd 已废弃），各进程持有独立页表，由调度器在
/// 上下文切换时切换 TTBR0_EL1。
fn init_main(_: usize) -> ! {
    // 启动提示音（880Hz / 120ms）：经 virtio-sound 低延迟路径播放，
    // 顺带验证 IRQ 驱动补充与环形缓冲（无音频设备时静默跳过）。
    audio::boot_beep();

    // 电源管理逻辑自检（bootargs 含 "selftest" 才执行，默认跳过）：纯函数
    // （governor/热模型/电池状态机），与其余启动期自检统一为可选诊断，
    // 运行时仍可用 shell `power selftest` 单独触发（[power-selftest] OK/FAIL）。
    if crate::bootargs_selftest() {
        kinfo! { crate::uart_send_str("[kernel] 电源自检开始\n"); }
        let power_fails = power::selftest();
        // 自检 PASS/FAIL 结论必须可见（不受安静模式门控 —— 见 kinfo! 的
        // 使用规范：「自检 PASS-FAIL 等必须可见的输出不要包进来」），
        // power_test.py 的 (boot) 断言依赖此行。
        crate::uart_send_str("[kernel] 电源自检完成：");
        crate::print_dec(power_fails as u64);
        crate::uart_send_str(" 个失败\n");
    }

    // bootargs 含 "shell"（QEMU -append "shell"）：GUI 启动同时拉起内置
    // 交互 shell，供 UART 驱动测试（power_test.py）。
    if crate::bootargs_shell() {
        kinfo! { crate::uart_send_str("[kernel] bootargs shell: 与 /init 并行启动 shell\n"); }
        process::spawn("shell", shell::shell_main, 0, sched_cfs::PriorityClass::Normal, 0);
    }

    // Wi-Fi worker：周期推进扫描 / 4-way 握手 / 断链检测（/dev/wifi 的后端）。
    // 必须在调度器启动后才能 spawn（依赖进程与上下文切换）。
    // QEMU `-append "wifitest"`：先跑无头自检（扫描 → WPA2-PSK 4-way → 拒绝
    // 错误口令），再拉起常驻 worker，避免自检与 worker 并发推进同一状态机。
    if crate::bootargs_wifitest() {
        wifi::selftest();
    }
    if wifi::initialized() {
        process::spawn("wifi", wifi::wifi_worker, 0, sched_cfs::PriorityClass::Normal, 0);
    }

    // 首次启动数据迁移：/data/oobe/.done 不存在时，把 /etc/oobe_default/ 的
    // 默认配置复制到 /data/oobe/（modules / modules_removed / lang / theme 等），
    // 供 OOBE 预填与 launcher 过滤应用。必须在调度器启动后执行，因为
    // copy_tree 经 vfs::open 分配 fd，fd 依赖「当前进程」上下文。
    if !file_exists("/data/oobe/.done") {
        kinfo! { uart_send_str("[kernel] 首次启动：迁移 /etc/oobe_default -> /data/oobe\n"); }
        copy_tree("/etc/oobe_default", "/data/oobe");
        kinfo! { uart_send_str("[kernel] /data/oobe 默认配置迁移完成\n"); }
    }

    kinfo! { uart_send_str("[kernel] 正在从 ramdisk 加载 /init…\n"); }
    crate::printk!(crate::log::LOG_NOTICE, "boot", "正在加载 /init");

    // 恢复模式：不启动正常用户态；挂载恢复 ramdisk 并运行恢复菜单。
    if crate::bootargs_recovery() {
        uart_send_str("[kernel] 恢复模式\n");
        let (raddr, rsize) = crate::sec_recovery_ramdisk();
        if ota::mount_recovery(raddr, rsize) == 0 {
            uart_send_str("[kernel] 恢复 ramdisk 已挂载到 /mnt/recovery\n");
            let pid = elf::spawn_from_path("/mnt/recovery/bin/recovery", &["recovery"]);
            if pid >= 0 {
                loop {
                    let (rpid, _code) = process::wait(pid);
                    if rpid < 0 {
                        break;
                    }
                }
            } else {
                uart_send_str("[kernel] 未找到 /mnt/recovery/bin/recovery\n");
            }
        } else {
            uart_send_str("[kernel] 恢复 ramdisk 挂载失败\n");
        }
        process::spawn("shell", shell::shell_main, 0, sched_cfs::PriorityClass::Normal, 0);
        loop {
            thread::yield_now();
        }
    }

    let pid = elf::spawn_from_path("/init", &["init"]);
    if pid >= 0 {
        // 启动链确认：/init 成功拉起 → 内核记录 BCB 启动成功（尝试计数清零）。
        ota::note_boot_success(crate::sec_build_version());
        // 帧统计/诊断报告线程：anim 与 perf 的延迟报告在 IRQ/呈现热路径只置
        // 标记，需要线程上下文冲刷。kmain 此后阻塞于 wait4 不能承担，故由
        // 本低优先级内核线程常驻轮询（GUI 正常启动路径的唯一冲刷点）。
        // 注意不能用 Idle 类：pick_next 把 Idle 类任务一律视为每核 idle 循环
        // （非本核 idle 槽位即跳过），Idle 类普通任务永远不会被选中。
        process::spawn(
            "perflog",
            perf_log_main,
            0,
            sched_cfs::PriorityClass::Normal,
            5,
        );
        // 等待 /init 退出，并监督其生死：
        //   - 正常退出（code=0：launcher 干净退出后 init 的规划路径）→ 走
        //     旧的「内核 shell 接管」路径；
        //   - 异常退出（panic exit 1 / 被遏制 -11 等）→ 用户态软复位：清理
        //     残余用户进程（防止新旧服务并存冲突）后重拉 /init，最多
        //     INIT_RESPAWN_MAX 次；超限回退内置 shell（系统仍可用控制台）。
        const INIT_RESPAWN_MAX: u32 = 3;
        let mut init_pid = pid;
        let mut respawns = 0u32;
        loop {
            let (rpid, code) = process::wait(init_pid);
            if rpid < 0 {
                break;
            }
            kinfo! {
                uart_send_str("[kernel] /init (pid ");
                print_dec(rpid as u64);
                uart_send_str(") 退出 code=");
                print_dec(code as u64);
                uart_send_str("\n");
            }
            // 退出码 2：kunxinit 在「launcher 崩溃循环放弃」后的规划退出
            // （GUI 已死）——不再软复位（重拉也一样会崩），走崩溃告知屏。
            if code == 2 {
                crate::crash_screen::show(
                    "桌面启动器反复崩溃，图形界面已停止",
                    &[
                        "系统其余部分（服务、网络、通话）仍在运行。",
                    ],
                );
                break;
            }
            if code == 0 || respawns >= INIT_RESPAWN_MAX {
                if code != 0 {
                    // 软复位次数耗尽：init 自身有系统性问题——告知用户后
                    // 回退控制台（系统仍运行，GUI 降级）。
                    crate::crash_screen::show(
                        "系统服务管理器反复崩溃，已回退控制台模式",
                        &["系统以最小形态继续运行（串口可操作）。"],
                    );
                }
                break;
            }
            respawns += 1;
            bootfail_bump();
            uart_send_str("[kernel] init 异常退出（code=");
            print_dec(code as u64);
            uart_send_str("）：用户态软复位，重拉 /init（第 ");
            print_dec(respawns as u64);
            uart_send_str(" 次）\n");
            crate::health::report(
                crate::health::SEV_CRITICAL,
                "init",
                "系统服务管理器（init）异常退出，正在自动恢复用户环境",
                "应用与服务会被重新启动，未保存数据可能丢失；若反复出现请重启设备或重新刷机",
            );
            // 先回收旧 init 槽位（其僵尸态会阻断 pid 复用与孤儿收养）
            elf::release(init_pid as u32);
            // 清理残余用户进程（守护进程/应用），保留内核线程（shell/perflog）
            process::kill_all_user();
            let npid = elf::spawn_from_path("/init", &["init"]);
            if npid < 0 {
                uart_send_str("[kernel] 重拉 /init 失败，回退内置 shell\n");
                crate::crash_screen::show(
                    "系统服务管理器无法启动，已回退控制台",
                    &["系统以控制台模式继续运行（串口可操作）。"],
                );
                break;
            }
            init_pid = npid;
        }
        elf::release(init_pid as u32);
    } else {
        uart_send_str("[kernel] 未找到 /init，回退到内置 shell\n");
    }

    // 回退：内置交互式 Shell（内核线程，独立进程槽位）。
    // bootargs "shell" 已并行拉起过一个 shell（BOOT_SHELL）：init 退出后
    // 不再重复拉起 —— 两个 shell 并发读同一 UART 输入流会把输入按字节
    // 劈开（lifecycle_core_test 的命令被劈成两半，双方都不执行）。
    if !crate::bootargs_shell() {
        process::spawn("shell", shell::shell_main, 0, sched_cfs::PriorityClass::Normal, 0);
    }

    // 按需分页 / COW 统计（诊断）：累计落地缺页数与 COW break 次数。
    kinfo! {
        uart_send_str("[kernel] 按需分页：已解决缺页数=");
        crate::print_dec(process::page_faults_resolved());
        uart_send_str(" COW 拆页数=");
        crate::print_dec(process::cow_breaks());
        uart_send_str("\n");
    }

    loop {
        // 帧统计打印在 IRQ 侧只置标记，这里（线程上下文）冲刷。
        anim::poll_deferred_stats();
        // 应用直绘帧统计 / t2f 报告（perf 侧同样只置标记，此处冲刷）。
        crate::perf::poll_report();
        thread::yield_now();
    }
}

/// 帧统计/诊断报告线程（Idle 类内核线程，正常 GUI 启动路径也会拉起）：
/// 冲刷 anim 与 perf 在 IRQ/呈现热路径置位的延迟统计标记。打印约 200 字符
/// 在 115200 波特下忙等 ~17ms，绝不能出现在呈现/IRQ 路径——本线程以
/// 200ms 周期轮询，有标记才打印，无标记时完全让出 CPU。
fn perf_log_main(_: usize) -> ! {
    let mut tick = 0u32;
    loop {
        anim::poll_deferred_stats();
        crate::perf::poll_report();
        // ---- 资源自愈扫描（每 2 秒一轮，200ms × 10）----
        tick = tick.wrapping_add(1);
        if tick % 10 == 0 {
            // 孤儿僵尸回收（init 死亡窗口泄漏的槽位）
            let reaped = process::reap_orphan_zombies();
            if reaped > 0 {
                crate::health::report(
                    crate::health::SEV_INFO,
                    "selfheal",
                    "已自动清理无主残留进程，释放系统资源",
                    "无需处理；若伴随功能异常请重启设备",
                );
            }
            // 槽位压力（进程 24 / 任务 32，余量 4 以内告警）
            let procs = process::active_count();
            let tasks = sched_cfs::task_count();
            if procs + 4 >= process::MAX_PROC || tasks + 4 >= sched_cfs::MAX_TASKS {
                crate::health::report(
                    crate::health::SEV_WARN,
                    "selfheal",
                    "进程/任务槽位接近耗尽，新进程可能启动失败",
                    "请关闭不使用的应用；若持续出现请重启设备",
                );
            }
            // 开机 120 秒稳定：清除崩溃循环计数（本次会话已证明健康）
            if tick == 600 {
                let _ = vfs::unlink("/data/.bootfails");
            }
        }
        sched_cfs::sleep_until(timer::now_us() + 200_000);
    }
}

/// 按最近睡眠截止时间动态重装物理定时器，同时保持 10ms 调度时间片上限。
fn timer_rearm() {
    timer::timer_set_interval(sched_cfs::next_timer_interval_us());
}

/// 检查路径是否可解析（文件或目录存在）。
#[allow(dead_code)]
fn file_exists(path: &str) -> bool {
    vfs::stat_path(path).is_some()
}

/// 读取崩溃循环计数（/data/.bootfails，单字节十进制；无文件返回 None）。
fn bootfail_read() -> Option<u32> {
    let fd = vfs::open("/data/.bootfails", vfs::O_RDONLY);
    if fd < 0 {
        return None;
    }
    let mut b = [0u8; 4];
    let n = vfs::read(fd, &mut b);
    vfs::close(fd);
    if n <= 0 {
        return None;
    }
    let mut v = 0u32;
    for &c in &b[..n as usize] {
        if !c.is_ascii_digit() {
            break;
        }
        v = (v * 10 + (c - b'0') as u32).min(99);
    }
    Some(v)
}

/// 崩溃循环计数 +1（用户态软复位时调用）。
fn bootfail_bump() {
    let n = bootfail_read().unwrap_or(0) + 1;
    let b = [b'0' + (n % 100 / 10) as u8, b'0' + (n % 10) as u8];
    let fd = vfs::open("/data/.bootfails", vfs::O_CREAT | vfs::O_WRONLY | vfs::O_TRUNC);
    if fd >= 0 {
        let mut off = 0usize;
        while off < b.len() {
            let w = vfs::write(fd, &b[off..]);
            if w <= 0 {
                break;
            }
            off += w as usize;
        }
        vfs::close(fd);
    }
}

// ==================== 自检 ====================

/// 符号链接自检：绝对/相对目标、目录穿透、悬空链接、环路检测、改名/删除。
/// 在 /tmp（tmpfs/SFS）上执行，全部通过输出 OK，失败输出 FAIL 并计数。
fn symlink_selftest() {
    let mut fails = 0usize;
    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                uart_send_str("[symlink] ");
                uart_send_str($name);
                uart_send_str(" 通过\n");
            } else {
                uart_send_str("[symlink] ");
                uart_send_str($name);
                uart_send_str(" 失败\n");
                fails += 1;
            }
        };
    }

    // 准备目标文件 /tmp/hello，内容 "HelloSym"
    let fd = vfs::open("/tmp/hello", vfs::O_CREAT | vfs::O_WRONLY);
    check!("创建目标文件", fd >= 0);
    if fd >= 0 {
        vfs::write(fd, b"HelloSym");
        vfs::close(fd);
    }

    // 1) 绝对符号链接：/tmp/link_abs -> /tmp/hello
    check!("创建绝对链接", vfs::symlink("/tmp/hello", "/tmp/link_abs") == 0);
    let mut tbuf = [0u8; 64];
    let n = vfs::readlink("/tmp/link_abs", &mut tbuf);
    check!("读取绝对链接", n == 10 && &tbuf[..10] == b"/tmp/hello");

    // 2) 相对符号链接：/tmp/link_rel -> hello（相对 /tmp）
    check!("创建相对链接", vfs::symlink("hello", "/tmp/link_rel") == 0);
    let n = vfs::readlink("/tmp/link_rel", &mut tbuf);
    check!("读取相对链接", n == 5 && &tbuf[..5] == b"hello");

    // 3) 经符号链接 open/read 读回目标内容
    let mut content = [0u8; 16];
    let fd = vfs::open("/tmp/link_abs", vfs::O_RDONLY);
    check!("经链接打开", fd >= 0);
    if fd >= 0 {
        let r = vfs::read(fd, &mut content);
        vfs::close(fd);
        check!("经链接读取", r == 8 && &content[..8] == b"HelloSym");
    }
    let fd = vfs::open("/tmp/link_rel", vfs::O_RDONLY);
    check!("打开相对链接", fd >= 0);
    if fd >= 0 {
        let r = vfs::read(fd, &mut content);
        vfs::close(fd);
        check!("经相对链接读取", r == 8 && &content[..8] == b"HelloSym");
    }

    // 4) 子目录中的符号链接：/tmp/d/l -> ../hello（相对目标含 ..）
    check!("创建目录 /tmp/d", vfs::mkdir("/tmp/d") == 0);
    check!("目录内创建链接", vfs::symlink("../hello", "/tmp/d/l") == 0);
    let fd = vfs::open("/tmp/d/l", vfs::O_RDONLY);
    check!("解析 ../ 目标", fd >= 0);
    if fd >= 0 {
        vfs::close(fd);
    }

    // 5) 悬空链接：解析失败（ENOENT 语义）
    check!("创建悬空链接", vfs::symlink("/tmp/not-exist", "/tmp/dangling") == 0);
    check!("解析悬空链接失败", {
        let mut d = 0u32;
        let mut i = 0u32;
        vfs::resolve("/tmp/dangling", &mut d, &mut i) != 0
    });

    // 6) 环路检测：/tmp/loop_a -> loop_b，/tmp/loop_b -> loop_a
    check!("创建链接 loop_a", vfs::symlink("loop_b", "/tmp/loop_a") == 0);
    check!("创建链接 loop_b", vfs::symlink("loop_a", "/tmp/loop_b") == 0);
    check!("环路解析失败（跳数上限）", {
        let mut d = 0u32;
        let mut i = 0u32;
        vfs::resolve("/tmp/loop_a", &mut d, &mut i) != 0
    });

    // 7) 在符号链接指向的目录下创建文件：/tmp/dirlink -> /tmp/d
    check!("创建指向目录的链接", vfs::symlink("/tmp/d", "/tmp/dirlink") == 0);
    let fd = vfs::open("/tmp/dirlink/newfile", vfs::O_CREAT | vfs::O_WRONLY);
    check!("在链接目录下创建文件", fd >= 0);
    if fd >= 0 {
        vfs::close(fd);
    }
    check!("实际目录中已创建", vfs::stat_path("/tmp/d/newfile").is_some());

    // 8) 重命名/删除符号链接
    check!("重命名链接", vfs::rename("/tmp/link_rel", "/tmp/link_rel2") == 0);
    check!("删除链接", vfs::unlink("/tmp/link_rel2") == 0);
    check!("删除后链接已消失", {
        let mut d = 0u32;
        let mut i = 0u32;
        vfs::resolve("/tmp/link_rel2", &mut d, &mut i) != 0
    });
    // lstat 语义：链接自身类型为 T_SYM
    check!("lstat 为链接类型", {
        vfs::lstat_path("/tmp/link_abs").map(|(t, _)| t) == Some(crate::fs_simple::T_SYM)
    });

    uart_send_str("[symlink] 自检 ");
    if fails == 0 {
        uart_send_str("全部通过\n");
    } else {
        uart_send_str("失败（");
        print_dec(fails as u64);
        uart_send_str("）\n");
    }
}

/// 自检：验证 kmalloc/kfree 无泄漏、map/unmap 正确。
fn self_test() {
    // ---- 1. 堆分配器 ----
    uart_send_str("[test] kmalloc/kfree ...\n");
    let before = allocator::used_pages();

    // 每个分级分配一块并写入魔数
    let mut small: [*mut u8; 9] = [core::ptr::null_mut(); 9];
    for (i, &size) in heap::CLASS_SIZES.iter().enumerate() {
        small[i] = heap::kmalloc(size);
        unsafe { *(small[i] as *mut u64) = 0xDEAD_BEEF_0000_0000 + i as u64; }
    }
    // 校验魔数
    let mut ok = true;
    for (i, _) in heap::CLASS_SIZES.iter().enumerate() {
        if unsafe { *(small[i] as *mut u64) } != 0xDEAD_BEEF_0000_0000 + i as u64 {
            ok = false;
        }
    }
    // 大块分配（> 2048）
    let large = heap::kmalloc(3000);
    unsafe { *(large as *mut u64) = 0x1234_5678_9ABC_DEF0; }
    if unsafe { *(large as *mut u64) } != 0x1234_5678_9ABC_DEF0 {
        ok = false;
    }

    // 全部释放
    for p in small.iter() {
        unsafe { heap::kfree(*p); }
    }
    unsafe { heap::kfree(large); }

    // 再次分配并释放（验证空闲链表复用）
    for (i, &size) in heap::CLASS_SIZES.iter().enumerate() {
        small[i] = heap::kmalloc(size);
    }
    for p in small.iter() {
        unsafe { heap::kfree(*p); }
    }

    let after = allocator::used_pages();
    uart_send_str("[test] 堆：使用前页数=");
    print_dec(before as u64);
    uart_send_str(" 使用后页数=");
    print_dec(after as u64);
    uart_send_str("\n");
    if ok && before == after {
        uart_send_str("[test] kmalloc/kfree 通过（无泄漏）\n");
    } else {
        uart_send_str("[test] kmalloc/kfree 失败\n");
    }

    // ---- 2. 动画引擎自检（iOS/Android 缓动曲线与弹簧数值验证） ----
    anim::self_test();

    // ---- 3. 映射 / 取消映射 ----
    uart_send_str("[test] map/unmap ...\n");
    const TEST_VA: usize = 0x2_0000_0000; // 尚未使用的低地址（L1[8] 原为无效项）
    let pa = allocator::alloc_page().expect("self_test: alloc_page failed");
    unsafe { *(pa as *mut u64) = 0xCAFE_BABE; }

    // 映射到 TEST_VA 并验证 VA/PA 指向同一物理页
    if !vm::map_page(TEST_VA, pa, vm::FLAG_KERNEL_DATA) {
        uart_send_str("[test] map_page 失败（页表 OOM）\n");
        return;
    }
    let via_va = unsafe { *(TEST_VA as *mut u64) };
    unsafe { *(TEST_VA as *mut u64) = 0x1122_3344_5566_7788; }
    let via_pa = unsafe { *(pa as *mut u64) };

    if via_va == 0xCAFE_BABE && via_pa == 0x1122_3344_5566_7788 {
        uart_send_str("[test] map_page 通过\n");
    } else {
        uart_send_str("[test] map_page 失败\n");
    }

    // 取消映射并归还物理页
    vm::unmap_page(TEST_VA);
    allocator::free_page(pa);
    uart_send_str("[test] unmap_page 通过\n");
}

// ==================== Phase 5：文件系统 ====================

/// RAM 磁盘主设备号与大小。
const RAMDISK_MAJOR: u32 = 1;
/// RAM 磁盘块数（与 tools/mkfs_sfs.py 生成的镜像大小一致：1536 × 4KB = 6MB，
/// 容纳全部应用 + cn.ttf 平滑字体子集）。
const RAMDISK_BLOCKS: usize = 4096;

/// 内嵌的 rootfs 镜像（由顶层 `make rootfs` 用 tools/mkfs_sfs.py 生成，
/// 输出到 kunxcore/ramdisk.img，随内核一并编译进 .rodata）。
const RAMDISK_IMG: &[u8] = include_bytes!("../ramdisk.img");

/// 初始化块设备、设备文件与 VFS，并把内嵌 rootfs 镜像挂载到根目录。
fn fs_boot() {
    kinfo! { uart_send_str("[fs] Phase 5: 文件系统\n"); }

    // 1. 注册字符设备（/dev/null、/dev/zero、/dev/tty、/dev/ttyN、/dev/ptmx）
    //    并初始化 TTY/PTY 会话（图形终端会话 0 附接后镜像串口输出）。
    tty::init();
    devfs::init();

    // 2. 创建 RAM 磁盘并注册为块设备
    blkdev::ramdisk_create("ramdisk", RAMDISK_MAJOR, RAMDISK_BLOCKS);

    // 3. 把内嵌 rootfs 镜像逐块写入 RAM 磁盘（镜像本身已是格式化好的 SFS）
    let blocks = RAMDISK_IMG.len() / blkdev::BLK_SIZE;
    for i in 0..blocks {
        let mut buf = [0u8; blkdev::BLK_SIZE];
        let base = i * blkdev::BLK_SIZE;
        buf.copy_from_slice(&RAMDISK_IMG[base..base + blkdev::BLK_SIZE]);
        blkdev::blk_write(RAMDISK_MAJOR, i, &buf);
    }

    // 3.1 安全启动链自校验：内嵌 rootfs 与 bootloader 验签的 RAMDISK 载荷摘要
    // 一致才挂载（防编译链被替换；bootloader 直载路径（make run）无该属性 → 跳过）。
    let prop_sha = crate::sec_ramdisk_sha();
    let missing = prop_sha.iter().all(|&b| b == 0xEE);
    if !missing {
        let actual = kunxsec::sha256::sha256(RAMDISK_IMG);
        if actual != *prop_sha {
            uart_send_str("[sec] 失败：内嵌 ramdisk 的 SHA-256 不匹配！\n");
            loop {
                core::hint::spin_loop();
            }
        }
        kinfo! { uart_send_str("[sec] ramdisk 自检通过（sha256 与 bootloader 一致）\n"); }
    }

    // 4. 挂载到 "/"（镜像含超级块，无需再 mkfs）
    vfs::init();
    if vfs::mount(RAMDISK_MAJOR, "/") != 0 {
        uart_send_str("[fs] 挂载失败\n");
        return;
    }

    // 5. mknod 设备文件（/dev 目录已存在于镜像中）
    vfs::mknod("/dev/null", vfs::FileType::Chr, devfs::MEM_MAJOR, devfs::NULL_MINOR);
    vfs::mknod("/dev/zero", vfs::FileType::Chr, devfs::MEM_MAJOR, devfs::ZERO_MINOR);
    vfs::mknod("/dev/tty", vfs::FileType::Chr, devfs::TTY_MAJOR, devfs::TTY_MINOR);
    // PTY 分配器 /dev/ptmx：预留下一会话（多终端）。
    vfs::mknod("/dev/ptmx", vfs::FileType::Chr, devfs::PTY_MAJOR, devfs::PTMX_MINOR);
    // 会话终端 /dev/tty1..ttyN（图形终端主侧；会话 0 绑定内核 Shell）。
    let mut tty_path = [0u8; 32];
    tty_path[..8].copy_from_slice(b"/dev/tty");
    for i in 1..=devfs::TTY_SESSION_MAX {
        // no_std（无 alloc）：手写十进制，避免 `.to_string()`（no_std 不含 ToString）
        let mut digits = [0u8; 8];
        let mut v = i as usize;
        let mut dlen = 0usize;
        loop {
            digits[dlen] = b'0' + (v % 10) as u8;
            dlen += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        let n = 8 + dlen;
        for k in 0..dlen {
            tty_path[8 + k] = digits[dlen - 1 - k];
        }
        tty_path[n] = 0;
        let path = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(tty_path.as_ptr(), n))
        };
        vfs::mknod(path, vfs::FileType::Chr, devfs::TTY_MAJOR, devfs::TTY_SESSION_BASE + i - 1);
    }
    vfs::mknod("/dev/kmsg", vfs::FileType::Chr, devfs::KMSG_MAJOR, devfs::KMSG_MINOR);

    kinfo! { uart_send_str("[fs] 已从内嵌 ramdisk 挂载根文件系统\n"); }
    crate::printk!(crate::log::LOG_INFO, "fs", "已挂载根文件系统（内嵌 ramdisk）");
}

// ==================== /data 持久化（virtio-blk + MBR 分区 + SFS） ====================

/// 探测 virtio-blk，解析 MBR 分区表，把 data 分区（sda3）挂载到 /data。
///
/// 若数据分区无 SFS 签名则先 mkfs；首次挂载时若 `/data/oobe/.done` 不存在，
/// 从 `/etc/oobe_default/` 复制默认配置。
fn data_boot(devs: &[net_virtio::MmioDev]) {
    // 1. 探测 virtio-blk（找不到则 /data 退回 ramdisk，不持久化）
    if !virtio_blk::init(devs) {
        kinfo! { uart_send_str("[blk] 无 virtio-blk 设备，/data 未持久化\n"); }
        return;
    }

    // 2. 读取 MBR（扇区 0）
    let mut mbr = [0u8; 512];
    if blkdev::blk_read_sectors(virtio_blk::MAJOR, 0, 1, &mut mbr) != 512 {
        uart_send_str("[blk] 读取 MBR 失败\n");
        return;
    }
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        uart_send_str("[blk] MBR 签名无效\n");
        return;
    }

    // 3. 解析 4 个主分区并注册
    let mut data_major = 0u32;
    for i in 0..4 {
        let e = 446 + i * 16;
        let typ = mbr[e + 4];
        if typ == 0 {
            continue;
        }
        let start = u32::from_le_bytes([mbr[e + 8], mbr[e + 9], mbr[e + 10], mbr[e + 11]])
            as usize;
        let count = u32::from_le_bytes([mbr[e + 12], mbr[e + 13], mbr[e + 14], mbr[e + 15]])
            as usize;
        if count == 0 {
            continue;
        }
        let part_num = (i + 1) as u32;
        let major =
            blkdev::blkdev_register_partition(virtio_blk::MAJOR, part_num, start, count);
        if major < 0 {
            continue;
        }
        kinfo! {
            uart_send_str("[blk] /dev/sda");
            print_dec(part_num as u64);
            uart_send_str(" type=0x");
            print_hex_byte(typ);
            uart_send_str(" start=");
            print_dec(start as u64);
            uart_send_str(" sectors=");
            print_dec(count as u64);
            uart_send_str("\n");
        }
        if part_num == 3 {
            if typ == blkdev::ENC_PART_TYPE {
                kinfo! { uart_send_str("[blk] /dev/sda3 已加密（XOR 演示密钥），正在包装\n"); }
                let enc = blkdev::wrap_encryption(major as u32);
                if enc >= 0 {
                    data_major = enc as u32;
                } else {
                    data_major = major as u32;
                }
            } else {
                data_major = major as u32;
            }
            kinfo! { uart_send_str("[blk] 检测到 /dev/sda3，type=SFS\n"); }
        }
        // 探测文件系统类型：
        // P1/P4 = FAT32 boot_a/boot_b（挂载 /mnt/fat32、/mnt/fat32b，OTA 写入非活动槽用）；
        // P2 = SFS 系统暂存（/mnt/sys，OTA 下载缓存）。
        let fs_kind = crate::fsops::probe(major as u32);
        match (part_num, fs_kind) {
            (1, Some(crate::fsops::FsKind::Fat32)) => {
                kinfo! { uart_send_str("[fs] /dev/sda1 FAT32 -> /mnt/fat32\n"); }
                if vfs::mount(major as u32, "/mnt/fat32") != 0 {
                    uart_send_str("[fs] 挂载 /mnt/fat32 失败\n");
                }
            }
            // sda2 = sys 暂存分区：避免重复挂载 SFS 导致 VFS 解析状态受扰（见 docs/ota.md），
            // OTA 包改存 /data/ota/（/data 挂载与既有路径一致）。
            (4, Some(crate::fsops::FsKind::Fat32)) => {
                kinfo! { uart_send_str("[fs] /dev/sda4 FAT32 -> /mnt/fat32b（非活动槽）\n"); }
                if vfs::mount(major as u32, "/mnt/fat32b") != 0 {
                    uart_send_str("[fs] 挂载 /mnt/fat32b 失败\n");
                }
            }
            _ => {}
        }
    }

    if data_major == 0 {
        kinfo! { uart_send_str("[blk] 未找到数据分区（sda3），/data 未持久化\n"); }
        return;
    }

    // 4. 检查 /dev/sda3 的 SFS 签名：无则 mkfs
    if fs_simple::fs_probe(data_major) {
        kinfo! { uart_send_str("[fs] /data 含 SFS 签名\n"); }
    } else {
        kinfo! { uart_send_str("[fs] /data 未格式化，正在 mkfs...\n"); }
        if fs_simple::mkfs(data_major) != 0 {
            uart_send_str("[fs] /data mkfs 失败\n");
            return;
        }
    }

    // 5. 挂载到 /data
    if vfs::mount(data_major, "/data") != 0 {
        uart_send_str("[fs] 挂载 /data 失败\n");
        return;
    }
    kinfo! { uart_send_str("[fs] /data 已挂载\n"); }

    // 6. 脏启动检测（看门狗/崩溃复位的用户告知链）：上次启动若未正常
    //    关机（/data/.bootdirty 残留），本次启动记健康事件告知用户；
    //    随后重新落脏标志，正常关机路径（reboot/poweroff）会清除它。
    if vfs::stat_path("/data/.bootdirty").is_some() {
        crate::health::report(
            crate::health::SEV_WARN,
            "boot",
            "上次关机异常（系统失去响应后被看门狗自动重启，或意外断电）",
            "若非手动断电，且此提示反复出现，请查看 /data/crash 崩溃报告（shell: crash）并考虑重新刷机",
        );
        let _ = vfs::unlink("/data/.bootdirty");
    }
    let d = vfs::open(
        "/data/.bootdirty",
        vfs::O_CREAT | vfs::O_WRONLY | vfs::O_TRUNC,
    );
    if d >= 0 {
        vfs::write(d, b"1");
        vfs::close(d);
    }

    // 7. 崩溃循环检测：本分区记录的异常重启次数达到阈值时，开机即告知
    //    用户（GUI 经健康横幅展示）并给出处置建议；开机 120 秒稳定后由
    //    perflog 清零（一次成功会话即视为恢复）。
    if let Some(n) = bootfail_read() {
        if n >= 2 {
            crate::health::report(
                crate::health::SEV_CRITICAL,
                "boot",
                "系统近期反复异常重启（累计 N 次以上），可能存在深层故障",
                "建议尽快备份 /data 中的数据；可尝试清除数据恢复出厂（将丢失用户数据），或重新刷机",
            );
        }
    }

    // 数据迁移（首次持久化复制默认配置）在 init_main 中进行（见 init_main：
    // /data/oobe/.done 不存在时调用 copy_tree("/etc/oobe_default", "/data/oobe")），
    // 因为复制需要 vfs::open 分配 fd，而 fd 依赖「当前进程」（调度器启动后才有）。
}

/// 递归复制 `src` 目录树到 `dst`（仅复制普通文件与子目录）。
fn copy_tree(src: &str, dst: &str) {
    vfs::mkdir(dst);
    let mut name = [0u8; 32];
    let mut idx = 0usize;
    loop {
        let ino = vfs::readdir(src, idx, &mut name);
        if ino < 0 {
            break;
        }
        idx += 1;
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let nm = core::str::from_utf8(&name[..len]).unwrap_or("");
        if nm == "." || nm == ".." {
            continue;
        }
        if nm == ".done" {
            // .done 是 OOBE 完成标志：绝不能作为默认配置迁入 /data/oobe，
            // 否则会误判 OOBE 已完成而跳过首次启动向导。
            continue;
        }
        let mut sp = [0u8; 128];
        let mut dp = [0u8; 128];
        join_path(&mut sp, src, nm);
        join_path(&mut dp, dst, nm);
        let sp_str = buf_str(&sp);
        let dp_str = buf_str(&dp);
        if let Some((typ, _)) = vfs::stat_path(sp_str) {
            if typ == fs_simple::T_DIR {
                copy_tree(sp_str, dp_str);
            } else {
                copy_file(sp_str, dp_str);
            }
        }
    }
}

/// 把 `src` 文件内容复制到 `dst`（O_CREAT | O_TRUNC）。
fn copy_file(src: &str, dst: &str) {
    let fd = vfs::open(src, vfs::O_RDONLY);
    if fd < 0 {
        return;
    }
    let dfd = vfs::open(dst, vfs::O_WRONLY | vfs::O_CREAT | vfs::O_TRUNC);
    if dfd < 0 {
        vfs::close(fd);
        return;
    }
    let mut buf = [0u8; 1024];
    loop {
        let n = vfs::read(fd, &mut buf);
        if n <= 0 {
            break;
        }
        vfs::write(dfd, &buf[..n as usize]);
    }
    vfs::close(dfd);
    vfs::close(fd);
}

/// 把 `dir` 与 `name` 拼成路径写入 `out`（自动补 `/`），返回末尾下标。
fn join_path(out: &mut [u8], dir: &str, name: &str) -> usize {
    let mut n = 0usize;
    for &c in dir.as_bytes() {
        if n + 1 >= out.len() {
            break;
        }
        out[n] = c;
        n += 1;
    }
    if n > 0 && out[n - 1] != b'/' && n + 1 < out.len() {
        out[n] = b'/';
        n += 1;
    }
    for &c in name.as_bytes() {
        if n + 1 >= out.len() {
            break;
        }
        out[n] = c;
        n += 1;
    }
    out[n] = 0;
    n
}

/// 把以 NUL 结尾的字节缓冲转成 `&str`。
fn buf_str(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

// ==================== Phase 6：图形 / 合成器 / 输入 / UI ====================

/// 开机 logo 停留时长（微秒）。
const BOOT_SPLASH_HOLD_US: u64 = 1_200_000;

// ==================== bootloader 启动画面交接 ====================

/// bootloader 写入交接信息的物理地址（与 kunxboot/src/splash.rs 一致）。
///
/// RISC-V：kunxboot（ARM bootloader）不参与启动，此地址只需**可安全读取**
/// 的清零内存（magic 校验不过 → 无交接，走正常 fb_init 路径）。选 0x8010_0000
/// （OpenSBI 区之上、内核 0x80200000 之下的空闲 RAM，恒等映射 2MB 块覆盖）。
/// aarch64 保持 kunxboot 协议地址 0x46F0_0000。
#[cfg(target_arch = "riscv64")]
const SPLASH_HANDOFF_ADDR: usize = 0x8010_0000;
#[cfg(not(target_arch = "riscv64"))]
const SPLASH_HANDOFF_ADDR: usize = 0x46F0_0000;
/// 交接信息魔数（"KNXS"）。
const SPLASH_MAGIC: u32 = 0x4B4E_5853;
/// 淡出时长（微秒）。
const SPLASH_FADE_US: u64 = 150_000;
/// 淡出步进（微秒）。
const SPLASH_FADE_STEP_US: u64 = 16_000;

/// bootloader 交接的启动画面信息。
#[derive(Clone, Copy)]
#[repr(C)]
struct SplashHandoff {
    magic: u32,
    fb_addr: u64,
    width: u32,
    height: u32,
    stride: u32,
    logo_x: u32,
    logo_y: u32,
    logo_w: u32,
    logo_h: u32,
    active: u8,
    _pad: [u8; 3],
}

/// bootloader 渲染的整帧画面（供淡出），由 `splash_capture` 保存。
static mut SPLASH_SAVE: *mut u32 = core::ptr::null_mut();
static mut SPLASH_SAVE_PIXELS: usize = 0;

/// 读取 bootloader 的启动画面交接信息（magic/active 校验通过才返回）。
fn splash_handoff() -> Option<SplashHandoff> {
    let ho = unsafe { &*(SPLASH_HANDOFF_ADDR as *const SplashHandoff) };
    if ho.magic == SPLASH_MAGIC && ho.active != 0 {
        Some(*ho)
    } else {
        None
    }
}

/// 把当前帧缓冲内容（bootloader 渲染的启动画面）保存为淡出素材。
fn splash_capture(_ho: &SplashHandoff) {
    let fb = gpu_simple::fb_base();
    let n = gpu_simple::fb_len() / 4;
    if fb == 0 || n == 0 {
        return;
    }
    let buf = heap::kmalloc(n * 4) as *mut u32;
    if buf.is_null() {
        return;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(fb as *const u32, buf, n);
        SPLASH_SAVE = buf;
        SPLASH_SAVE_PIXELS = n;
    }
}

/// 线性淡出（150ms）：把 bootloader 的启动画面叠加在桌面首帧上，
/// alpha 从 255 线性降到 0；由用户态在首帧渲染完成后经 `FBIO_FADE_SPLASH`
/// 触发，仅执行一次（执行后释放保存的画面）。
///
/// 走合成器路径：底图取当前 front（已显示的桌面首帧），逐帧把混合结果写入
/// 持久画布（`/dev/fb0` 映射目标）并经 `fb_present` 拷贝 + 页翻转上屏，
/// 保证淡出期间双缓冲无撕裂。
fn splash_fade() {
    unsafe {
        if SPLASH_SAVE.is_null() || SPLASH_SAVE_PIXELS == 0 {
            return;
        }
        if !compositor::ready() {
            return;
        }
        let n = SPLASH_SAVE_PIXELS;
        let splash = SPLASH_SAVE;
        // 捕获当前帧（front，桌面首帧 + 合成器覆盖层）作为底图，避免逐帧叠加累积误差
        let desk = heap::kmalloc(n * 4) as *mut u32;
        if desk.is_null() {
            return;
        }
        core::ptr::copy_nonoverlapping(compositor::front_buffer() as *const u32, desk, n);
        kinfo! { uart_send_str("[gpu] 启动画面淡出开始\n"); }

        let t0 = timer::now_us();
        loop {
            let t = timer::now_us().saturating_sub(t0);
            if t >= SPLASH_FADE_US {
                break;
            }
            let a = (255 - (255 * t) / SPLASH_FADE_US) as u32;
            // 混合结果写入持久画布（fb0 映射目标），再经 fb_present 拷贝上屏
            let dst = compositor::fb_target() as *mut u32;
            for i in 0..n {
                *dst.add(i) = splash_blend(*desk.add(i), *splash.add(i), a);
            }
            compositor::fb_present(&[compositor::fb_full_rect()]);
            let step = t + SPLASH_FADE_STEP_US;
            while timer::now_us().saturating_sub(t0) < step {
                core::hint::spin_loop();
            }
        }
        // 末帧：桌面完整显示
        let dst = compositor::fb_target() as *mut u32;
        core::ptr::copy_nonoverlapping(desk, dst, n);
        compositor::fb_present(&[compositor::fb_full_rect()]);

        heap::kfree(desk as *mut u8);
        heap::kfree(splash as *mut u8);
        SPLASH_SAVE = core::ptr::null_mut();
        SPLASH_SAVE_PIXELS = 0;
        kinfo! { uart_send_str("[gpu] 启动画面淡出完成\n"); }
    }
}

/// 直通 alpha 混合：`desk` 为底图，`splash` 为叠加画面，`alpha` 0..255。
fn splash_blend(desk: u32, splash: u32, alpha: u32) -> u32 {
    if alpha == 255 {
        return splash;
    }
    if alpha == 0 {
        return desk;
    }
    let a = alpha;
    let sr = (splash >> 16) & 0xFF;
    let sg = (splash >> 8) & 0xFF;
    let sb = splash & 0xFF;
    let dr = (desk >> 16) & 0xFF;
    let dg = (desk >> 8) & 0xFF;
    let db = desk & 0xFF;
    let r = (sr * a + dr * (255 - a)) / 255;
    let g = (sg * a + dg * (255 - a)) / 255;
    let b = (sb * a + db * (255 - a)) / 255;
    (r << 16) | (g << 8) | b
}

/// 开机 logo 步骤：在真实帧缓冲上绘制纯黑背景 + 白色加粗的 "KunxOS" logo
/// 与其下方作者署名，停留片刻保证可见，随后由用户态 OOBE/launcher 覆盖。
///
/// 粗体用「同一字形在 x 与 x+1 各绘制一次」模拟（cn.ttf 平滑字体）。
fn boot_splash(fb_base: usize, w: u32, h: u32) {
    let s = compositor::Surface { pixels: fb_base as *mut u32, w, h };
    // 纯黑背景
    s.fill(compositor::Color::BLACK);
    // 加载 cn.ttf 平滑字体（rootfs 已挂载）——调试:暂时跳过,排查内核堆损坏
    // ui_framework::font_init();
    let logo = "KunxOS";
    let author = "欢迎使用 KunxOS";
    let lw = ui_framework::text_width(logo);
    let aw = ui_framework::text_width(author);
    let lx = (w as i32 - lw) / 2;
    let ax = (w as i32 - aw) / 2;
    let row_h = ui_framework::GLYPH_H; // 16px 字号行高
    let ly = ((h as i32) - row_h * 2 - 10) / 2;
    let ay = ly + row_h + 10;
    ui_framework::draw_text(&s, lx, ly, logo, compositor::Color::WHITE, 1);
    ui_framework::draw_text(&s, lx + 1, ly, logo, compositor::Color::WHITE, 1);
    ui_framework::draw_text(&s, ax, ay, author, compositor::Color::WHITE, 1);
    ui_framework::draw_text(&s, ax + 1, ay, author, compositor::Color::WHITE, 1);
    // 刷新扫描输出并 clean dcache，保证 QEMU/真实硬件能看到像素
    gpu_simple::present(fb_base);
    // 停留片刻，保证用户能看到 logo 步骤
    let t0 = timer::now_us();
    while timer::now_us().saturating_sub(t0) < BOOT_SPLASH_HOLD_US {
        core::hint::spin_loop();
    }
    kinfo! { uart_send_str("[gpu] 开机画面已显示\n"); }
}

/// 初始化图形栈（帧缓冲 → /dev/fb0）。
///
/// 分配一整屏显存并立即翻转为扫描输出，同时注册 `/dev/fb0` 字符设备，
/// 使 `mmap` 能把显存映射到用户空间。launcher 等 EL0 进程直接 open/mmap
/// `/dev/fb0` 绘制，无需再经过 displayd 合成器。
fn gfx_boot(fwcfg_base: usize) {
    kinfo! { uart_send_str("[gpu] Phase 6: 图形\n"); }
    match gpu_simple::init(fwcfg_base) {
        Some((w, h)) => {
            kinfo! {
                uart_send_str("[gpu] ramfb ");
                print_dec(w as u64);
                uart_send_str("x");
                print_dec(h as u64);
                uart_send_str(" 就绪\n");
            }
            // bootloader 启动画面交接：沿用其帧缓冲（不重新翻转，画面保持显示），
            // 直到用户态渲染出桌面首帧后由 splash_fade 线性淡出
            if let Some(ho) = splash_handoff() {
                if ho.fb_addr != 0 && ho.width == w && ho.height == h {
                    let fb_base = gpu_simple::fb_adopt(ho.fb_addr as usize);
                    allocator::reserve_region(ho.fb_addr as usize, gpu_simple::fb_len());
                    allocator::reserve_region(SPLASH_HANDOFF_ADDR, 4096);
                    devfs::fb_init(fb_base, gpu_simple::fb_len(), w, h, gpu_simple::stride());
                    vfs::mknod("/dev/fb0", vfs::FileType::Chr, devfs::FB_MAJOR, devfs::FB_MINOR);
                    // 合成器接管 bootloader 显存作为 front（画面保持显示），
                    // 另分配 back 供应用直绘；/dev/fb0 映射目标 = 当前 back
                    compositor::init(fb_base);
                    gesture::init();
                    splash_capture(&ho);
                    kinfo! { uart_send_str("[gpu] 已沿用 bootloader 的开机画面\n"); }
                    return;
                }
            }
            let fb_base = gpu_simple::fb_init();
            devfs::fb_init(fb_base, gpu_simple::fb_len(), w, h, gpu_simple::stride());
            vfs::mknod("/dev/fb0", vfs::FileType::Chr, devfs::FB_MAJOR, devfs::FB_MINOR);
            kinfo! {
                uart_send_str("[gpu] /dev/fb0 就绪，帧缓冲 @ ");
                print_hex(fb_base as u64);
                uart_send_str("\n");
            }
            // 初始化合成器：/dev/fb0 缓冲作为 front（扫描输出不中断），
            // back 为应用直绘目标；应用首帧 present 后进入双缓冲翻转
            compositor::init(fb_base);
            // 全屏手势系统：Home Indicator + 多任务变暗层
            gesture::init();
            // 开机 logo 步骤：黑底 + 白色加粗 logo 与作者署名，停留片刻
            boot_splash(fb_base, w, h);
        }
        None => {
            kinfo! { uart_send_str("[gpu] 无 ramfb 设备（运行需加 -device ramfb）\n"); }
        }
    }
}

// ==================== Phase 7：网络 ====================

/// 本机网络配置（QEMU slirp 用户网络默认：10.0.2.15/24，网关 10.0.2.2，DNS 10.0.2.3）。
const NET_IP: u32 = 0x0A00_020F; // 10.0.2.15
const NET_GW: u32 = 0x0A00_0202; // 10.0.2.2
const NET_MASK: u32 = 0xFFFF_FF00; // 255.255.255.0
const NET_DNS: u32 = 0x0A00_0203; // 10.0.2.3

/// 初始化网络：驱动 virtio-net、配置协议栈与 socket 层。
fn net_boot(devs: &[net_virtio::MmioDev]) {
    kinfo! { uart_send_str("[net] Phase 7: 网络\n"); }
    if !net_virtio::init(devs) {
        kinfo! { uart_send_str("[net] 无 virtio-net 设备（可添加 -device virtio-net-device）\n"); }
        return;
    }
    let mac = net_virtio::mac();
    kinfo! {
        uart_send_str("[net] virtio-net MAC = ");
        for i in 0..6 {
            if i > 0 {
                uart_send_char(b':');
            }
            print_hex_byte(mac[i]);
        }
        uart_send_str("\n");
    }

    socket::init();
    net_stack::init(mac, NET_IP, NET_GW, NET_MASK, NET_DNS);

    kinfo! {
        uart_send_str("[net] ip=");
        print_ip(NET_IP);
        uart_send_str(" gateway=");
        print_ip(NET_GW);
        uart_send_str(" dns=");
        print_ip(NET_DNS);
        uart_send_str("\n");
    }
}

/// 打印点分十进制 IPv4 地址。
fn print_ip(ip: u32) {
    let b = ip.to_be_bytes();
    for i in 0..4 {
        if i > 0 {
            uart_send_char(b'.');
        }
        print_dec(b[i] as u64);
    }
}

/// 打印 1 字节十六进制（两位，无 0x 前缀）。
fn print_hex_byte(b: u8) {
    let hi = b >> 4;
    let lo = b & 0xF;
    uart_send_char(if hi < 10 { b'0' + hi } else { b'a' + hi - 10 });
    uart_send_char(if lo < 10 { b'0' + lo } else { b'a' + lo - 10 });
}

// ==================== Panic 处理 ====================

/// UART 写入器，实现 core::fmt::Write 以支持格式化输出
struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        uart_send_str(s);
        Ok(())
    }
}

/// panic 处理器：打印信息（含寄存器/栈/任务诊断）后死循环。
/// 与 `sync_exception_handler` 对齐：屏蔽 IRQ 防递归，输出 sp 与当前任务，
/// 栈回溯前 64 字 + 返回地址扫描，便于定位 kmalloc OOM 等 panic 的调用链。
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe { asm!("csrci sstatus, 2") };
    uart_send_str("PANIC: ");
    let mut writer = UartWriter;
    // 格式化 panic 信息（含位置与消息）
    let _ = write!(writer, "{}", info);
    uart_send_str(" pid=");
    crate::print_dec(crate::sched_cfs::current_pid() as u64);
    uart_send_str(" tid=");
    crate::print_dec(crate::sched_cfs::current_tid() as u64);
    uart_send_str(" name=");
    uart_send_str(crate::sched_cfs::current_task_name());
    uart_send_str(" sp=");
    let sp: u64;
    unsafe { asm!("mv {}, sp", out(reg) sp) };
    crate::print_hex(sp);
    // 栈回溯（前 64 个 8 字节）与返回地址扫描（.text 区内的字）
    uart_send_str("\n[stack] ");
    for i in 0..64usize {
        let v = unsafe { *((sp as *const u64).add(i)) };
        crate::print_hex(v);
        uart_send_str(" ");
    }
    uart_send_str("\n[ret-scan] ");
    for i in 0..256usize {
        let v = unsafe { *((sp as *const u64).add(i)) };
        if (0x8020_0000usize..crate::kernel_end()).contains(&(v as usize)) {
            uart_send_str("ret?=");
            crate::print_hex(v);
            uart_send_str(" ");
        }
    }
    uart_send_str("\n");
    // [crash] 生成并落盘 tombstone（best-effort，VFS 不可用时仅控制台）
    let fi = crate::crash::FaultInfo {
        kind: "PANIC",
        pid: crate::sched_cfs::current_pid(),
        name: crate::sched_cfs::current_task_name(),
        esr: 0,
        elr: 0,
        far: 0,
    };
    crate::crash::kernel_panic(&fi);
    // 故障遏制：panic 发生在 syscall 上下文（代表用户进程执行系统调用，
    // 典型如用户传入坏指针/坏数据触发的切片越界、非法解引用、算术 panic）
    // 时，把故障归咎于该进程并终止它，系统继续运行；内核自身 bug（非
    // syscall 上下文 / 内核线程 / 中断嵌套）保持整机停机诊断。
    if crate::sched_cfs::in_syscall_user() {
        let pid = crate::sched_cfs::current_pid();
        if (pid as usize) < crate::process::MAX_PROC
            && crate::process::processes_pub_ttbr0(pid) != 0
        {
            crate::uart_send_str("[contain] 内核 panic 遏制：终止 pid=");
            crate::print_dec(pid as u64);
            crate::uart_send_str(" name=");
            crate::uart_send_str(crate::sched_cfs::current_task_name());
            crate::uart_send_str("（系统继续运行）\n");
            crate::health::report_irq(
                true,
                crate::health::SEV_WARN,
                "contain",
                "内核处理应用请求时发现内部错误，已终止该进程",
                "系统已自动恢复，无需处理。若同一应用反复出现，请更新或卸载它；详情见 /data/crash（shell: crash）",
            );
            // SIGSEGV 语义退出：内部屏蔽 IRQ 并切换任务，不返回
            crate::process::exit(-11)
        }
    }
    // 内核线程 panic（非 syscall 上下文）：只终止该线程，系统继续运行
    // （tombstone 已由上方 kernel_panic 落盘）
    if crate::kthread_fault_contain("panic", 0, 0) {
        unreachable!()
    }
    // 死循环，不再返回
    loop {
        unsafe {
            wfi();
        }
    }
}

/// S 态异常的故障遏制入口（`interrupt.rs` 的 trap_dispatch 在走整机诊断前
/// 调用）。
///
/// 判定：当前任务处于 syscall 上下文（`sched_cfs::in_syscall_user`）且其
/// 进程拥有用户地址空间——即内核正在代表该用户进程执行系统调用，S 态
/// 异常（缺页/未对齐/非法指令）的来源大概率是它传入的坏指针或坏数据。
/// 此时写 tombstone（SIGSEGV 语义）后终止该进程，`process::exit` 内部
/// 切换任务不返回，系统继续调度；无法归咎（内核线程/idle/非 syscall
/// 上下文）返回 false，由调用方走 `sync_exception_handler` 整机诊断。
///
/// 已知取舍：故障点持有的内核自旋锁不释放——fs_lock 等有「死亡持有者
/// 抢锁自愈」（sched_cfs::tid_alive 判定），其余锁由后续取锁者等待；
/// 与 Linux oops 杀进程的语义一致：牺牲状态一致性换系统存活。
#[no_mangle]
pub extern "C" fn kernel_fault_contain(
    frame: *mut crate::syscall_table::TrapFrame,
    cause: u64,
    tval: u64,
) -> bool {
    if !crate::sched_cfs::in_syscall_user() {
        return false;
    }
    let pid = crate::sched_cfs::current_pid();
    if (pid as usize) >= crate::process::MAX_PROC || crate::process::processes_pub_ttbr0(pid) == 0 {
        return false; // 内核任务（无用户地址空间）：不遏制
    }
    uart_send_str("[contain] S态异常遏制：故障归咎于 pid=");
    print_dec(pid as u64);
    uart_send_str(" name=");
    uart_send_str(crate::sched_cfs::current_task_name());
    uart_send_str(" scause=");
    print_hex(cause);
    uart_send_str(" stval=");
    print_hex(tval);
    uart_send_str("（终止进程，系统继续运行）\n");
    // 健康事件（陷入上下文 IRQ 屏蔽态：跳过 VFS 落盘，tombstone 已有落盘副本）
    let mut msg = [0u8; 88];
    let mut n = 0usize;
    let mut push = |s: &str, msg: &mut [u8; 88], n: &mut usize| {
        for &c in s.as_bytes() {
            if *n < msg.len() {
                msg[*n] = c;
                *n += 1;
            }
        }
    };
    push("内核处理进程 ", &mut msg, &mut n);
    let mut pb = [0u8; 10];
    let mut pi = pb.len();
    let mut pv = pid as u64;
    loop {
        pi -= 1;
        pb[pi] = b'0' + (pv % 10) as u8;
        pv /= 10;
        if pv == 0 {
            break;
        }
    }
    push(core::str::from_utf8(&pb[pi..]).unwrap_or(""), &mut msg, &mut n);
    push("（", &mut msg, &mut n);
    push(crate::sched_cfs::current_task_name(), &mut msg, &mut n);
    push("）的请求时故障，已终止该进程", &mut msg, &mut n);
    crate::health::report_irq(
        true,
        crate::health::SEV_WARN,
        "contain",
        core::str::from_utf8(&msg[..n]).unwrap_or("内核故障遏制：已终止进程"),
        "系统已自动恢复，无需处理。若同一应用反复出现此问题，请更新或卸载该应用；详情见 /data/crash 崩溃报告（shell: crash）",
    );
    let elr = unsafe { read_sepc() };
    let fi = crate::crash::FaultInfo {
        kind: "S态故障(遏制)",
        pid,
        name: crate::sched_cfs::current_task_name(),
        esr: cause,
        elr,
        far: tval,
    };
    crate::crash::el0_fault(&fi, frame as *const u64);
    // SIGSEGV 语义终止：内部屏蔽 IRQ 并切换任务，不返回
    crate::process::exit(-11)
}

/// 内核线程故障遏制：非 syscall 上下文的 S 态异常/panic，但当前任务是可
/// 调度的内核线程（wifi/shell/perflog 等）时，只终止该线程保全系统。
///
/// 这是比看门狗重启温和一级的恢复：损失该内核线程承载的功能（如无线），
/// 系统与其余功能继续运行；tombstone + 健康事件告知用户。线程持有的锁
/// 不释放（fs_lock 有死亡持有者自愈，其余锁由后续取锁者兜底）。
///
/// 返回 false 表示无法遏制（idle/无任务上下文），调用方走整机诊断。
#[no_mangle]
pub extern "C" fn kthread_fault_contain(kind: &str, cause: u64, tval: u64) -> bool {
    if !crate::sched_cfs::is_sched_kthread() {
        return false;
    }
    let pid = crate::sched_cfs::current_pid();
    uart_send_str("[contain] 内核线程故障遏制：");
    uart_send_str(kind);
    uart_send_str(" pid=");
    print_dec(pid as u64);
    uart_send_str(" name=");
    uart_send_str(crate::sched_cfs::current_task_name());
    uart_send_str(" scause=");
    print_hex(cause);
    uart_send_str(" stval=");
    print_hex(tval);
    uart_send_str("（终止该线程，系统继续运行）\n");
    // tombstone（best-effort；递归保护内置）
    let fi = crate::crash::FaultInfo {
        kind: "内核线程故障(遏制)",
        pid,
        name: crate::sched_cfs::current_task_name(),
        esr: cause,
        elr: unsafe { read_sepc() },
        far: tval,
    };
    crate::crash::kernel_panic(&fi);
    crate::health::report_irq(
        true,
        crate::health::SEV_CRITICAL,
        "kthread",
        "内核后台线程故障已停止（对应功能停摆，系统其余功能正常）",
        "相关功能（如无线/音频/日志之一）不可用；请重启设备恢复；反复出现请查看 /data/crash",
    );
    // 清睡眠登记后标记退出并切走（不返回；栈在槽位复用时回收）
    unsafe { crate::sched_cfs::sleepers_clear_tid(crate::sched_cfs::current_tid()) };
    crate::sched_cfs::exit_current()
}

// ==================== IRQ 故障遏制（帧展开） ====================

// 展开汇编：a0 = 外层中断帧指针。把 sp 重指到该帧后复用 trap_return 的
// 完整恢复逻辑（GPR/FP 恢复、sscratch 约定、SPP 分流、PC 护栏）——
// 效果等于「这次中断从未发生过」：handler 的半成品工作被放弃，被打断
// 的上下文（用户代码或内核代码）原地恢复执行。
core::arch::global_asm!(
    ".section .text.trap_entry, \"ax\"",
    ".balign 4",
    ".global irq_fault_unwind",
    ".type irq_fault_unwind, %function",
    "irq_fault_unwind:",
    "    mv sp, a0",
    "    j trap_return",
);

extern "C" {
    fn irq_fault_unwind(frame: *mut crate::syscall_table::TrapFrame) -> !;
}

/// IRQ 故障遏制入口（`trap_dispatch` 在进程遏制之前调用）。
///
/// 判定：当前 CPU 处于中断处理上下文（`interrupt::irq_ctx_frame` 非零）——
/// 即 S 态异常来自中断 handler 自身（驱动 bug / 半更新硬件状态），没有可
/// 归咎的用户进程。遏制动作：
///   - 外部中断（有线号）：PLIC 屏蔽该线——故障源从此静默，其余中断正常；
///   - 定时器：无法屏蔽（调度依赖），仅展开；累计超过上限（系统性 bug）
///     返回 false 转为整机诊断；
///   - 健康事件 + 控制台告知用户；
///   - 帧展开恢复被打断的上下文（不返回）。
///
/// 已知取舍：handler 持有的锁不释放（同进程遏制语义）；PLIC 的本次
/// claim 不再 complete（该线已被屏蔽，无重投递）。
#[no_mangle]
pub extern "C" fn irq_fault_contain(
    _frame: *mut crate::syscall_table::TrapFrame,
    cause: u64,
    tval: u64,
) -> bool {
    let outer = crate::interrupt::irq_ctx_frame();
    if outer == 0 {
        return false; // 非中断上下文：交由进程遏制/整机诊断
    }
    let line = crate::interrupt::irq_ctx_line();
    let kind = crate::interrupt::irq_ctx_kind();
    // 定时器故障：无法屏蔽，计数限流，超限转整机诊断（系统性 bug）
    if kind == 5 && !crate::interrupt::timer_contain_tick() {
        return false;
    }
    uart_send_str("[contain] IRQ 故障遏制：");
    if line != 0 {
        print_dec(line as u64);
    } else if kind == 5 {
        uart_send_str("定时器");
    } else {
        uart_send_str("中断 cause=");
        print_dec(kind as u64);
    }
    uart_send_str(" 处理中故障 scause=");
    print_hex(cause);
    uart_send_str(" stval=");
    print_hex(tval);
    if line != 0 {
        crate::plic::plic_disable_irq(line);
        uart_send_str("（已屏蔽该 IRQ 线，恢复被打断的任务）\n");
        crate::health::report_irq(
            true,
            crate::health::SEV_WARN,
            "interrupt",
            "某设备中断处理故障，对应功能已停用（其余功能正常）",
            "相关硬件功能（如输入/网络/音频之一）可能失效；请保存工作后重启设备恢复",
        );
    } else {
        uart_send_str("（放弃本次中断处理，恢复被打断的任务）\n");
        crate::health::report_irq(
            true,
            crate::health::SEV_WARN,
            "interrupt",
            "定时器/中断公共路径故障，本次中断已跳过",
            "若系统出现卡顿或时钟异常，请重启设备",
        );
    }
    // 清标记后展开（不返回）
    crate::interrupt::irq_ctx_reset();
    unsafe { irq_fault_unwind(outer as *mut crate::syscall_table::TrapFrame) }
}

/// 同步异常处理入口：读取 ESR_EL1 / ELR_EL1 后打印并死循环（等效 panic）。
///
/// 用于捕获 MMU 配置错误等引起的 data/prefetch abort。
#[no_mangle]
extern "C" fn sync_exception_handler() -> ! {
    // 防递归：入口屏蔽 IRQ 并记录首次故障，避免 panic 处理自身再次
    // 触发异常（用户栈区 sp 等损坏状态会无限递归刷屏）
    unsafe { asm!("csrci sstatus, 2") };
    static mut FIRST: bool = true;
    if unsafe { !FIRST } {
        let esr = unsafe { read_scause() };
        uart_send_str("PANIC（重入）scause=");
        print_hex(esr);
        uart_send_str("\n");
        loop {
            unsafe { wfi() };
        }
    }
    unsafe { FIRST = false };
    // 读取异常原因与故障地址（Sv39：scause/stval/sepc）
    let esr = unsafe { read_scause() };
    let elr = unsafe { read_sepc() };
    let far = unsafe { read_stval() };
    uart_send_str("PANIC: 同步异常 scause=");
    print_hex(esr);
    uart_send_str(" sepc=");
    print_hex(elr);
    uart_send_str(" stval=");
    print_hex(far);
    uart_send_str(" pid=");
    crate::print_dec(crate::sched_cfs::current_pid() as u64);
    uart_send_str(" tid=");
    crate::print_dec(crate::sched_cfs::current_tid() as u64);
    uart_send_str(" name=");
    uart_send_str(crate::sched_cfs::current_task_name());
    uart_send_str(" sp=");
    let sp: u64;
    unsafe { asm!("mv {}, sp", out(reg) sp) };
    print_hex(sp);
    // 故障点通用寄存器（sync_entry 汇编入口保存，未被 print 污染）
    uart_send_str("\n[regs] ");
    for i in 0..31usize {
        let v = unsafe { PANIC_REGS[i] };
        print_hex(v);
        uart_send_str(" ");
    }
    uart_send_str("\n");
    // 栈回溯（前 64 个 8 字节）：找返回地址（x30 附近）
    uart_send_str("\n[stack] ");
    for i in 0..64usize {
        let v = unsafe { *((sp as *const u64).add(i)) };
        print_hex(v);
        uart_send_str(" ");
    }
    // 栈扫描：内核文本区（.text 0x40080000..0x400c3000）内的字=可能的返回地址，
    // 按出现顺序输出，直接给出 memcpy 等故障点的调用链。
    uart_send_str("\n[ret-scan] ");
    for i in 0..256usize {
        let v = unsafe { *((sp as *const u64).add(i)) };
        if (0x8020_0000usize..crate::kernel_end()).contains(&(v as usize)) {
            uart_send_str("ret?=");
            print_hex(v);
            uart_send_str(" ");
        }
    }
    // 任务表转储：定位损坏的 TCB（elr/x30/sp 为 0 或越界即嫌疑）
    uart_send_str("\n[tasks]\n");
    crate::sched_cfs::dump_tasks();
    // [crash] 生成并落盘 tombstone（best-effort）
    let fi = crate::crash::FaultInfo {
        kind: "SYNC-EXC",
        pid: crate::sched_cfs::current_pid(),
        name: crate::sched_cfs::current_task_name(),
        esr,
        elr,
        far,
    };
    crate::crash::kernel_panic(&fi);
    // 死循环
    loop {
        unsafe {
            wfi();
        }
    }
}

/// FIQ / SError / SP0 / AArch32 等未接线的异常入口：打印诊断后死循环。
/// 由汇编 `unknown_fault_entry` 调用（无保存寄存器，仅读系统寄存器）。
#[no_mangle]
extern "C" fn unknown_exception_handler() -> ! {
    let esr = unsafe { read_scause() };
    let elr = unsafe { read_sepc() };
    let far = unsafe { read_stval() };
    uart_send_str("PANIC: 未处理异常 scause=");
    crate::print_hex(esr);
    uart_send_str(" ELR=");
    crate::print_hex(elr);
    uart_send_str(" FAR=");
    crate::print_hex(far);
    uart_send_str(" pid=");
    crate::print_dec(crate::sched_cfs::current_pid() as u64);
    uart_send_str(" name=");
    uart_send_str(crate::sched_cfs::current_task_name());
    uart_send_str("\n");
    loop {
        unsafe { wfi() };
    }
}

/// EL0 用户程序故障处理器：打印故障信息后终止当前进程（SIGSEGV 语义），
/// 由汇编 `el0_fault` 入口调用（陷阱帧已保存在内核栈上，sp 传入）。
#[no_mangle]
extern "C" fn el0_fault_handler(frame: *mut u8) -> ! {
    let esr = unsafe { read_scause() };
    let elr = unsafe { read_sepc() };
    let pid = crate::sched_cfs::current_pid();
    // [crash] 生成并落盘 tombstone（SIGSEGV/SIGILL/SIGTRAP 语义，
    // 含寄存器/内存映射/栈回溯/日志快照），随后继续打印详细诊断。
    {
        let far = unsafe { read_stval() };
        let sig = crate::crash::esr_describe(esr).0;
        let fi = crate::crash::FaultInfo {
            kind: crate::crash::signal_name(sig),
            pid,
            name: crate::sched_cfs::current_task_name(),
            esr,
            elr,
            far,
        };
        crate::crash::el0_fault(&fi, frame as *const u64);
    }
    if pid < crate::process::MAX_PROC as u32 {
        let root = crate::process::processes_pub_ttbr0(pid);
        let mapped = crate::vm::user_range_mapped(root, elr as usize, 4096);
        let rec = crate::process::processes_pub_find(pid, elr as usize);
        kinfo! {
            crate::uart_send_str("[fault] pid=");
            crate::print_dec(pid as u64);
            crate::uart_send_str(" elr=");
            crate::print_hex(elr);
            crate::uart_send_str(" mapped=");
            crate::print_dec(mapped as u64);
            crate::uart_send_str(" rec=");
            crate::print_hex(rec as u64);
            crate::uart_send_str("\n");
        }
    }

    // [诊断] 跨进程物理页重复分配检查（双重分配 → 系统级损坏根源）；
    // [诊断] 所有任务内核栈哨兵检查（栈溢出 → 破坏位图 → 分配重叠）。
    // 检查本身必须在安静模式下也执行：失败时的错误输出不受门控。
    crate::process::check_double_allocations();
    crate::sched_cfs::check_all_stack_canaries();

    // 以下为详细诊断转储（寄存器/物理页/系统调用轨迹/用户栈/回溯），
    // 安静模式（默认）下不上串口；完整信息见 /var/crash tombstone。
    kinfo! {
    // 转储陷阱帧中的 sp_el0/elr/spsr（frame 指向栈上的 TrapFrame）
    let (f_sp, f_elr, f_spsr) = unsafe {
        let p = frame as *const u64;
        // x[2](用户 sp)@2, elr@33, spsr@34（新布局：x[0..32] + 兼容位 + elr + spsr）
        (*p.add(2), *p.add(33), *p.add(34))
    };
    uart_send_str("\n[el0-fault frame] sp(x2)=");
    print_hex(f_sp);
    uart_send_str(" elr=");
    print_hex(f_elr);
    uart_send_str(" sstatus=");
    print_hex(f_spsr);

    // 关键调用寄存器：s0(x8,fp)/ra(x1)/a0(x10)/a1(x11)/a7(x17)（定位跳转 0
    // 的调用点；riscv 帧指针为 s0，返回地址为 ra）
    let (x29, x30, x0, x1, x8) = unsafe {
        let p = frame as *const u64;
        (*p.add(8), *p.add(1), *p.add(10), *p.add(11), *p.add(8))
    };
    uart_send_str(" fp(x8)=");
    print_hex(x29);
    uart_send_str(" ra(x1)=");
    print_hex(x30);
    uart_send_str(" a0(x10)=");
    print_hex(x0);
    uart_send_str(" a1(x11)=");
    print_hex(x1);
    let (x19, x20, x21, x22, x23) = unsafe {
        let p = frame as *const u64;
        (*p.add(17), *p.add(5), *p.add(6), *p.add(2), *p.add(3))
    };
    uart_send_str(" a7(x17)=");
    print_hex(x19);
    uart_send_str(" t0(x5)=");
    print_hex(x20);
    uart_send_str(" t1(x6)=");
    print_hex(x21);
    uart_send_str(" sp(x2)=");
    print_hex(x22);
    uart_send_str(" gp(x3)=");
    print_hex(x23);

    // stval：data abort 时是故障访问地址（定位越界写目标）
    let far = unsafe { read_stval() };
    uart_send_str(" stval=");
    print_hex(far);

    // 诊断：用户栈 VA 与帧缓冲映射 VA 的物理页（核对栈/显存物理页是否重叠）
    let root = crate::process::current_root();
    uart_send_str(" [phys] sp=");
    print_hex(crate::vm::user_va_to_pa(root, f_sp as usize) as u64);
    uart_send_str(" fb@0x20000000=");
    print_hex(crate::vm::user_va_to_pa(root, 0x2000_0000) as u64);
    uart_send_str(" far=");
    print_hex(crate::vm::user_va_to_pa(root, far as usize) as u64);
    uart_send_str(" bss@0x406000=");
    print_hex(crate::vm::user_va_to_pa(root, 0x4060_00) as u64);
    uart_send_str(" bss@0x409000=");
    print_hex(crate::vm::user_va_to_pa(root, 0x4090_00) as u64);
    uart_send_str(" bss@0x410000=");
    print_hex(crate::vm::user_va_to_pa(root, 0x4100_00) as u64);

    // 最近一次系统调用的用户 PC 与用户 sp（核对「sp 被 syscall 返回错位」）
    let (ls_nr, ls_elr, ls_sp) = crate::linux_compat::last_syscall();
    uart_send_str(" [last-syscall] nr=");
    crate::print_dec(ls_nr as u64);
    uart_send_str(" elr=");
    print_hex(ls_elr);
    uart_send_str(" sp=");
    print_hex(ls_sp);
    uart_send_str("\n");
    crate::linux_compat::dump_sys_trace();

    // 用户栈转储：故障点附近 [-256, +128) 的内容（定位「已保存 LR 被清零」
    // 类损坏；页未映射的部分跳过）。边界用 ASLR 随机化后的实际栈区。
    let root = crate::process::current_root();
    let ustack_base = crate::elf::current_stack_base() as u64;
    let ustack_top = crate::elf::current_stack_top() as u64;
    uart_send_str(" [user-stack]");
    let start = f_sp.saturating_sub(256);
    unsafe { crate::mmu::uaccess_begin() };
    for off in (0..(384usize)).step_by(8) {
        let va = start + off as u64;
        if va + 8 <= ustack_top
            && va >= ustack_base
            && crate::vm::user_range_mapped(root, va as usize, 8)
        {
            let v = unsafe { *((va as usize) as *const u64) };
            uart_send_str(" ");
            print_hex(v);
        } else {
            uart_send_str(" ..");
        }
    }
    unsafe { crate::mmu::uaccess_end() };
    uart_send_str("\n");

    // 用户回溯：沿 s0 帧指针链（每帧 [fp]=prev_fp, [fp-8]=ra，riscv 布局）
    // 打印调用栈，定位「ret 到 0 / 垃圾地址」时崩溃函数的调用者。
    uart_send_str(" [user-bt]");
    let mut fp = x29;
    let mut steps = 0;
    unsafe { crate::mmu::uaccess_begin() };
    while steps < 16 {
        if fp < ustack_base
            || fp + 16 > ustack_top
            || !crate::vm::user_range_mapped(root, fp as usize, 16)
            || (fp % 8) != 0
        {
            break;
        }
        let prev_fp = unsafe { *(fp as *const u64) };
        let lr = unsafe { *((fp - 8) as *const u64) };
        uart_send_str(" [fp=");
        print_hex(fp);
        uart_send_str(" lr=");
        print_hex(lr);
        uart_send_str("]");
        if prev_fp <= fp || prev_fp > ustack_top {
            break;
        }
        fp = prev_fp;
        steps += 1;
    }
    unsafe { crate::mmu::uaccess_end() };
    uart_send_str("\n");

    // 内核栈溢出检测：栈底 16 字节应为 0xA5 填充模式
    let (sb, st) = crate::sched_cfs::current_stack();
    if sb >= 0x8000_0000 && st > sb {
        let mut clobbered = 0usize;
        for off in (0..64usize).step_by(8) {
            let v = unsafe { *((sb + off) as *const u64) };
            if v != 0xA5A5A5A5A5A5A5A5 {
                clobbered += 1;
            }
        }
        uart_send_str(" [stack-canary] bottom_clobbered_words=");
        crate::print_dec(clobbered as u64);
        uart_send_str(" base=");
        print_hex(sb as u64);
        uart_send_str(" top=");
        print_hex(st as u64);
        uart_send_str("\n");
        // 转储栈顶下方 512 字节（陷阱帧 + 可能的 irq 保存区，定位覆写来源）
        if st >= 512 {
            uart_send_str("[stack-top-dump] ");
            for i in (0..512usize).step_by(8) {
                let v = unsafe { *((st - 512 + i) as *const u64) };
                print_hex(v);
                uart_send_str(" ");
            }
            uart_send_str("\n");
        }
    }

    crate::sched_cfs::dump_tasks();
    } // kinfo!（详细诊断转储结束）

    uart_send_str("[kernel] EL0 fault pid=");
    crate::print_dec(crate::process::current_pid() as u64);
    uart_send_str(" ESR=");
    print_hex(esr);
    uart_send_str(" ELR=");
    print_hex(elr);
    uart_send_str("（正在终止进程）\n");
    // 以 SIGSEGV（-11）语义退出；exit 内部屏蔽 IRQ 并切换任务，不返回。
    // 父进程/init 回收后按 inittab 策略重启，用户程序崩溃不再挂死内核。
    crate::process::exit(-11)
}

/// EL0 data abort 处理器（由 `lower_sync` 汇编在 EC==0x24 时调用）。
///
/// 读取 FAR_EL1（故障地址）与 ESR_EL1 的 DFSC（故障状态码）：
/// - **translation fault**（页未映射）：按需分页落地 ——
///   若该页是 LAZY 预留页则分配物理页 + 清零 + 建映射，返回 `true`；
/// - **permission fault**（写只读页）：COW 写时复制 ——
///   若该页是 COW 共享页则 break 出私有可写副本，返回 `true`；
/// - 其余情况返回 `false`，汇编走 `el0_fault_handler` 终止进程（SIGSEGV）。
///
/// DFSC 的 bits[3:1] 编码故障所在页表 level、bit4 为 TTL 标志：匹配时先
/// `& 0b111001` 掩掉 level/TTL 位，只保留故障类型（实测 EL0 写 COW 页报
/// permission fault level 1 = 0b001111，未掩位时与 0b001101 不匹配导致
/// COW 永不触发、进程被误杀）。
///
/// 汇编在返回 `true` 时恢复陷阱帧并 `eret` 重试故障指令。
/// 本函数在 IRQ 屏蔽态下运行（lower_sync 入口已 `daifset`），期间不允许被
/// 抢占，因此读 ESR/FAR 与落地是原子的。
#[no_mangle]
extern "C" fn el0_page_fault_handler(_frame: *mut u8, stval: usize) -> bool {
    // stval：缺页故障访问地址（interrupt.rs 从 stval 寄存器传入）
    let va = stval;
    if !crate::vm::user_va_range_ok(va, 1) {
        return false;
    }
    // Sv39 缺页成因（scause 12/13/15：取指/读/写缺页）不区分 translation 与
    // permission（后者以 access-fault 编码）：全部先试 LAZY 预留页落地
    // （mmap 匿名区按需分页），再试 COW，均失败才 SIGSEGV。wild 指针因无
    // 记录自然落到 SIGSEGV 分支，语义不变。
    if crate::process::page_fault_resolve(va) {
        true
    } else if crate::process::cow_break(va) {
        true
    } else {
        // [diag] 落地失败诊断：musl 静态程序 mmap 首写崩溃排查用
        // （mtest：va=0x11000000 且记录应在，却 pid=0 查空——见记忆）。
        let (tid, pid) = (crate::sched_cfs::current_tid(), crate::sched_cfs::current_pid());
        crate::uart_send_str("[pf-diag] va=");
        crate::print_hex(va as u64);
        crate::uart_send_str(" tid=");
        crate::print_dec(tid as u64);
        crate::uart_send_str(" pid=");
        crate::print_dec(pid as u64);
        crate::uart_send_str(" recs=");
        crate::print_dec(crate::process::processes_pub_record_count(pid) as u64);
        crate::uart_send_str("\n");
        false
    }
}

// ==================== FP 现场保存/恢复（inline asm! 辅助） ====================
//
// global_asm! 在 opt-level>0 时不接受 FP 指令（rustc 内建汇编器不随
// -Ctarget-feature 开 D 扩展，LTO/优化路径一致拒绝 fsd/fld），而 inline
// asm! 经 LLVM 代码生成、继承 riscv64gc 全部扩展。陷阱帧 FP 区布局：
// f0-f31 @0x120..0x320，fcsr @0x320。

/// 保存 f0-f31 + fcsr 到陷阱帧 FP 区（`p` = 帧基址）。
#[no_mangle]
pub unsafe extern "C" fn trap_save_fp(p: *mut u8) {
    asm!(
        "fsd f0, 0x120({p})",
        "fsd f1, 0x128({p})",
        "fsd f2, 0x130({p})",
        "fsd f3, 0x138({p})",
        "fsd f4, 0x140({p})",
        "fsd f5, 0x148({p})",
        "fsd f6, 0x150({p})",
        "fsd f7, 0x158({p})",
        "fsd f8, 0x160({p})",
        "fsd f9, 0x168({p})",
        "fsd f10, 0x170({p})",
        "fsd f11, 0x178({p})",
        "fsd f12, 0x180({p})",
        "fsd f13, 0x188({p})",
        "fsd f14, 0x190({p})",
        "fsd f15, 0x198({p})",
        "fsd f16, 0x1A0({p})",
        "fsd f17, 0x1A8({p})",
        "fsd f18, 0x1B0({p})",
        "fsd f19, 0x1B8({p})",
        "fsd f20, 0x1C0({p})",
        "fsd f21, 0x1C8({p})",
        "fsd f22, 0x1D0({p})",
        "fsd f23, 0x1D8({p})",
        "fsd f24, 0x1E0({p})",
        "fsd f25, 0x1E8({p})",
        "fsd f26, 0x1F0({p})",
        "fsd f27, 0x1F8({p})",
        "fsd f28, 0x200({p})",
        "fsd f29, 0x208({p})",
        "fsd f30, 0x210({p})",
        "fsd f31, 0x218({p})",
        "csrr {t}, fcsr",
        "sd {t}, 0x320({p})",
        p = in(reg) p,
        t = out(reg) _,
        options(nostack),
    );
}

/// 从陷阱帧 FP 区恢复 f0-f31 + fcsr（`p` = 帧基址）。
#[no_mangle]
pub unsafe extern "C" fn trap_restore_fp(p: *mut u8) {
    asm!(
        "ld {t}, 0x320({p})",
        "csrw fcsr, {t}",
        "fld f0, 0x120({p})",
        "fld f1, 0x128({p})",
        "fld f2, 0x130({p})",
        "fld f3, 0x138({p})",
        "fld f4, 0x140({p})",
        "fld f5, 0x148({p})",
        "fld f6, 0x150({p})",
        "fld f7, 0x158({p})",
        "fld f8, 0x160({p})",
        "fld f9, 0x168({p})",
        "fld f10, 0x170({p})",
        "fld f11, 0x178({p})",
        "fld f12, 0x180({p})",
        "fld f13, 0x188({p})",
        "fld f14, 0x190({p})",
        "fld f15, 0x198({p})",
        "fld f16, 0x1A0({p})",
        "fld f17, 0x1A8({p})",
        "fld f18, 0x1B0({p})",
        "fld f19, 0x1B8({p})",
        "fld f20, 0x1C0({p})",
        "fld f21, 0x1C8({p})",
        "fld f22, 0x1D0({p})",
        "fld f23, 0x1D8({p})",
        "fld f24, 0x1E0({p})",
        "fld f25, 0x1E8({p})",
        "fld f26, 0x1F0({p})",
        "fld f27, 0x1F8({p})",
        "fld f28, 0x200({p})",
        "fld f29, 0x208({p})",
        "fld f30, 0x210({p})",
        "fld f31, 0x218({p})",
        p = in(reg) p,
        t = out(reg) _,
        options(nostack),
    );
}

// ==================== 启动/陷入汇编（RISC-V Sv39 + S 模式） ====================

global_asm!(
    r#"
    // ============================================================
    // 启动代码：OpenSBI fw_dynamic 以 S 模式进入
    //   a0 = hartid，a1 = DTB 物理地址，MMU 关（satp=0）
    // ============================================================
    .section .text._start, "ax"
    .global _start
    .type _start, %function
_start:
    // sscratch 停泊约定：用户态运行时 = 下次用户陷入用的内核栈指针；
    // 内核态运行时 = 0（陷入入口据此区分来源，见 trap_entry）
    csrw sscratch, zero
    // 非 boot hart（OpenSBI 把所有 hart 都带到这）→ 停车场等待调度就绪
    bnez a0, park
    mv tp, a0                    // tp = hartid（cpu_id 读此寄存器）
    // 保存 DTB 地址到全局（在 BSS 清零之后写入才有效——先清 BSS 再写）
    // 关 MMU（防御 OpenSBI 遗留 satp；清 satp 后必须 sfence）
    csrw satp, zero
    sfence.vma
    // 引导栈：链接脚本 .bootstack 段尾（栈向下生长）
    la sp, _stack_top
    // 清零 BSS（sd 对齐推进；_bss_start/_bss_end 均 8 字节对齐由链接脚本保证）
    la t0, _bss_start
    la t1, _bss_end
1:
    bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b
2:
    // 写入 DTB 地址（BSS 已清零）
    la t0, DTB_ADDR
    sd a1, 0(t0)
    // FPU 使能（sstatus.FS=Dirty）：复位后 FS=Off，任何浮点指令触发
    // illegal instruction；Rust 编译器可能生成浮点/向量访问
    li t0, 0x6000
    csrs sstatus, t0
    // SUM|MXR：S 模式可访问 U 页 / 读仅执行页（内核直读用户内存）
    li t0, 0xC0000
    csrs sstatus, t0
    // ★ 每中断源使能：SSIE|STIE|SEIE（sstatus.SIE 只是总闸）。此前漏开
    // 导致监督定时器与 PLIC 外部中断从不投递 —— 系统纯协作式裸奔（无
    // 抢占、无定时唤醒、设备全靠轮询）。
    li t0, 0x222
    csrs sie, t0
    // 统一陷入入口
    la t0, trap_entry
    csrw stvec, t0
    // 屏蔽中断，进入 Rust 主体
    csrci sstatus, 2
    call rust_main
    // rust_main 不应返回
3:  wfi
    j 3b

    // ============================================================
    // secondary hart 停车场：等待主核置 SCHED_READY 后进调度器
    // ============================================================
    .section .text._start
    .global park
    .type park, %function
park:
    mv tp, a0                    // tp = hartid
    la t0, trap_entry
    csrw stvec, t0
    // FPU + SUM/MXR（与 _start 一致；satp 由 smp_secondary_main→mmu::init_secondary 配置）
    li t0, 0x6000
    csrs sstatus, t0
    li t0, 0xC0000
    csrs sstatus, t0
    // 每中断源使能（与 _start 一致，见彼处注释）
    li t0, 0x222
    csrs sie, t0
    csrci sstatus, 2
1:
    la t0, SCHED_READY           // AtomicBool（1 字节）
    lbu t1, 0(t0)
    beqz t1, 2f
    // 就绪：per-cpu 栈（SECONDARY_STACKS + hartid*16KB，栈顶 = 区段尾）
    la t0, SECONDARY_STACKS
    slli t1, a0, 14              // t1 = hartid * 16384
    add t0, t0, t1
    li t1, 16384                 // addi 立即数限 12 位，16384 经 li 中转
    add t0, t0, t1               // SECONDARY_STACKS + hartid*16KB + 16KB（栈顶）
    andi t0, t0, -16             // SP 16 字节向下对齐（riscv ABI）
    mv sp, t0
    csrw sscratch, zero
    mv a0, tp
    call smp_secondary_main
3:  wfi
    j 3b
2:  wfi
    j 1b

    // ============================================================
    // 统一陷入入口（stvec 指向此处，4 字节对齐）
    //
    // 陷阱帧（832 字节 = 0x340，16 对齐，位于当前任务内核栈）：
    //   x[0..32]   @0x000..0x100（x0 恒 0 占位；x2 = 陷阱前 sp）
    //   sp_el0     @0x100（兼容占位，恒 0）
    //   elr(sepc)  @0x108
    //   spsr(sstatus) @0x110
    //   f[0..32]   @0x120..0x320（f0-f31）
    //   fcsr       @0x320
    // FP 在陷入入口保存、返回前恢复（内核可能用浮点，防止污染用户现场）。
    // ============================================================
    .section .text.trap_entry, "ax"
    .balign 4
    .global trap_entry
    .type trap_entry, %function
trap_entry:
    // sp ↔ sscratch 交换：来自用户 → sp=停泊内核栈、sscratch=用户 sp；
    // 来自内核（嵌套 IRQ 等）→ sscratch=0，交换后 sp=0
    csrrw sp, sscratch, sp
    bnez sp, 1f
    csrr sp, sscratch            // 内核来源：恢复陷阱前内核 sp
1:
    addi sp, sp, -832
    // 保存通用寄存器 x1..x31（x2 槽稍后由 sscratch 补）
    sd x1, 8(sp)
    sd x3, 24(sp)
    sd x4, 32(sp)
    sd x5, 40(sp)
    sd x6, 48(sp)
    sd x7, 56(sp)
    sd x8, 64(sp)
    sd x9, 72(sp)
    sd x10, 80(sp)
    sd x11, 88(sp)
    sd x12, 96(sp)
    sd x13, 104(sp)
    sd x14, 112(sp)
    sd x15, 120(sp)
    sd x16, 128(sp)
    sd x17, 136(sp)
    sd x18, 144(sp)
    sd x19, 152(sp)
    sd x20, 160(sp)
    sd x21, 168(sp)
    sd x22, 176(sp)
    sd x23, 184(sp)
    sd x24, 192(sp)
    sd x25, 200(sp)
    sd x26, 208(sp)
    sd x27, 216(sp)
    sd x28, 224(sp)
    sd x29, 232(sp)
    sd x30, 240(sp)
    sd x31, 248(sp)
    sd zero, 0(sp)               // x0 = 0
    // x2 = 陷阱前 sp（csrrw 后保存在 sscratch）；sp_el0 槽同步镜像
    // （信号投递/rt_sigreturn 以 sp_el0 槽为用户 sp 的权威槽位）
    csrr t0, sscratch
    sd t0, 16(sp)
    sd t0, 0x100(sp)
    // 保存 FP 现场（f0-f31 + fcsr @0x120..0x328）：经 Rust 辅助函数
    // trap_save_fp（inline asm!）—— global_asm 在 opt>0 下不接受 FP 指令
    // （rustc 汇编器不随 -Ctarget-feature 打开 D 扩展），inline asm! 走
    // LLVM 代码生成不受此限。此时全部 GPR 已入帧，caller-saved 随便压。
    mv a0, sp
    call trap_save_fp
    // 保存 CSR 现场
    csrr t0, sepc
    sd t0, 0x108(sp)
    csrr t0, sstatus
    sd t0, 0x110(sp)
    // 标记内核态（嵌套陷阱走「来自内核」路径）
    csrw sscratch, zero
    // 进入 Rust 分发（正常路径返回后落到 trap_return）
    mv a0, sp
    call trap_dispatch

    // ===== 统一恢复点 =====
    // 该标签同时作为 fork/clone 子任务/子线程的首次调度入口：子任务的
    // TaskContext 把 sp 指向其**拷贝的陷阱帧**、ra 指向此处，从而绕过
    // trap_dispatch —— 子帧 a0 已被 fork/clone 置 0，elr/sstatus/x2 均
    // 为其自身的副本值，sret 直接回到用户态 fork()/clone() 调用点。
    .global syscall_return_restore
    .global trap_return
    .type trap_return, %function
trap_return:
syscall_return_restore:
    // ★ 恢复/sret 全程屏蔽中断：syscall 体（dispatch 前）已放开抢占，若
    // 定时器恰在「写 sepc → sret」窗口抢占本任务，switch_to 会把恢复点
    // 存进任务上下文；恢复时 sret 跳回该内核地址 → U 模式执行内核代码。
    // sret 按 SPIE 恢复 IE（用户态自动开中断），此处屏蔽不吞用户中断。
    csrci sstatus, 2
    // ---- PC 护栏（仅用户来源）：elr 非 0 且低于内核恒等映射起点 ----
    ld t0, 0x110(sp)             // sstatus
    andi t1, t0, 0x100           // SPP=bit8（bit1 是 SIE！）：1 = 来自内核
    bnez t1, 2f
    ld t0, 0x108(sp)             // elr
    beqz t0, 3f
    li t1, 0x80000000
    bgeu t0, t1, 3f
    // ---- sstatus 白名单清洗（用户来源）----
    // rt_sigreturn 等路径的帧内容用户可控：只保留 FS（浮点状态），强制
    // SPP=0（回 U 模式）、SPIE=1（sret 后开中断）、SUM|MXR=1（内核语义），
    // 其余位（XS/UBE 等伪装特权位）清零。
    ld t0, 0x110(sp)
    li t1, 0x6000                // FS 掩码
    and t0, t0, t1
    li t1, 0xC0020               // SUM|MXR|SPIE
    or t0, t0, t1
    csrw sstatus, t0
    j 4f
2:  // 内核来源：帧内 sstatus 原样回写（SPP=1 → sret 回 S 模式，SPIE 保留
    // 陷阱前的关中断状态）
    ld t0, 0x110(sp)
    csrw sstatus, t0
    j 4f
3:  // 护栏失败：elr 为 0 / 内核地址 → 干净终止当前进程（不 eret 到垃圾 PC）
    mv a0, sp
    call el0_fault_handler
    j .
4:
    // sepc
    ld t0, 0x108(sp)
    csrw sepc, t0
    // FP 恢复：经 Rust 辅助函数 trap_restore_fp（inline asm!，理由同保存侧；
    // 此时尚未从帧恢复 GPR，caller-saved 压掉无妨）
    mv a0, sp
    call trap_restore_fp
    // ---- 分流出口 ----
    ld t0, 0x110(sp)
    andi t0, t0, 0x100           // SPP=bit8
    bnez t0, kexit               // 内核来源出口
    // 用户出口：先恢复全部通用寄存器（x2 除外），再经 sscratch 中转装回
    // 用户 sp —— RISC-V 的 x2 就是 sp：若直接 `ld x2, 0x100(sp)` 后弹帧，
    // addi 会加到刚装载的用户 sp 上（用户 sp 恒 +832 的根因）。
    ld x1, 8(sp)
    ld x3, 24(sp)
    ld x4, 32(sp)
    ld x5, 40(sp)
    ld x6, 48(sp)
    ld x7, 56(sp)
    ld x8, 64(sp)
    ld x9, 72(sp)
    ld x10, 80(sp)
    ld x11, 88(sp)
    ld x12, 96(sp)
    ld x13, 104(sp)
    ld x14, 112(sp)
    ld x15, 120(sp)
    ld x16, 128(sp)
    ld x17, 136(sp)
    ld x18, 144(sp)
    ld x19, 152(sp)
    ld x20, 160(sp)
    ld x21, 168(sp)
    ld x22, 176(sp)
    ld x23, 184(sp)
    ld x24, 192(sp)
    ld x25, 200(sp)
    ld x26, 208(sp)
    ld x27, 216(sp)
    ld x28, 224(sp)
    ld x29, 232(sp)
    ld x30, 240(sp)
    ld x31, 248(sp)
    // ★ 用户 sp 装回序列（x2 == sp，禁止常规 ld+addi 组合）：
    //   1) t0 = 帧内用户 sp，暂存 sscratch；
    //   2) 弹帧（sp = 下一轮用户陷阱的停泊值）；
    //   3) 从帧残留内存补恢复 t0 的用户值（x5 槽 = 40-832 = -792）；
    //   4) csrrw 交换：sp ← 用户 sp，sscratch ← 停泊内核 sp（一次完成）。
    ld t0, 0x100(sp)
    csrw sscratch, t0
    addi sp, sp, 832
    ld t0, -792(sp)               // x5 槽 = 帧基+40（40-832=-792；-784 是 x6 槽）
    csrrw sp, sscratch, sp
    sret
kexit:
    // 内核出口：sscratch 回 0（内核标记），sp 不动（x2 槽是陷阱前内核 sp）
    csrw sscratch, zero
    ld x1, 8(sp)
    ld x3, 24(sp)
    ld x4, 32(sp)
    ld x5, 40(sp)
    ld x6, 48(sp)
    ld x7, 56(sp)
    ld x8, 64(sp)
    ld x9, 72(sp)
    ld x10, 80(sp)
    ld x11, 88(sp)
    ld x12, 96(sp)
    ld x13, 104(sp)
    ld x14, 112(sp)
    ld x15, 120(sp)
    ld x16, 128(sp)
    ld x17, 136(sp)
    ld x18, 144(sp)
    ld x19, 152(sp)
    ld x20, 160(sp)
    ld x21, 168(sp)
    ld x22, 176(sp)
    ld x23, 184(sp)
    ld x24, 192(sp)
    ld x25, 200(sp)
    ld x26, 208(sp)
    ld x27, 216(sp)
    ld x28, 224(sp)
    ld x29, 232(sp)
    ld x30, 240(sp)
    ld x31, 248(sp)
    addi sp, sp, 832
    sret

    // ===== fork/clone 子任务首次调度入口 =====
    // 子任务的恢复点不是任何 switch 路径（thread_trampoline 只用于 spawn），
    // 须先释放调度锁（切换路径在 switch 前获取、由被切到任务的恢复点释放，
    // 见 sched_cfs::sched_unlock_from_asm），再进入 trap_return。
    .global el0_resume_entry
    .type el0_resume_entry, %function
el0_resume_entry:
    call sched_unlock_from_asm
    j trap_return
"#
);
