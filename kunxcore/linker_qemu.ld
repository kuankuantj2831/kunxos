/* QEMU virt AArch64 裸机内核链接脚本 */

ENTRY(_start)

SECTIONS
{
    /* 加载地址：QEMU 将 raw 内核镜像加载到 0x40080000 */
    . = 0x40080000;

    /* 栈顶：内核启动栈从 0x40080000 向下增长（朝向 0x40000000） */
    _stack_top = .;

    /* 代码段：只读可执行 */
    .text : {
        _text_start = .;            /* 代码段起始（MMU 映射 RX 用） */
        KEEP(*(.text._start))       /* 启动代码放最前，保证入口位于 0x40080000 */
        KEEP(*(.text.secondary))    /* SMP secondary 入口：QEMU loader 引导至此，防 gc 剥离 */
        *(.text .text.*)
    }

    /* 段间按 4KB 对齐，保证页表映射边界为整页 */
    . = ALIGN(4096);

    /* 只读数据段 */
    .rodata : {
        _rodata_start = .;
        *(.rodata .rodata.*)
    }

    . = ALIGN(4096);

    /* 已初始化数据段：可读写 */
    .data : {
        _data_start = .;            /* 数据段起始（MMU 映射 RW 用） */
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
