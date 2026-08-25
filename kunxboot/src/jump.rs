//! 跳转到内核：关闭 MMU / 缓存 / 分支预测，设置 x0 = DTB 地址后跳转。

use core::arch::asm;

/// 跳转前的缓存维护：作废整条指令缓存（`ic iallu`），并按地址区间
/// clean+invalidate 数据缓存（`dc civac`），确保内核取指/读数据拿到的是
/// 设备 DMA 写入后的最新内容（真机加固：KVM/物理硬件上必需，TCG 下无害）。
///
/// # Safety
/// `regions` 必须指向合法可读的 (地址, 长度) 数组。
unsafe fn cache_maintenance(regions: &[(usize, usize)]) {
    // 指令缓存整条作废（内核代码将由 icache 重新填充）
    asm!("ic iallu");
    asm!("dsb sy");
    asm!("isb");

    // 数据缓存：对每个镜像区间按缓存行 clean + invalidate
    for &(addr, len) in regions {
        if len == 0 {
            continue;
        }
        let mut a = addr & !63usize;
        let end = (addr + len + 63) & !63usize;
        while a < end {
            asm!("dc civac, {}", in(reg) a);
            a += 64;
        }
    }
    asm!("dsb sy");
    asm!("isb");
}

/// 关闭 MMU、D-cache、I-cache 与 TLB，设置 x0=dtb 地址并跳转到 `entry`。
///
/// `regions` 为已加载镜像的 (地址, 长度) 列表（内核/DTB/ramdisk），跳转前
/// 对其做缓存维护，保证真机上 CPU 看到的是最新数据。
///
/// # Safety
/// 该函数不返回；调用后 CPU 控制权交给内核。
pub fn jump_to_kernel(dtb_addr: u64, entry: u64, regions: &[(usize, usize)]) -> ! {
    unsafe {
        cache_maintenance(regions);
        asm!(
            "dsb sy",
            "isb",
            // 关闭 MMU(M)、D-cache(C)、I-cache(I)
            "mrs x1, sctlr_el1",
            "bic x1, x1, #1",
            "bic x1, x1, #4",
            "bic x1, x1, #4096",
            "msr sctlr_el1, x1",
            "isb",
            // 使 TLB 失效（MMU 已关，确保无残留映射）
            "tlbi vmalle1",
            "dsb sy",
            "isb",
            // 按 AArch64 内核引导约定：x0 = 设备树物理地址
            "mov x0, {dtb}",
            "br {entry}",
            dtb = in(reg) dtb_addr,
            entry = in(reg) entry,
            options(noreturn)
        );
    }
}
