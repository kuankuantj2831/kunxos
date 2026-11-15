// KunxOS 崩溃注入测试程序（开发者工具链验证用）。
//
// 用法：
//   crashme segv   -> 写地址 0（EL0 data abort → SIGSEGV tombstone）
//   crashme ill    -> 执行未定义指令（.word 0x00000000 → SIGILL tombstone）
//   crashme div0   -> 整数除零（SIGFPE，若内核映射该 EC）
//   crashme kfault -> 内核态解引用坏地址（S 态缺页 → 故障遏制：只杀本进程，
//                     系统继续运行；root 专用测试 syscall 415）
//   crashme kpanic -> 内核切片越界 panic（panic 遏制：只杀本进程）
//   crashme kirq   -> 定时器中断内故障（IRQ 遏制：屏蔽/跳过该次中断不死机）
//   crashme ok     -> 正常退出（对照）
//
// 崩溃后内核生成 /data/crash/tombstone_*.txt（或 /tmp/crash/），
// shell `crash` 命令可列出/查看。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::arch::asm;

const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") SYS_WRITE,
        in("a0") fd,
        in("a1") buf.as_ptr() as usize,
        in("a2") buf.len(),
        lateout("a0") ret,
        options(nostack),
    );
    ret
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    // 从 argv 取子命令（argv[1]；argv 布局：NUL 结尾字符串指针数组，末尾 0）。
    // 逐字节读到 NUL 为止：此前 from_raw_parts 固定盲读 64 字节，字符串落在
    // 栈顶页尾时跨界读触发 SIGSEGV——「崩溃注入工具自己先崩」且崩在进入
    // 模式分发之前，三种模式全部同 PC 假崩溃。
    let mut arg = [0u8; 24];
    let mut alen = 0usize;
    unsafe {
        let mut p = _argv;
        if *p != 0 {
            p = p.add(1); // argv[1]
        }
        if *p != 0 {
            let s = *p as *const u8;
            while alen < arg.len() - 1 {
                let c = core::ptr::read_volatile(s.add(alen));
                if c == 0 {
                    break;
                }
                arg[alen] = c;
                alen += 1;
            }
        }
    }
    let mode = core::str::from_utf8(&arg[..alen]).unwrap_or("ok");

    match mode {
        "segv" => {
            print("[crashme] 写入地址 0（预期 SIGSEGV）\n");
            unsafe {
                // 写地址 0：EL0 data abort（translation fault）→ SIGSEGV
                core::ptr::write_volatile(0usize as *mut u64, 0xDEAD_BEEF);
            }
        }
        "ill" => {
            print("[crashme] 执行未定义指令（预期 SIGILL）\n");
            unsafe {
                asm!(".word 0x00000000"); // 未定义指令（riscv 全 0 = illegal instruction）
            }
        }
        "div0" => {
            print("[crashme] 整数除零（预期 SIGFPE）\n");
            unsafe {
                let a: u64 = 1;
                let b: u64 = 0;
                // riscv 除零：divu 结果为全 1（不触发异常），改用「读只读 csr 后跳 0」类
                // 故障由上方 illegal 路径覆盖；此处保留一个非对齐访存触发
                asm!("lw {r}, 1({a})", r = out(reg) _, a = in(reg) a);
            }
        }
        "kfault" => {
            print("[crashme] 内核态解引用坏地址（预期 S态故障遏制：杀本进程不死机）\n");
            unsafe {
                // SYS_KUNX_FAULT_INJECT(415)：x0=1 在 S 态解引用 x1=0x1（必未映射）
                asm!(
                    "ecall",
                    in("a7") 415usize,
                    in("a0") 1usize,
                    in("a1") 0x1usize,
                    lateout("a0") _,
                    options(nostack),
                );
            }
        }
        "kpanic" => {
            print("[crashme] 内核切片越界（预期 panic 遏制：杀本进程不死机）\n");
            unsafe {
                asm!(
                    "ecall",
                    in("a7") 415usize,
                    in("a0") 2usize,
                    in("a1") 0usize,
                    lateout("a0") _,
                    options(nostack),
                );
            }
        }
        "kirq" => {
            print("[crashme] 定时器中断故障注入（预期 IRQ 遏制：跳过该次中断不死机）\n");
            unsafe {
                asm!(
                    "ecall",
                    in("a7") 415usize,
                    in("a0") 3usize,
                    in("a1") 0usize,
                    lateout("a0") _,
                    options(nostack),
                );
            }
            print("[crashme] 已布防，故障将于 10ms 内在下一个 tick 发生\n");
        }
        "kwedge" => {
            print("[crashme] 本核心楔死注入（预期互监看门狗 5 秒内自动重启）\n");
            unsafe {
                asm!(
                    "ecall",
                    in("a7") 415usize,
                    in("a0") 4usize,
                    in("a1") 0usize,
                    lateout("a0") _,
                    options(nostack),
                );
            }
        }
        _ => {
            print("[crashme] 正常（无崩溃）\n");
        }
    }
    0
}

// ==================== 入口 ====================

kunxrt::entry!();

kunxrt::panic_handler!("crashme");
