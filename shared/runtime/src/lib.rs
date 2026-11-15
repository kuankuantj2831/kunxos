//! KunxOS native RV64 userspace support. No libc or third-party dependencies.
//! Rust core/alloc and compiler builtins are still supplied by the toolchain.
#![no_std]

pub mod heap;
pub mod memory;
pub mod process;
pub mod syscall;

#[cfg(test)]
extern crate std;

/// Register the reclaiming process allocator once in a guest executable.
#[macro_export]
macro_rules! allocator {
    () => {
        #[global_allocator]
        static KUNXRT_ALLOCATOR: $crate::heap::ProcessAllocator = $crate::heap::ProcessAllocator::new();
    };
}

/// RV64 ELF entry: preserve argc/argv before aligning the stack for the psABI.
/// k_main must be exported with C ABI and return its process exit status.
#[macro_export]
macro_rules! entry {
    () => {
        core::arch::global_asm!(
            ".section .text._start, \"ax\"", ".balign 4", ".global _start",
            ".type _start, @function", "_start:", "mv a1, sp", "ld a0, 0(a1)",
            "addi a1, a1, 8", "andi sp, sp, -16", "call k_main", "li a7, 93",
            "ecall", "1: j 1b", ".size _start, .-_start",
        );
    };
}

/// Uniform allocation-free panic diagnostics. Recovery can retain halt behavior.
#[macro_export]
macro_rules! panic_handler {
    ($label:literal) => {
        #[panic_handler]
        fn panic(info: &core::panic::PanicInfo) -> ! {
            $crate::process::report_panic($label, info);
            $crate::process::exit(1)
        }
    };
    ($label:literal, halt) => {
        #[panic_handler]
        fn panic(info: &core::panic::PanicInfo) -> ! {
            $crate::process::report_panic($label, info);
            loop { core::hint::spin_loop(); }
        }
    };
}
