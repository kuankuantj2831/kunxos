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
mod sec_boot;
mod splash;
mod uart;

use uart::print_dec;

/// 修复后写入 DTB 的内存大小：128MB。
const RAM_SIZE: u64 = 128 * 1024 * 1024;

/// DTB 安全属性值暂存区（static，&'static 引用给 fixup 用）。
static mut PROP_ARENA: [u8; 256] = [0; 256];
static mut PROP_N: usize = 0;

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
    println!("[boot] DDR：跳过（QEMU）");

    // 启动画面：DDR 就绪后立即渲染，不等待文件系统/网络/其他驱动
    if splash::show() {
        println!("[boot] splash：ramfb 就绪");
    } else {
        println!("[boot] splash：无 ramfb，回退串口输出");
    }

    // 块存储
    if !mmc::init() {
        println!("[boot] MMC：初始化失败");
        hang();
    }
    println!("[boot] MMC：virtio-blk 初始化完成");

    // 安全启动链：BCB 槽位决策 + OTP 锚定 + KXSI 验签（含恢复模式）
    let plan = match sec_boot::secure_boot_flow() {
        Some(p) => p,
        None => {
            println!("[boot] 安全启动失败：无受信任镜像（请手动进入恢复模式）");
            hang();
        }
    };
    print!("[sec] boot slot ");
    if plan.slot == 0 {
        print!("A");
    } else {
        print!("B");
    }
    print!(" kind=");
    if matches!(plan.kind, sec_boot::BootKind::Recovery) {
        print!("recovery");
    } else {
        print!("normal");
    }
    print!(" build=");
    print_dec(plan.build as u64);
    println!(" （KXSI 已校验）");

    // 组装 DTB 安全属性：值暂存于静态 arena（&'static 引用，绕开栈自引用借用冲突）
    let mut props: [(&[u8], &[u8]); 6] = [(&[], &[]); 6];
    let mut n = 0usize;
    // arena 溢出必须硬失败：静默截断会在 DTB 里写入空属性值（无声数据损坏）
    fn arena(v: &[u8]) -> &'static [u8] {
        unsafe {
            let off = PROP_N;
            if off + v.len() > PROP_ARENA.len() {
                panic!("prop arena overflow");
            }
            PROP_ARENA[off..off + v.len()].copy_from_slice(&v[..v.len()]);
            PROP_N = off + v.len();
            &PROP_ARENA[off..off + v.len()]
        }
    }
    let mut bootargs_storage = [0u8; 64];
    let bargs: &[u8] = if matches!(plan.kind, sec_boot::BootKind::Recovery) {
        b"console=ttyAMA0 recovery"
    } else {
        b"console=ttyAMA0"
    };
    bootargs_storage[..bargs.len()].copy_from_slice(bargs);
    bootargs_storage[bargs.len()] = 0;
    props[n] = (b"bootargs", arena(&bootargs_storage[..bargs.len() + 1]));
    n += 1;
    props[n] = (b"kunx,secure-boot", arena(&1u32.to_be_bytes()));
    n += 1;
    props[n] = (b"kunx,boot-slot", arena(&(plan.slot as u32).to_be_bytes()));
    n += 1;
    props[n] = (b"kunx,build-version", arena(&plan.build.to_be_bytes()));
    n += 1;
    if matches!(plan.kind, sec_boot::BootKind::Recovery) {
        let mut cells = [0u8; 8];
        cells[0..4].copy_from_slice(&(load_kernel::RECOVERY_RAMDISK_LOAD_ADDR as u32).to_be_bytes());
        cells[4..8].copy_from_slice(&(plan.recovrd_len as u32).to_be_bytes());
        props[n] = (b"kunx,recovery-ramdisk", arena(&cells));
        n += 1;
    } else {
        let sha_src = unsafe {
            core::slice::from_raw_parts(load_kernel::RAMDISK_LOAD_ADDR as *const u8, plan.ramdisk_len)
        };
        props[n] = (b"kunx,ramdisk-sha256", arena(&kunxsec::sha256::sha256(sha_src)));
        n += 1;
    }

    // DTB 修复（bootargs/安全属性缺失时内核无法识别启动链，必须成功）
    let dtb_fixed_len = match dtb::fixup(load_kernel::DTB_LOAD_ADDR, RAM_SIZE, &props[..n]) {
        Some(len) => len,
        None => {
            println!("[boot] DTB 修正失败");
            hang();
        }
    };
    print!("[boot] DTB 修正：内存=");
    print_dec((RAM_SIZE / (1024 * 1024)) as u64);
    println!("MB，安全属性已注入");

    // 跳转内核（先做已加载镜像的缓存维护：真机取指/读数据必须是最新内容。
    // DTB 长度用 fixup 后的实际总长——属性注入会增长 DTB，用旧长度会漏掉
    // 增长尾部的 clean，内核读到陈旧数据）
    println!("[boot] 正在跳转内核…");
    let regions = [
        (load_kernel::KERNEL_LOAD_ADDR, plan.kernel_len),
        (load_kernel::DTB_LOAD_ADDR, dtb_fixed_len),
        (load_kernel::RAMDISK_LOAD_ADDR, plan.ramdisk_len),
        (load_kernel::RECOVERY_RAMDISK_LOAD_ADDR, plan.recovrd_len),
    ];
    jump::jump_to_kernel(
        load_kernel::DTB_LOAD_ADDR as u64,
        plan.entry,
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
    println!("\n[boot] 内核崩溃：{}", info);
    hang();
}
