/* Raspberry Pi 4 AArch64 裸机内核链接脚本。
 *
 * Pi 4 由固件（bootcode/config.txt）加载 raw 内核镜像（kernel8.img）到
 * 0x80000（RAM 起始 0x00000000 之后的低地址），随后进入 EL2 并跳转 _start。
 * 段布局与 QEMU 版一致，仅加载地址与栈方向不同。
 */

ENTRY(_start)

SECTIONS
{
    /* 加载地址：Pi 4 固件将 kernel8.img 加载到 0x80000 */
    . = 0x80000;

    /* 栈顶：内核启动栈从 0x80000 向下增长（朝向 0x00000000） */
    _stack_top = .;

    /* 代码段：只读可执行 */
    .text : {
        _text_start = .;
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    . = ALIGN(4096);

    /* 只读数据段 */
    .rodata : {
        _rodata_start = .;
        *(.rodata .rodata.*)
    }

    . = ALIGN(4096);

    /* 已初始化数据段：可读写 */
    .data : {
        _data_start = .;
        *(.data .data.*)
    }

    . = ALIGN(4096);

    /* BSS 段：不占用镜像空间，由启动代码清零 */
    .bss (NOLOAD) : {
        _bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        _bss_end = .;
    }

    . = ALIGN(4096);
    _kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
