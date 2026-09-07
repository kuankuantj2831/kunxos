/* QEMU virt AArch64 裸机 bootloader 链接脚本。
 *
 * bootloader 经 `-device loader,file=...,addr=0x46000000,cpu-num=0` 加载到
 * 0x46000000 并从此处开始执行。内核镜像约定加载到 0x40080000，随内核增长
 * （当前约 12.8MB，含静态进程表与内嵌字库）可延伸至 ~0x41000000；
 * bootloader 置于 0x46000000（128MB RAM 高区，避开内核/DTB/ramdisk 与
 * 启动画面帧缓冲），不再与内核镜像区域重叠。
 */

ENTRY(_start)

SECTIONS
{
    /* 加载地址 */
    . = 0x46000000;

    /* 栈顶：启动栈从 0x46000000 向下增长（朝向 0x45F00000） */
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
