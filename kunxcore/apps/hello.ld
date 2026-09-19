ENTRY(_start)
SECTIONS {
    . = 0x400000;
    .text : { *(.text*) *(.text.*) }
    .rodata : { *(.rodata*) *(.rodata.*) *(.srodata*) }
    .data : { *(.data*) *(.sdata*) }
    .bss : { *(.bss*) *(.sbss*) *(COMMON) }
}
