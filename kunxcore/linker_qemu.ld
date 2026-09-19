/* QEMU virt RISC-V 64 裸机内核链接脚本
 *
 * 经 OpenSBI（fw_dynamic，`-bios default` 或显式 -bios）以 S 模式进入：
 * ELF 由 OpenSBI 按链接地址装载到 0x80200000，入口 a0=hartid、a1=DTB 地址。
 * DRAM 从 0x80000000 起，[0x80000000, 0x80200000) 为 OpenSBI 保护区
 * （PMP 禁止 S 模式访问），引导栈因此不放在内核下方，改用镜像尾部的
 * .bootstack NOLOAD 段（256KB，与调度器任务栈同规格：execve 深调用链）。 */

ENTRY(_start)

SECTIONS
{
    /* 加载地址：OpenSBI 将内核 ELF 装载到 0x80200000 */
    . = 0x80200000;

    /* 代码段：只读可执行 */
    .text : {
        _text_start = .;            /* 代码段起始（MMU 映射 RX 用） */
        KEEP(*(.text._start))       /* 启动代码放最前，保证入口位于 0x80200000 */
        KEEP(*(.text.trap_entry))   /* 统一陷入入口：stvec 常驻，防 gc 剥离 */
        *(.text .text.*)
    }

    /* 段间按 4KB 对齐，保证页表映射边界为整页 */
    . = ALIGN(4096);

    /* 只读数据段 */
    .rodata : {
        _rodata_start = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    . = ALIGN(4096);

    /* 已初始化数据段：可读写 */
    .data : {
        _data_start = .;            /* 数据段起始（MMU 映射 RW 用） */
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    . = ALIGN(4096);

    /* BSS 段：不占用镜像空间，由启动代码清零 */
    .bss (NOLOAD) : {
        _bss_start = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        _bss_end = .;
    }

    . = ALIGN(16);

    /* 引导栈（CPU0）：256KB NOLOAD，栈顶 = 段尾。
     * aarch64 版本把引导栈放在内核下方（0x40000000-0x40080000 空闲 RAM）；
     * RISC-V 上该区间是 OpenSBI PMP 保护区，栈随之并入镜像尾部，
     * 页分配器据 _kernel_end 起算，不会重复分配。 */
    .bootstack (NOLOAD) : {
        . = . + 0x40000;
        . = ALIGN(16);
        _stack_top = .;
    }

    /* 内核末尾（4KB 对齐）：页分配器据此标记内核区域已用 */
    . = ALIGN(4096);
    _kernel_end = .;

    /* 丢弃无用段，保持镜像精简 */
    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
