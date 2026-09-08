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
/// 拷贝）。这里在两端 16 字节对齐时用 AArch64 `ldp/stp` 16 字节宽通道，
/// 仅 8 字节对齐时用 `ldr/str x`，尾部与未对齐场景回退字节循环 ——
/// 任意对齐都正确，同时把大块拷贝降到接近 NEON 前的理论上限。
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut d = dest as usize;
    let mut s = src as usize;
    let end = d.saturating_add(n);
    // 两端 16 字节对齐：ldp/stp 对（每迭代 16B）
    if (d & 15) == 0 && (s & 15) == 0 {
        while end.saturating_sub(d) >= 16 {
            asm!(
                "ldp {v0}, {v1}, [{s}]",
                "add {s}, {s}, #16",
                "stp {v0}, {v1}, [{d}]",
                "add {d}, {d}, #16",
                v0 = out(reg) _,
                v1 = out(reg) _,
                s = inout(reg) s,
                d = inout(reg) d,
                options(nostack),
            );
        }
    }
    // 仅 8 字节对齐：ldr/str x（每迭代 8B）
    if (d & 7) == 0 && (s & 7) == 0 {
        while end.saturating_sub(d) >= 8 {
            asm!(
                "ldr {v}, [{s}]",
                "add {s}, {s}, #8",
                "str {v}, [{d}]",
                "add {d}, {d}, #8",
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

/// 字节填充：8B `str` 宽通道（页清零 / 画布填充走此路径）。
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
    if p & 15 == 0 {
        while end.saturating_sub(p) >= 16 {
            asm!(
                "stp {w}, {w}, [{p}]",
                "add {p}, {p}, #16",
                w = in(reg) word,
                p = inout(reg) p,
                options(nostack),
            );
        }
    }
    while end.saturating_sub(p) >= 8 {
        asm!(
            "str {w}, [{p}]",
            "add {p}, {p}, #8",
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
mod gic;
mod gpu_simple;
mod gesture;
mod heap;
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

// ==================== PL011 UART 寄存器 ====================

/// PL011 寄存器布局（偏移以 32 位字为单位）
#[repr(C)]
struct Pl011 {
    dr: u32,               // 0x000 数据寄存器（读=接收，写=发送）
    _ecr: u32,             // 0x004 错误清除 / 接收状态
    _reserved0: [u32; 4],  // 0x008 .. 0x014 保留
    fr: u32,               // 0x018 标志寄存器（TXFF / RXFE 等）
    _reserved1: u32,       // 0x01C 保留
    _ilpr: u32,            // 0x020 IrDA 低功耗寄存器
    ibrd: u32,             // 0x024 整数波特率除数
    fbrd: u32,             // 0x028 小数波特率除数
    lcr_h: u32,            // 0x02C 行控制寄存器
    cr: u32,               // 0x030 控制寄存器（使能 UART / TX / RX）
    ifls: u32,             // 0x034 FIFO 中断触发深度
    imsc: u32,             // 0x038 中断屏蔽置位 / 清除
    ris: u32,              // 0x03C 原始中断状态
    _mis: u32,             // 0x040 屏蔽后中断状态
    icr: u32,              // 0x044 中断清除
    _dmacr: u32,           // 0x048 DMA 控制
}

/// UART MMIO 基地址（由设备树解析得到，默认 QEMU virt 值）
static mut UART_BASE: usize = 0x0900_0000;

/// 获取 UART 设备引用
///
/// # 安全
/// `UART_BASE` 指向的 MMIO 地址处始终存在一个 PL011 设备。
fn uart() -> &'static mut Pl011 {
    unsafe { &mut *(UART_BASE as *mut Pl011) }
}

/// UART 标志寄存器位定义
const FR_TXFF: u32 = 1 << 5; // 发送 FIFO 满
const FR_RXFE: u32 = 1 << 4; // 接收 FIFO 空

/// UART 中断相关位定义
const IMSC_RXIM: u32 = 1 << 4; // 接收中断屏蔽位
const IMSC_RTIM: u32 = 1 << 6; // 接收超时中断屏蔽位
const ICR_RXIC: u32 = 1 << 4;  // 清除接收中断
const ICR_RTIC: u32 = 1 << 6;  // 清除接收超时中断

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
    unsafe { asm!("mrs {}, daif", out(reg) saved) };
    unsafe { asm!("msr daifset, #2") };
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
            "msr daif, {}",
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

// ==================== UART 驱动 ====================

/// 初始化 PL011：`base` 为 MMIO 基地址，`clock` 为 UART 时钟（Hz）。
/// 波特率 115200 8N1，使能 FIFO，使能接收中断。
fn uart_init(base: usize, clock: u32) {
    unsafe {
        UART_BASE = base;
    }
    let uart = uart();
    unsafe {
        // 先关闭 UART，才能安全修改寄存器
        write_volatile(&mut uart.cr, 0);

        // 波特率除数 = clock / (16 * baud)，小数部分 6 位
        let divisor = clock / (16 * 115200);
        let remainder = clock % (16 * 115200);
        let frac = (remainder * 64 + (16 * 115200) / 2) / (16 * 115200);
        write_volatile(&mut uart.ibrd, divisor);
        write_volatile(&mut uart.fbrd, frac);

        // 8N1：字长 8 位（WLEN=0b11），使能 FIFO（FEN）
        write_volatile(&mut uart.lcr_h, (0b11 << 5) | (1 << 4));

        // 使能接收中断（RXIM）与接收超时中断（RTIM）
        write_volatile(&mut uart.imsc, IMSC_RXIM | IMSC_RTIM);

        // 使能 UART（UARTEN=bit0）、发送（TXE=bit8）、接收（RXE=bit9）
        write_volatile(&mut uart.cr, (1 << 0) | (1 << 8) | (1 << 9));
    }
}

/// 阻塞发送单个字符
pub(crate) fn uart_send_char(c: u8) {
    kmsg_put(c);
    let uart = uart();
    // 等待发送 FIFO 非满（TXFF 位清零）
    while unsafe { read_volatile(&uart.fr) } & FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    unsafe {
        write_volatile(&mut uart.dr, c as u32);
    }
    // TTY/PTY 屏幕镜像：把串口输出同步到活动会话的 out_q（图形终端主侧读取）。
    // 未附接时是零开销布尔判断；附接后 GUI 终端因此能看到 Shell 提示符、回显
    // 与所有命令输出（包含 ANSI 光标/颜色序列）。
    crate::tty::echo_mirror(c);
}

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

/// 直接从 PL011 硬件接收 FIFO 读一个字节（绕过 IRQ/软件缓冲）。
/// 供 GDB 调试桩在屏蔽 IRQ 期间使用（此时 RX 中断不触发，软件缓冲不再增长）。
pub(crate) fn uart_read_hw() -> Option<u8> {
    let uart = uart();
    if unsafe { read_volatile(&uart.fr) } & FR_RXFE != 0 {
        return None; // FIFO 空
    }
    Some(unsafe { read_volatile(&uart.dr) } as u8)
}

/// 把 PL011 硬件接收 FIFO 全部排空到软件缓冲（供 GDB 调试桩使用：
/// 断点期间 IRQ 被屏蔽，RX 中断不触发，必须主动轮询排空 16 字节硬件
/// FIFO，否则长报文在 FIFO 溢出时丢字节 → 校验和错误）。
pub(crate) fn uart_drain_hw() {
    let uart = uart();
    while unsafe { read_volatile(&uart.fr) } & FR_RXFE == 0 {
        let c = unsafe { read_volatile(&uart.dr) } as u8;
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

/// 非阻塞读取 UART 接收缓冲中的一个字节（供 /dev/tty 与 shell 使用）。
///
/// 返回 `None` 表示当前无待读字节。
pub(crate) fn uart_try_read_char() -> Option<u8> {
    unsafe { asm!("msr daifset, #2") };
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
    unsafe { asm!("msr daifclr, #2") };
    c
}

/// 读取并清零 UART 溢出丢弃计数（供 shell 提示用户输入被丢弃）。
pub(crate) fn uart_take_dropped() -> usize {
    unsafe { asm!("msr daifset, #2") };
    let d = unsafe { UART_RX_DROPPED };
    unsafe {
        UART_RX_DROPPED = 0;
    }
    unsafe { asm!("msr daifclr, #2") };
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

/// 清除 PSTATE 的 I 位，开启 IRQ
///
/// # 安全
/// 调用者必须已正确初始化异常向量表与 GIC，否则开启 IRQ 会导致未定义行为。
pub(crate) unsafe fn enable_irq() {
    asm!("msr daifclr, #2");
}

/// 等待中断
///
/// # 安全
/// wfi 是架构指令，任意时刻可执行。
pub(crate) unsafe fn wfi() {
    asm!("wfi");
}

/// 读取 ESR_EL1（异常综合寄存器）
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn read_esr_el1() -> u64 {
    let v: u64;
    asm!("mrs {}, esr_el1", out(reg) v);
    v
}

/// 读取 ELR_EL1（异常链接寄存器，保存故障指令地址）
///
/// # 安全
/// 需要 EL1 权限。
unsafe fn read_elr_el1() -> u64 {
    let v: u64;
    asm!("mrs {}, elr_el1", out(reg) v);
    v
}

// ==================== UART 中断处理 ====================

/// UART 接收中断：读取所有可用字符并送入接收环形缓冲（不在此回显，
/// 回显由消费方如 shell 的行编辑完成）。
fn uart_handle_rx() {
    unsafe { uart_rx_drain() };
    let uart = uart();
    // 清除接收中断与接收超时中断
    unsafe {
        write_volatile(&mut uart.icr, ICR_RXIC | ICR_RTIC);
    }
}

/// 排空 UART 接收 FIFO 并按正常路径分发（输入子系统优先，其余入 tty 环形缓冲）。
///
/// 供两处调用：RX 中断处理本体，以及 IRQ 关闭的长 TX 路径（风暴转储等）
/// 的间隙轮询 —— PL011 RX FIFO 仅 16 字节（115200 下 ~1.4ms 填满），转储
/// 忙等超过该窗口会静默丢输入字节（终端粘贴/自动化测试注入被截断）。
pub(crate) unsafe fn uart_rx_drain() {
    let uart = uart();
    // 只要接收 FIFO 非空（RXFE=0）就读取
    while read_volatile(&uart.fr) & FR_RXFE == 0 {
        let c = read_volatile(&uart.dr) as u8;
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
    let mut gicd_base = platform.gicd_base();
    let mut gicc_base = platform.gicc_base();
    let mut mem_base = platform.ram_base();
    let mut mem_size = platform.ram_size();
    // fw_cfg 为 QEMU 特有设备（Pi 4 无此设备），保留默认值，由设备树覆盖
    let mut fwcfg_base: usize = 0x0902_0000;
    let mut stdout_path: Option<&str> = None;
    let mut boot_shell = false; // bootargs 含 "shell"：GUI 启动同时拉起内置 shell（QEMU 测试用）
    let mut boot_wifitest = false; // bootargs 含 "wifitest"：无头 Wi-Fi 自检
    let mut boot_perftest = false; // bootargs 含 "perftest"：启动期动画性能基线（默认跳过）
    let mut boot_selftest = false; // bootargs 含 "selftest"：启动期内核自检（默认跳过）
    let mut boot_verbose = false; // bootargs 含 "verbose"：完整启动日志（默认安静控制台）
    let mut virtio_devs: [net_virtio::MmioDev; 32] = [net_virtio::MmioDev::empty(); 32];
    let mut virtio_count = 0usize;

    if let Some(fdt) = fdt {
        // /chosen
        if let Some(chosen) = fdt.node("/chosen") {
            stdout_path = chosen.prop_str(&fdt, "stdout-path");
            // QEMU `-append "shell"` 会写入 /chosen/bootargs；含 "shell" 时
            // 在 /init 之外再拉起内置交互 shell（供 UART 驱动测试）。
            if let Some(args) = chosen.prop_str(&fdt, "bootargs") {
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
        // UART：优先 stdout-path 指定的节点（secure=on 时 DTB 有 secure-chosen
        // →0x9040000 与普通 chosen →0x9000000 两个 pl011，控制台接普通串口，
        // 必须按 stdout-path 选择而非 find_compatible 的首个匹配），
        // 失败按 /soc/pl011 或 compatible 查找
        let uart = if let Some(sp) = stdout_path {
            fdt.node(sp).or_else(|| fdt.node("/soc/pl011").or_else(|| fdt.find_compatible("arm,pl011")))
        } else {
            fdt.node("/soc/pl011").or_else(|| fdt.find_compatible("arm,pl011"))
        };
        if let Some(uart) = uart {
            if let Some((base, _)) = uart.reg(&fdt, 0) {
                uart_base = base as usize;
            }
            if let Some((t, n, _)) = uart.interrupt(&fdt, 0) {
                uart_irq = fdt::gic_irq(t, n);
            }
        }
        // UART 时钟：fixed-clock 节点（QEMU virt 为 24MHz）
        if let Some(clk) = fdt.find_compatible("fixed-clock") {
            if let Some(freq) = clk.prop_u32(&fdt, "clock-frequency", 0) {
                uart_clk = freq;
            }
        }
        // GIC：优先 /soc/gic 路径，失败按 compatible 查找
        if let Some(gic) = fdt.node("/soc/gic").or_else(|| fdt.find_compatible("arm,cortex-a15-gic")) {
            if let Some((base, _)) = gic.reg(&fdt, 0) {
                gicd_base = base as usize;
            }
            if let Some((base, _)) = gic.reg(&fdt, 1) {
                gicc_base = base as usize;
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
        uart_send_str("[FDT] gic gicd=");
        print_hex(gicd_base as u64);
        uart_send_str(" gicc=");
        print_hex(gicc_base as u64);
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
    gic::gic_init(gicd_base, gicc_base);
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
    timer::timer2_set_interval(8_333);
    timer::timer2_register_handler(anim::frame_tick);

    // ===== 7. 注册中断处理函数 =====
    interrupt::register_handler(timer::TIMER_IRQ, timer::timer_handle_irq);
    interrupt::register_handler(timer::TIMER2_IRQ, timer::timer2_handle_irq);
    interrupt::register_handler(uart_irq, uart_handle_rx);

    // ===== 8. 配置并使能 GIC 中断 =====
    gic::gic_enable_irq(timer::TIMER_IRQ);
    gic::gic_set_priority(timer::TIMER_IRQ, 0x80);
    gic::gic_enable_irq(timer::TIMER2_IRQ); // 动画帧 tick（手势动画渲染管线）
    gic::gic_set_priority(timer::TIMER2_IRQ, 0x80);
    gic::gic_enable_irq(uart_irq);
    gic::gic_set_priority(uart_irq, 0xA0);
    gic::gic_set_target(uart_irq, 0x01); // 目标 CPU0

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
    if smp::cpu_count() > 1 {
        smp::SCHED_READY.store(true, core::sync::atomic::Ordering::Release);
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
        // 等待 /init 退出（正常情况下 /init 常驻，循环 wait4 回收子进程）。
        loop {
            let (rpid, code) = process::wait(pid);
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
        }
        elf::release(pid as u32);
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

/// 按最近睡眠截止时间动态重装物理定时器，同时保持 10ms 调度时间片上限。
fn timer_rearm() {
    timer::timer_set_interval(sched_cfs::next_timer_interval_us());
}

/// 检查路径是否可解析（文件或目录存在）。
#[allow(dead_code)]
fn file_exists(path: &str) -> bool {
    vfs::stat_path(path).is_some()
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
    unsafe { asm!("msr daifset, #2") };
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
    unsafe { asm!("mov {}, sp", out(reg) sp) };
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
        if (0x4008_0000..0x400c_3000).contains(&v) {
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
    // 死循环，不再返回
    loop {
        unsafe {
            wfi();
        }
    }
}

/// 同步异常处理入口：读取 ESR_EL1 / ELR_EL1 后打印并死循环（等效 panic）。
///
/// 用于捕获 MMU 配置错误等引起的 data/prefetch abort。
#[no_mangle]
extern "C" fn sync_exception_handler() -> ! {
    // 防递归：入口屏蔽 IRQ 并记录首次故障，避免 panic 处理自身再次
    // 触发异常（用户栈区 sp 等损坏状态会无限递归刷屏）
    unsafe { asm!("msr daifset, #2") };
    static mut FIRST: bool = true;
    if unsafe { !FIRST } {
        let esr = unsafe { read_esr_el1() };
        uart_send_str("PANIC（重入）ESR=");
        print_hex(esr);
        uart_send_str("\n");
        loop {
            unsafe { wfi() };
        }
    }
    unsafe { FIRST = false };
    // 读取异常综合寄存器（异常原因）与异常链接寄存器（故障地址）
    let esr = unsafe { read_esr_el1() };
    let elr = unsafe { read_elr_el1() };
    let far: u64;
    unsafe { asm!("mrs {}, far_el1", out(reg) far) };
    uart_send_str("PANIC: 同步异常 ESR=");
    print_hex(esr);
    uart_send_str(" ELR=");
    print_hex(elr);
    uart_send_str(" FAR=");
    print_hex(far);
    // PAN 故障诊断：DFSC=permission fault（0b001001/0b001011/0b001101/0b001111）
    // 且 FAR 落在用户地址范围、SCTLR_EL1.PAN 已置位 → 内核漏了 uaccess 包装
    let dfsc = (esr & 0x3F) as u32;
    let is_perm_fault = matches!(dfsc & 0b111001, 0b001001 | 0b010001);
    if is_perm_fault && far < crate::vm::USER_VA_TOP as u64 && mmu::pan_supported() {
        uart_send_str("\n[PAN] EL1 访问用户内存被 PAN 拦截（漏了 uaccess_begin/end 包装？）"
            );
    }
    uart_send_str(" pid=");
    crate::print_dec(crate::sched_cfs::current_pid() as u64);
    uart_send_str(" tid=");
    crate::print_dec(crate::sched_cfs::current_tid() as u64);
    uart_send_str(" name=");
    uart_send_str(crate::sched_cfs::current_task_name());
    uart_send_str(" sp=");
    let sp: u64;
    unsafe { asm!("mov {}, sp", out(reg) sp) };
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
        if (0x4008_0000..0x400c_3000).contains(&v) {
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
    let esr = unsafe { read_esr_el1() };
    let elr = unsafe { read_elr_el1() };
    let far: u64;
    unsafe { asm!("mrs {}, far_el1", out(reg) far) };
    uart_send_str("PANIC: 未处理异常（FIQ/SError/SP0）ESR=");
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
    let esr = unsafe { read_esr_el1() };
    let elr = unsafe { read_elr_el1() };
    let pid = crate::sched_cfs::current_pid();
    // [crash] 生成并落盘 tombstone（SIGSEGV/SIGILL/SIGTRAP 语义，
    // 含寄存器/内存映射/栈回溯/日志快照），随后继续打印详细诊断。
    {
        let far: u64;
        unsafe { asm!("mrs {}, far_el1", out(reg) far) };
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
        (*p.add(31), *p.add(32), *p.add(33)) // sp_el0@0xf8/32, elr@0x100/33, spsr@0x108/34
    };
    uart_send_str("\n[el0-fault frame] sp_el0=");
    print_hex(f_sp);
    uart_send_str(" elr=");
    print_hex(f_elr);
    uart_send_str(" spsr=");
    print_hex(f_spsr);

    // 关键调用寄存器：x29(fp)/x30(lr)/x0/x1/x8（定位跳转 0 的调用点）
    let (x29, x30, x0, x1, x8) = unsafe {
        let p = frame as *const u64;
        (*p.add(29), *p.add(30), *p.add(0), *p.add(1), *p.add(8))
    };
    uart_send_str(" x29=");
    print_hex(x29);
    uart_send_str(" x30=");
    print_hex(x30);
    uart_send_str(" x0=");
    print_hex(x0);
    uart_send_str(" x1=");
    print_hex(x1);
    uart_send_str(" x8=");
    print_hex(x8);
    let (x19, x20, x21, x22, x23) = unsafe {
        let p = frame as *const u64;
        (*p.add(19), *p.add(20), *p.add(21), *p.add(22), *p.add(23))
    };
    uart_send_str(" x19=");
    print_hex(x19);
    uart_send_str(" x20=");
    print_hex(x20);
    uart_send_str(" x21=");
    print_hex(x21);
    uart_send_str(" x22=");
    print_hex(x22);
    uart_send_str(" x23=");
    print_hex(x23);

    // FAR_EL1：data abort 时是故障访问地址（定位越界写目标）
    let far: u64;
    unsafe { asm!("mrs {}, far_el1", out(reg) far) };
    uart_send_str(" far=");
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

    // 用户回溯：沿 x29 帧指针链（每帧 [fp]=prev_fp, [fp+8]=lr）打印调用栈，
    // 定位「ret 到 0 / 垃圾地址」时崩溃函数的调用者。
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
        let lr = unsafe { *((fp + 8) as *const u64) };
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
    if sb >= 0x4000_0000 && st > sb {
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
extern "C" fn el0_page_fault_handler(_frame: *mut u8) -> bool {
    // ESR_EL1：DFSC 位于 bits[5:0]
    let esr: u64;
    unsafe { asm!("mrs {}, esr_el1", out(reg) esr) };
    let dfsc = (esr & 0x3F) as u32;
    // FAR_EL1：data abort 的故障访问地址
    let far: u64;
    unsafe { asm!("mrs {}, far_el1", out(reg) far) };
    let va = far as usize;
    if !crate::vm::user_va_range_ok(va, 1) {
        return false;
    }
    // 掩掉 level 位（bit1、bit2、bit3）与 TTL 位（bit4），只保留故障类型：
    //   translation（任意 level）→ 0b000001
    //   permission L0/L1（无 TTL）→ 0b001001；permission L2/L3（TTL）→ 0b010001
    const DFSC_TRANSLATION: u32 = 0b000001;
    const DFSC_PERMISSION_L01: u32 = 0b001001;
    const DFSC_PERMISSION_L23: u32 = 0b010001;
    match dfsc & 0b111001 {
        DFSC_TRANSLATION => crate::process::page_fault_resolve(va),
        DFSC_PERMISSION_L01 | DFSC_PERMISSION_L23 => crate::process::cow_break(va),
        _ => false,
    }
}
// ==================== 启动汇编 ====================

global_asm!(
    r#"
    // ===== 启动代码：从复位向量开始 =====
    .section .text._start, "ax"
    .global _start
    .type _start, %function
_start:
    // 保存 QEMU 通过 x0 传入的设备树地址到 x19（启动代码不使用 x19，后续会写入 DTB_ADDR）
    mov x19, x0

    // ===== 若当前在 EL2（secure=on：EL3 固件把内核带到 EL2），先降到 EL1 =====
    bl drop_to_el1

    // 关闭 MMU：清除 SCTLR_EL1 的 M 位（bit0）
    mrs x0, sctlr_el1
    bic x0, x0, #1
    msr sctlr_el1, x0
    isb

    // 切换当前栈指针为 SP_EL1（SPSel=1）
    msr spsel, #1
    isb
    // 设置 EL1 栈指针为 0x40080000（mov sp 写入当前选中的 SP_EL1）
    ldr x0, =_stack_top
    mov sp, x0

    // 设置异常向量表基地址
    ldr x0, =vector_table
    msr vbar_el1, x0
    isb

    // 清零 BSS 段
    ldr x0, =_bss_start
    ldr x1, =_bss_end
1:
    cmp x0, x1
    b.ge 2f
    str xzr, [x0], #8
    b 1b
2:

    // 使能 FP/SIMD：CPACR_EL1.FPEN = 0b11（bits[21:20]）。
    // 复位时 FP/SIMD 被禁用，Rust 编译器可能生成 NEON 指令（如向量化初始化），
    // 若不使能会在执行 SIMD 指令时触发 synchronous trap。
    mov x0, #0x300000
    msr cpacr_el1, x0
    isb

    // 把保存的设备树地址写入全局变量（必须在 BSS 清零之后）
    ldr x1, =DTB_ADDR
    str x19, [x1]

    // 跳转到 Rust 主函数
    bl rust_main

    // rust_main 不应返回，若返回则死循环
    b .

    // ===== secondary 核入口（SMP，PSCI CPU_ON 引导至此） =====
    .section .text.secondary, "ax"
    .global secondary_entry
    .type secondary_entry, %function
secondary_entry:
    // x0 = cpu id（从 MPIDR 读取，不依赖 PSCI context_id）
    mrs x19, mpidr_el1
    and x19, x19, #0xFF
    // 设置 per-cpu 栈：SECONDARY_STACKS + cpu*STACK_SIZE + STACK_SIZE
    ldr x0, =SECONDARY_STACKS
    mov x1, #16384
    madd x0, x19, x1, x0
    add sp, x0, x1
    // 若 PSCI 以 EL2 唤醒（secure=on），降到 EL1
    bl drop_to_el1
    // 使能 FP/SIMD（与 _start 一致）
    mov x0, #0x300000
    msr cpacr_el1, x0
    isb
    // 设置异常向量表
    ldr x0, =vector_table
    msr vbar_el1, x0
    isb
    mov x0, x19
    bl smp_secondary_main
    b .

    // ===== 子程序：当前在 EL2 时降到 EL1（调用者处于 EL2 或 EL1 均可） =====
    // 返回后处于 EL1，继续执行调用点之后的代码。
    // 仅供 secure=on（EL3 固件把内核/唤醒核带到 EL2）的启动路径使用。
    .section .text.drop_to_el1, "ax"
    .global drop_to_el1
    .type drop_to_el1, %function
drop_to_el1:
    mrs x9, currentel
    and x9, x9, #0xC
    cmp x9, #0x8
    b.ne 9f
    // 配置 HCR_EL2.RW=1（EL1 以 AArch64 运行）
    mrs x9, hcr_el2
    orr x9, x9, #(1 << 31)
    msr hcr_el2, x9
    isb
    // 清零 SCTLR_EL1（关闭 EL1 MMU/缓存，恢复复位语义）
    msr sctlr_el1, xzr
    // 降到 EL1：SPSR_EL2 = EL1h + DAIF 屏蔽，ELR_EL2 = 继续执行的地址
    mov x9, #0x3C5
    msr spsr_el2, x9
    adr x9, 9f
    msr elr_el2, x9
    eret
9:
    ret

    // ===== 异常向量表：必须 2KB 对齐，每个表项必须位于 0x80 偏移处 =====
    .section .text.vector, "ax"
    .balign 2048
    .global vector_table
vector_table:
    // 当前 EL + SP_EL0（内核不运行于 SP0，若触发说明配置错误：打印诊断）
    b unknown_fault_entry  // 0x000 同步异常
    .balign 0x80
    b unknown_fault_entry  // 0x080 IRQ
    .balign 0x80
    b unknown_fault_entry  // 0x100 FIQ
    .balign 0x80
    b unknown_fault_entry  // 0x180 SError
    .balign 0x80
    // 当前 EL + SP_ELx
    b sync_entry        // 0x200 同步异常
    .balign 0x80
    b irq_entry         // 0x280 IRQ（使用 SP_EL1 时的 IRQ 入口）
    .balign 0x80
    b unknown_fault_entry  // 0x300 FIQ
    .balign 0x80
    b unknown_fault_entry  // 0x380 SError
    .balign 0x80
    // 低特权级 AArch64
    b lower_sync         // 0x400 同步异常（svc #0）
    .balign 0x80
    b irq_entry          // 0x480 IRQ（EL0 被定时器抢占时，复用 EL1 的 IRQ 处理入口）
    .balign 0x80
    b unknown_fault_entry  // 0x500 FIQ
    .balign 0x80
    b unknown_fault_entry  // 0x580 SError
    .balign 0x80
    // 低特权级 AArch32（本系统不运行 AArch32：打印诊断后停机）
    b unknown_fault_entry  // 0x600 同步异常
    .balign 0x80
    b unknown_fault_entry  // 0x680 IRQ
    .balign 0x80
    b unknown_fault_entry  // 0x700 FIQ
    .balign 0x80
    b unknown_fault_entry  // 0x780 SError
    .balign 0x80

    // ===== 未接线异常入口：屏蔽 IRQ 后调用 Rust 诊断处理器 =====
    .type unknown_fault_entry, %function
unknown_fault_entry:
    msr daifset, #2
    bl unknown_exception_handler
    b .

    // ===== IRQ 处理入口：保存现场 -> 调用 Rust handler -> 恢复现场 -> eret =====
    .type irq_entry, %function
irq_entry:
    // 立即屏蔽 IRQ（硬件入口已置 I=1，此处显式化）：保证后续寄存器保存与
    // elr_el1/spsr_el1 的读取在屏蔽态下原子完成，杜绝嵌套抢占污染。
    msr daifset, #2
    // 保存通用寄存器 x0-x30（32*8=256 字节）+ FP/SIMD 上下文
    // （v0-v31 + fpsr/fpcr @0x108 起，共 520 字节）；帧总 784 字节、16 对齐。
    sub sp, sp, #784
    stp x0, x1, [sp, #(16 * 0)]
    stp x2, x3, [sp, #(16 * 1)]
    stp x4, x5, [sp, #(16 * 2)]
    stp x6, x7, [sp, #(16 * 3)]
    stp x8, x9, [sp, #(16 * 4)]
    stp x10, x11, [sp, #(16 * 5)]
    stp x12, x13, [sp, #(16 * 6)]
    stp x14, x15, [sp, #(16 * 7)]
    stp x16, x17, [sp, #(16 * 8)]
    stp x18, x19, [sp, #(16 * 9)]
    stp x20, x21, [sp, #(16 * 10)]
    stp x22, x23, [sp, #(16 * 11)]
    stp x24, x25, [sp, #(16 * 12)]
    stp x26, x27, [sp, #(16 * 13)]
    stp x28, x29, [sp, #(16 * 14)]
    str x30, [sp, #(16 * 15)]
    // FP/SIMD 保存 @0x100（q 寄存器对，16 对齐）
    stp q0, q1, [sp, #0x100]
    stp q2, q3, [sp, #0x120]
    stp q4, q5, [sp, #0x140]
    stp q6, q7, [sp, #0x160]
    stp q8, q9, [sp, #0x180]
    stp q10, q11, [sp, #0x1A0]
    stp q12, q13, [sp, #0x1C0]
    stp q14, q15, [sp, #0x1E0]
    stp q16, q17, [sp, #0x200]
    stp q18, q19, [sp, #0x220]
    stp q20, q21, [sp, #0x240]
    stp q22, q23, [sp, #0x260]
    stp q24, q25, [sp, #0x280]
    stp q26, q27, [sp, #0x2A0]
    stp q28, q29, [sp, #0x2C0]
    stp q30, q31, [sp, #0x2E0]
    mrs x9, fpsr
    str x9, [sp, #0x300]
    mrs x9, fpcr
    str x9, [sp, #0x308]

    // 调用 Rust IRQ 处理器
    bl irq_handler

    // 防御护栏：返回 EL0 前校验 eret 目标。用户态 IRQ 的中断点（elr_el1）在
    // 被抢占/恢复期间若被损坏为 0 或内核地址（>1GB），eret 会让用户态从 PC=0
    // 或内核地址取指（表现为偶发「EL0 取指 PC=0 / 执行内核代码」故障）。
    // 校验放在**弹出用户寄存器之前**（x9/x10 随后被 pop 恢复，不污染用户现场）；
    // EL1 返回（M≠0，syscall/内核上下文里的 IRQ）跳过校验。
    mrs x9, spsr_el1
    and w10, w9, #0xf
    cbnz w10, 1f
    mrs x9, elr_el1
    cbz x9, 2f
    movz x10, #0x4000, lsl #16
    cmp x9, x10
    b.hs 2f
1:
    // 恢复 FP/SIMD 上下文（x9 随后从帧中恢复）
    ldp q0, q1, [sp, #0x100]
    ldp q2, q3, [sp, #0x120]
    ldp q4, q5, [sp, #0x140]
    ldp q6, q7, [sp, #0x160]
    ldp q8, q9, [sp, #0x180]
    ldp q10, q11, [sp, #0x1A0]
    ldp q12, q13, [sp, #0x1C0]
    ldp q14, q15, [sp, #0x1E0]
    ldp q16, q17, [sp, #0x200]
    ldp q18, q19, [sp, #0x220]
    ldp q20, q21, [sp, #0x240]
    ldp q22, q23, [sp, #0x260]
    ldp q24, q25, [sp, #0x280]
    ldp q26, q27, [sp, #0x2A0]
    ldp q28, q29, [sp, #0x2C0]
    ldp q30, q31, [sp, #0x2E0]
    ldr x9, [sp, #0x300]
    msr fpsr, x9
    ldr x9, [sp, #0x308]
    msr fpcr, x9
    // 恢复通用寄存器
    ldp x0, x1, [sp, #(16 * 0)]
    ldp x2, x3, [sp, #(16 * 1)]
    ldp x4, x5, [sp, #(16 * 2)]
    ldp x6, x7, [sp, #(16 * 3)]
    ldp x8, x9, [sp, #(16 * 4)]
    ldp x10, x11, [sp, #(16 * 5)]
    ldp x12, x13, [sp, #(16 * 6)]
    ldp x14, x15, [sp, #(16 * 7)]
    ldp x16, x17, [sp, #(16 * 8)]
    ldp x18, x19, [sp, #(16 * 9)]
    ldp x20, x21, [sp, #(16 * 10)]
    ldp x22, x23, [sp, #(16 * 11)]
    ldp x24, x25, [sp, #(16 * 12)]
    ldp x26, x27, [sp, #(16 * 13)]
    ldp x28, x29, [sp, #(16 * 14)]
    ldr x30, [sp, #(16 * 15)]
    add sp, sp, #784
    // 异常返回
    eret
2:
    // 校验失败：把 IRQ 帧转成 272 字节陷阱帧（基址下移 16 字节 + 补 sp_el0/elr/spsr），
    // 走 el0_fault 干净终止进程（罕见路径，长度无所谓）。
    sub sp, sp, #16
    ldr x0, [sp, #0x10]; str x0, [sp, #0x00]
    ldr x1, [sp, #0x18]; str x1, [sp, #0x08]
    ldr x2, [sp, #0x20]; str x2, [sp, #0x10]
    ldr x3, [sp, #0x28]; str x3, [sp, #0x18]
    ldr x4, [sp, #0x30]; str x4, [sp, #0x20]
    ldr x5, [sp, #0x38]; str x5, [sp, #0x28]
    ldr x6, [sp, #0x40]; str x6, [sp, #0x30]
    ldr x7, [sp, #0x48]; str x7, [sp, #0x38]
    ldr x8, [sp, #0x50]; str x8, [sp, #0x40]
    ldr x9, [sp, #0x58]; str x9, [sp, #0x48]
    ldr x10, [sp, #0x60]; str x10, [sp, #0x50]
    ldr x11, [sp, #0x68]; str x11, [sp, #0x58]
    ldr x12, [sp, #0x70]; str x12, [sp, #0x60]
    ldr x13, [sp, #0x78]; str x13, [sp, #0x68]
    ldr x14, [sp, #0x80]; str x14, [sp, #0x70]
    ldr x15, [sp, #0x88]; str x15, [sp, #0x78]
    ldr x16, [sp, #0x90]; str x16, [sp, #0x80]
    ldr x17, [sp, #0x98]; str x17, [sp, #0x88]
    ldr x18, [sp, #0xa0]; str x18, [sp, #0x90]
    ldr x19, [sp, #0xa8]; str x19, [sp, #0x98]
    ldr x20, [sp, #0xb0]; str x20, [sp, #0xa0]
    ldr x21, [sp, #0xb8]; str x21, [sp, #0xa8]
    ldr x22, [sp, #0xc0]; str x22, [sp, #0xb0]
    ldr x23, [sp, #0xc8]; str x23, [sp, #0xb8]
    ldr x24, [sp, #0xd0]; str x24, [sp, #0xc0]
    ldr x25, [sp, #0xd8]; str x25, [sp, #0xc8]
    ldr x26, [sp, #0xe0]; str x26, [sp, #0xd0]
    ldr x27, [sp, #0xe8]; str x27, [sp, #0xd8]
    ldr x28, [sp, #0xf0]; str x28, [sp, #0xe0]
    ldr x29, [sp, #0xf8]; str x29, [sp, #0xe8]
    ldr x30, [sp, #0x100]; str x30, [sp, #0xf0]
    mrs x0, sp_el0
    str x0, [sp, #0xf8]
    mrs x0, elr_el1
    str x0, [sp, #0x100]
    mrs x0, spsr_el1
    str x0, [sp, #0x108]
    b el0_fault

    // ===== 同步异常处理入口：调用 Rust 处理器（不返回） =====
    .type sync_entry, %function
sync_entry:
    // 立即屏蔽 IRQ：同步异常入口到 handler 的 daifset 之间若被定时器抢占，
    // IRQ 入口会覆盖 elr_el1/spsr_el1，panic 诊断读到 IRQ 残留（PC/寄存器错乱）。
    msr daifset, #2
    // [诊断] 在调用 Rust 处理器前保存故障点通用寄存器（print 会污染 x0-x30）。
    // 基址用 x11（故障 x11 丢失，槽位 11 存基址地址作标记）；memcpy 循环的
    // dst=x9 / src=x13 / data=x14 均完整保留。
    adrp x11, PANIC_REGS
    add x11, x11, :lo12:PANIC_REGS
    stp x0, x1, [x11, #0x00]
    stp x2, x3, [x11, #0x10]
    stp x4, x5, [x11, #0x20]
    stp x6, x7, [x11, #0x30]
    stp x8, x9, [x11, #0x40]
    stp x10, x11, [x11, #0x50]
    stp x12, x13, [x11, #0x60]
    stp x14, x15, [x11, #0x70]
    stp x16, x17, [x11, #0x80]
    stp x18, x19, [x11, #0x90]
    stp x20, x21, [x11, #0xa0]
    stp x22, x23, [x11, #0xb0]
    stp x24, x25, [x11, #0xc0]
    stp x26, x27, [x11, #0xd0]
    stp x28, x29, [x11, #0xe0]
    str x30, [x11, #0xf0]
    // GDB 内核断点：BRK64（0x3C）与软件步进（0x33）from EL1 → 调试桩；
    // 返回 0 = 恢复寄存器并 eret（继续内核执行），非 0 = 走 panic
    mrs x9, esr_el1
    lsr x10, x9, #26
    cmp x10, #0x3c
    b.eq 1f
    cmp x10, #0x33
    b.ne 2f
1:
    bl sync_gdb_entry
    cbz x0, 3f
2:
    // 直接调用 Rust 同步异常处理器（它读取 ESR/ELR 并死循环，不返回）
    bl sync_exception_handler
    b .
3:
    // GDB 继续：从 PANIC_REGS 恢复 x0-x30（x11 槽为基址标记，恢复后 x11
    // 为基址地址——内核断点场景下 x11 原始值丢失，属已知限制）并 eret。
    // elr_el1/spsr_el1 已由 sync_gdb_entry 写入。
    adrp x11, PANIC_REGS
    add x11, x11, :lo12:PANIC_REGS
    ldp x0, x1, [x11, #0x00]
    ldp x2, x3, [x11, #0x10]
    ldp x4, x5, [x11, #0x20]
    ldp x6, x7, [x11, #0x30]
    ldp x8, x9, [x11, #0x40]
    ldp x10, x11, [x11, #0x50]
    ldp x12, x13, [x11, #0x60]
    ldp x14, x15, [x11, #0x70]
    ldp x16, x17, [x11, #0x80]
    ldp x18, x19, [x11, #0x90]
    ldp x20, x21, [x11, #0xa0]
    ldp x22, x23, [x11, #0xb0]
    ldp x24, x25, [x11, #0xc0]
    ldp x26, x27, [x11, #0xd0]
    ldp x28, x29, [x11, #0xe0]
    ldr x30, [x11, #0xf0]
    eret

    // ===== EL0 同步异常（svc）入口：保存完整寄存器现场，调用 Rust 分发后 eret =====
    .type lower_sync, %function
    lower_sync:
    // 立即屏蔽 IRQ：svc 入口的 ESR 检查（下方 mrs esr_el1）必须读到**本次**
    // 异常的 ESR。若在检查前被定时器抢占，irq_entry 的 eret 不恢复 ESR_EL1，
    // 检查会读到 IRQ 残留值（EC≠0x15）→ 把正常 svc 误判为 EL0 fault。
    msr daifset, #2
    // 在 SP_EL1（当前任务内核栈）上分配 800 字节陷阱帧：272 字节头（布局与
    // syscall_table::TrapFrame 一致）+ 8 字节填充 + 520 字节 FP/SIMD 上下文
    // （v0-v31 + fpsr/fpcr @0x118 起）。FP 在异常入口保存、返回前恢复，
    // 防止 IRQ/内核 memcpy（NEON）静默破坏用户态浮点状态。
    sub sp, sp, #800
    stp x0, x1, [sp, #0x00]
    stp x2, x3, [sp, #0x10]
    stp x4, x5, [sp, #0x20]
    stp x6, x7, [sp, #0x30]
    stp x8, x9, [sp, #0x40]
    stp x10, x11, [sp, #0x50]
    stp x12, x13, [sp, #0x60]
    stp x14, x15, [sp, #0x70]
    stp x16, x17, [sp, #0x80]
    stp x18, x19, [sp, #0x90]
    stp x20, x21, [sp, #0xa0]
    stp x22, x23, [sp, #0xb0]
    stp x24, x25, [sp, #0xc0]
    stp x26, x27, [sp, #0xd0]
    stp x28, x29, [sp, #0xe0]
    str x30, [sp, #0xf0]
    mrs x21, sp_el0
    mrs x22, elr_el1
    mrs x23, spsr_el1
    str x21, [sp, #0xf8]
    str x22, [sp, #0x100]
    str x23, [sp, #0x108]
    // 保存 FP/SIMD 上下文（v0-v31 + fpsr/fpcr）@0x110 起（q 寄存器对，16 对齐）
    stp q0, q1, [sp, #0x110]
    stp q2, q3, [sp, #0x130]
    stp q4, q5, [sp, #0x150]
    stp q6, q7, [sp, #0x170]
    stp q8, q9, [sp, #0x190]
    stp q10, q11, [sp, #0x1B0]
    stp q12, q13, [sp, #0x1D0]
    stp q14, q15, [sp, #0x1F0]
    stp q16, q17, [sp, #0x210]
    stp q18, q19, [sp, #0x230]
    stp q20, q21, [sp, #0x250]
    stp q22, q23, [sp, #0x270]
    stp q24, q25, [sp, #0x290]
    stp q26, q27, [sp, #0x2B0]
    stp q28, q29, [sp, #0x2D0]
    stp q30, q31, [sp, #0x2F0]
    mrs x9, fpsr
    str x9, [sp, #0x310]
    mrs x9, fpcr
    str x9, [sp, #0x318]

    // 检查 ESR：仅 EC == 0x15（SVC from EL0）按系统调用处理；
    // EC == 0x24（data abort from EL0）先尝试按需分页解析（demand paging），
    // 解析成功则恢复现场并 eret 重试故障指令，失败再终止进程；
    // 其余（指令 abort / PC 未对齐等）直接走 el0_fault_handler 终止当前进程，
    // 而不是被当成 svc 或挂死整个内核。
    mrs x9, esr_el1
    lsr x10, x9, #26
    cmp x10, #0x15
    b.eq 1f
    // 非 SVC：是否为 EL0 data abort（按需分页候选）
    cmp x10, #0x24
    b.eq 2f
    // GDB 调试异常：BRK64（0x3C，软件断点）与软件步进（0x32）→ 调试桩
    cmp x10, #0x3c
    b.eq 3f
    cmp x10, #0x32
    b.eq 3f
    b el0_fault
2:
    // 尝试按需分页：x0 = 陷阱帧指针（保留，未处理时复用）
    mov x0, sp
    bl el0_page_fault_handler
    // 返回 x0 != 0 表示已落地：恢复现场并 eret 重试故障指令
    cbnz x0, syscall_return_restore
    // 未处理（非预留页等）：走 el0_fault_handler 终止进程
    mov x0, sp
    b el0_fault
3:
    // GDB 调试桩：x0 = 陷阱帧指针；返回 0 = 恢复现场继续（eret），
    // 非 0（未注册断点的 BRK 等）= 走 el0_fault 终止进程
    mov x0, sp
    bl gdb_debug_entry
    cbz x0, syscall_return_restore
    b el0_fault
1:
    // 确认是 svc：恢复 IRQ（syscall 处理可被抢占）
    msr daifclr, #2
    // 调用 Rust 系统调用分发：x0 = 陷阱帧指针
    mov x0, sp
    bl syscall_dispatch
    // 恢复现场（帧中的 x0 已被更新为返回值）。
    // 该标签同时作为 fork/clone 子进程/子线程的首次调度入口：子进程的
    // TaskContext 直接把 sp 指向其**拷贝的陷阱帧**、x30 指向此处，从而绕过
    // syscall_dispatch —— 既避免子进程把返回值写回父进程陷阱帧（跨栈写），
    // 也避免子进程沿父调用链回卷（子帧 x0 已被 fork/clone 置 0，elr/spsr/
    // sp_el0 均为其自身的副本值），eret 直接回到用户态 fork()/clone() 调用点。
    .global syscall_return_restore
    // ===== fork/clone 子任务首次调度入口 =====
    // 子任务的恢复点不是任何 switch 路径（thread_trampoline 只用于 spawn），
    // 须先释放调度锁（切换路径在 switch 前获取、由被切到任务的恢复点释放，
    // 见 sched_cfs::sched_unlock_from_asm），再进入 syscall_return_restore。
    .global el0_resume_entry
    .type el0_resume_entry, %function
el0_resume_entry:
    bl sched_unlock_from_asm
    b syscall_return_restore
syscall_return_restore:
    // ★ 关键修复：恢复/eret 全程屏蔽 IRQ。syscall 体（dispatch 前）已被
    // `daifclr` 放开抢占，若定时器 IRQ 恰在「msr elr_el1（0x38）→ eret」窗口
    // 抢占本任务，switch_to 会把 elr_el1 = 本段代码地址（如 0x4008113c）存进
    // 任务上下文；恢复时 eret 跳回该内核地址 → EL0 执行内核代码 → 故障
    // （表现为 fork 子进程偶发 PC=0 / 内核地址）。此处重新屏蔽 IRQ，保证
    // 恢复+eret 原子完成（eret 到 EL0 时硬件自动开中断）。
    msr daifset, #2
    ldr x21, [sp, #0xf8]
    ldr x22, [sp, #0x100]
    ldr x23, [sp, #0x108]
    // 防御护栏：EL0 返回地址必须非 0 且低于内核恒等映射起点（0x40000000）。
    // 陷阱帧 elr 损坏（被并发覆写/栈越界踩坏）时直接走 el0_fault 干净终止
    // 当前进程，而不是 eret 到地址 0（此前偶发「EL0 取指 PC=0」故障）。
    cmp x22, #0
    b.eq el0_fault
    movz x24, #0x4000, lsl #16
    cmp x22, x24
    b.hs el0_fault
    // ★ SPSR 白名单清洗：eret 把 SPSR 中的 M/DAIF/SS/IL 原样写入 PSTATE。
    // AArch64 的 EL0 可直接 `msr daifset`，且 rt_sigreturn 等路径的帧内容
    // 用户可控 —— 若不清洗，EL0 能以关中断状态运行（吞掉全部定时器 tick，
    // 单核整机冻结）甚至伪造 EL1 特权位。此处强制：EL0t（M[3:0]=0）、
    // 中断全开（DAIF bit9:6 清零）、SS/IL（bit21/20）清零；NZCV 等其余
    // 标志保留。
    bic x23, x23, #0xf
    bic x23, x23, #0x3c0
    bic x23, x23, #0x300000
    msr sp_el0, x21
    msr elr_el1, x22
    msr spsr_el1, x23
    // 恢复 FP/SIMD 上下文（v0-v31 + fpsr/fpcr）；x9 随后从帧中恢复
    ldp q0, q1, [sp, #0x110]
    ldp q2, q3, [sp, #0x130]
    ldp q4, q5, [sp, #0x150]
    ldp q6, q7, [sp, #0x170]
    ldp q8, q9, [sp, #0x190]
    ldp q10, q11, [sp, #0x1B0]
    ldp q12, q13, [sp, #0x1D0]
    ldp q14, q15, [sp, #0x1F0]
    ldp q16, q17, [sp, #0x210]
    ldp q18, q19, [sp, #0x230]
    ldp q20, q21, [sp, #0x250]
    ldp q22, q23, [sp, #0x270]
    ldp q24, q25, [sp, #0x290]
    ldp q26, q27, [sp, #0x2B0]
    ldp q28, q29, [sp, #0x2D0]
    ldp q30, q31, [sp, #0x2F0]
    ldr x9, [sp, #0x310]
    msr fpsr, x9
    ldr x9, [sp, #0x318]
    msr fpcr, x9

    ldp x0, x1, [sp, #0x00]
    ldp x2, x3, [sp, #0x10]
    ldp x4, x5, [sp, #0x20]
    ldp x6, x7, [sp, #0x30]
    ldp x8, x9, [sp, #0x40]
    ldp x10, x11, [sp, #0x50]
    ldp x12, x13, [sp, #0x60]
    ldp x14, x15, [sp, #0x70]
    ldp x16, x17, [sp, #0x80]
    ldp x18, x19, [sp, #0x90]
    ldp x20, x21, [sp, #0xa0]
    ldp x22, x23, [sp, #0xb0]
    ldp x24, x25, [sp, #0xc0]
    ldp x26, x27, [sp, #0xd0]
    ldp x28, x29, [sp, #0xe0]
    ldr x30, [sp, #0xf0]
    add sp, sp, #800
    eret

    // ===== EL0 用户程序故障（非 svc 同步异常）入口 =====
    // 陷阱帧已保存在 sp 处（与 syscall 布局一致），调用 Rust 处理器终止
    // 当前进程（SIGSEGV 语义），不返回——避免用户程序崩溃挂死整个内核。
    .type el0_fault, %function
el0_fault:
    mov x0, sp
    bl el0_fault_handler
    b .
"#
);
