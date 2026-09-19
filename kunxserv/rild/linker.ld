/* KunxOS rild 用户态 ELF 链接脚本（供内核 elf.rs 加载器解析）。
 *
 * 加载地址 0x400000，段布局与其它服务一致：.text+.rodata 只读可执行，
 * .data+.bss 读写（bss 由加载器清零）。C 库（libpdu.a）的 .text/.rodata/
 * .data/.bss 汇入对应段。
 */

ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    . = ALIGN(0x1000);

    .data : {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note .note.*)
    }
}
