ENTRY(_start)

SECTIONS {
    . = 0x100000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.entry))
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
        *(.got .got.*)
    }

    .bss (NOLOAD) : ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __stack_bottom = .;
        . += 64K;
        __stack_top = .;
        __bss_end = .;
    }

    __kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
    }
}
