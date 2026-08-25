/* QEMU virt AArch64 裸机 bootloader 链接脚本。
 *
 * bootloader 经 `-device loader,file=...,addr=0x40200000,cpu-num=0` 加载到
 * 0x40200000 并从此处开始执行。QEMU 会把机器 DTB 放在 RAM 起始 0x40000000
 * （1MB），内核镜像约定加载到 0x40080000，因此 bootloader 避开该区域，
 * 置于 0x40200000。
 */

ENTRY(_start)

SECTIONS
{
    /* 加载地址 */
    . = 0x40200000;

    /* 栈顶：启动栈从 0x40200000 向下增长（朝向 0x40100000） */
    _stack_top = .;

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    . = ALIGN(16);

    .rodata : {
        *(.rodata .rodata.*)
    }

    /* .data 按 4096 对齐，保证 virtqueue 静态缓冲满足 virtio QueueAlign */
    .data : ALIGN(4096) {
        *(.data .data.*)
    }

    .bss (NOLOAD) : {
        _bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        _bss_end = .;
    }

    . = ALIGN(16);
    _bootloader_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
