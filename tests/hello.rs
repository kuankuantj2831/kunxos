//! Build the assembly syscall smoke test with the installed Rust RISC-V toolchain.
#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("../kunxcore/apps/hello.S"));

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop { core::hint::spin_loop(); }
}
