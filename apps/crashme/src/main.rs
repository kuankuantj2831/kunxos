// KunxOS 崩溃注入测试程序（开发者工具链验证用）。
//
// 用法：
//   crashme segv   -> 写地址 0（EL0 data abort → SIGSEGV tombstone）
//   crashme ill    -> 执行未定义指令（.word 0x00000000 → SIGILL tombstone）
//   crashme div0   -> 整数除零（SIGFPE，若内核映射该 EC）
//   crashme ok     -> 正常退出（对照）
//
// 崩溃后内核生成 /data/crash/tombstone_*.txt（或 /tmp/crash/），
// shell `crash` 命令可列出/查看。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

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
    // 从 argv 取子命令（argv[1]；argv 布局：&str 指针数组，末尾 0）
    let mode = unsafe {
        let mut p = _argv;
        if *p != 0 {
            p = p.add(1); // argv[1]
        }
        if *p != 0 {
            let s = core::str::from_utf8(core::slice::from_raw_parts(*p as *const u8, 64))
                .unwrap_or("ok");
            let len = s.as_bytes().iter().position(|&c| c == 0).unwrap_or(s.len());
            core::str::from_utf8(&s.as_bytes()[..len]).unwrap_or("ok")
        } else {
            "ok"
        }
    };

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
        _ => {
            print("[crashme] 正常（无崩溃）\n");
        }
    }
    0
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mv a1, sp",
    "    ld a0, 0(a1)",
    "    addi a1, a1, 8",
    "    call k_main",
    "    li a7, 93",
    "    ecall",
    "    j .",
);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: crashme\n");
        write_fd(2, &w.buf[..w.len]);
        asm!("li a7, 93", "ecall", options(nostack));
    }
    loop {}
}
