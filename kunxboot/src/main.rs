#![no_std]
#![no_main]
// 全局状态以 static mut 组织，经 unsafe 访问（单核 + 无中断），放行 2021 版提示。
#![allow(static_mut_refs)]

use core::arch::global_asm;
use core::panic::PanicInfo;

mod ddr;
mod dtb;
mod fs_fat;
mod jump;
mod load_kernel;
mod mmc;
mod uart;

use uart::print_dec;

/// 修复后写入 DTB 的内存大小：128MB。
const RAM_SIZE: u64 = 128 * 1024 * 1024;

// ==================== 启动汇编 ====================

global_asm!(
    r#"
    .section .text._start, "ax"
    .global _start
    .type _start, %function
_start:
    // EL3/EL2 -> EL1
    mrs x1, CurrentEL
    lsr x1, x1, #2
    and x1, x1, #3
    cmp x1, #3
    beq 1f
    cmp x1, #2
    beq 2f
    b 3f
1:
    // EL3：配置 SCR_EL3（NS=1、HCE=1、RW=1）后 eret 到 EL2
    ldr x2, =0x531
    msr scr_el3, x2
    adr x2, 2f
    msr elr_el3, x2
    ldr x2, =0x3c9          // SPSR：EL2h、AArch64、DAIF=1
    msr spsr_el3, x2
    isb
    eret
2:
    // EL2：配置 HCR_EL2（RW=1）后 eret 到 EL1
    ldr x2, =0x80000000
    msr hcr_el2, x2
    adr x2, 3f
    msr elr_el2, x2
    ldr x2, =0x3c5          // SPSR：EL1h、AArch64、DAIF=1
    msr spsr_el2, x2
    isb
    eret
3:
    // EL1：关闭 MMU / 缓存
    mrs x2, sctlr_el1
    bic x2, x2, #1
    bic x2, x2, #4
    bic x2, x2, #4096
    msr sctlr_el1, x2
    isb
    // 使用 SP_EL1 并设置栈
    msr spsel, #1
    isb
    ldr x2, =_stack_top
    mov sp, x2
    // 清零 BSS
    ldr x2, =_bss_start
    ldr x3, =_bss_end
4:
    cmp x2, x3
    b.ge 5f
    str xzr, [x2], #8
    b 4b
5:
    // 使能 FP/SIMD（Rust 可能生成 NEON 指令）
    mov x2, #0x300000
    msr cpacr_el1, x2
    isb
    bl rust_main
    b .
"#
);

extern "C" {
    static _stack_top: u8;
    static _bss_start: u8;
    static _bss_end: u8;
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    uart::init();
    println!("KunxOS Bootloader v0.1");

    // DDR
    ddr::init();
    #[cfg(not(feature = "pi4"))]
    println!("[boot] DDR: skipped (QEMU)");

    // 块存储
    if !mmc::init() {
        println!("[boot] MMC: init FAILED");
        hang();
    }
    println!("[boot] MMC: virtio-blk init OK");

    // FAT32 分区
    let vol = match fs_fat::FatVolume::mount() {
        Some(v) => v,
        None => {
            println!("[boot] FAT32: mount FAILED");
            hang();
        }
    };
    println!("[boot] FAT32: mounted");

    // 加载镜像
    let loaded = match load_kernel::load_all(&vol) {
        Some(l) => l,
        None => {
            println!("[boot] load FAILED");
            hang();
        }
    };

    // DTB 修复
    dtb::fixup(load_kernel::DTB_LOAD_ADDR, RAM_SIZE);
    print!("[boot] DTB fixup: memory=");
    print_dec((RAM_SIZE / (1024 * 1024)) as u64);
    println!("MB");

    // 跳转内核（先做已加载镜像的缓存维护：真机取指/读数据必须是最新内容）
    println!("[boot] jumping to kernel...");
    let regions = [
        (load_kernel::KERNEL_LOAD_ADDR, loaded.kernel_size),
        (load_kernel::DTB_LOAD_ADDR, loaded.dtb_size),
        (load_kernel::RAMDISK_LOAD_ADDR, loaded.ramdisk_size),
    ];
    jump::jump_to_kernel(
        load_kernel::DTB_LOAD_ADDR as u64,
        load_kernel::KERNEL_LOAD_ADDR as u64,
        &regions,
    );
}

fn hang() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

// ==================== panic ====================

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\n[boot] PANIC: {}", info);
    hang();
}
