/* Raspberry Pi 4 AArch64 裸机 bootloader 链接脚本（骨架，仅编译检查）。
 *
 * Pi 4 由 GPU 固件把 kernel8.img 加载到 0x80000 后进入 EL2 并跳转 _start。
 * 本脚本仅保证 Pi4 feature 编译通过，DDR/MMC 驱动均为骨架。
 */

ENTRY(_start)

SECTIONS
{
    . = 0x80000;

    _stack_top = .;

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    . = ALIGN(16);

    .rodata : {
        *(.rodata .rodata.*)
    }

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
