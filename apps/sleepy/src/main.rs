// KunxOS 测试应用：kill(self, SIGKILL) 后调用 write——dispatch 应在 syscall
// 入口检查信号位图并终止（不输出 "alive"），验证 SIGKILL 投递。
#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
#[path = "../../../shared/ttf_text.rs"]
mod ttf_text;


const SYS_WRITE: usize = 64;
const SYS_KILL: usize = 129;
const SYS_GETPID: usize = 172;

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    ttf_text::font_init();
    let mypid: usize;
    unsafe {
        asm!("ecall", "mv {}, a0", out(reg) mypid, in("a7") SYS_GETPID, options(nostack));
    }
    // kill(self, 9)
    unsafe {
        asm!("ecall", in("a7") SYS_KILL, in("a0") mypid, in("a1") 9usize, options(nostack));
    }
    // 下一次系统调用：dispatch 应因 SIGKILL 挂起而终止进程
    let msg = b"alive\n";
    unsafe {
        asm!("ecall", in("a7") SYS_WRITE, in("a0") 1usize, in("a1") msg.as_ptr(), in("a2") msg.len(), options(nostack));
    }
    0
}

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
    // 保持原有行为：不主动 exit。
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
    let msg = b"PANIC: sleepy\n";
    let out: &[u8] = &w.buf[..w.len];
    unsafe {
        asm!("ecall", in("a7") SYS_WRITE, in("a0") 2usize, in("a1") msg.as_ptr(), in("a2") msg.len(), options(nostack));
        asm!("ecall", in("a7") SYS_WRITE, in("a0") 2usize, in("a1") out.as_ptr(), in("a2") out.len(), options(nostack));
    }
    loop {}
}
