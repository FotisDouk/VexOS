%ifndef KERNEL_SECTORS
    %error "KERNEL_SECTORS must be defined; build with `cargo xtask build`"
%endif
%ifndef IMAGE_SECTORS
    %error "IMAGE_SECTORS must be defined; build with `cargo xtask build`"
%endif

STAGE1_LOAD      equ 0x7C00
STAGE2_LOAD      equ 0x7E00
STAGE2_SECTORS   equ 16
KERNEL_LBA       equ 1 + STAGE2_SECTORS
KERNEL_LOAD      equ 0x100000

BOOTINFO_ADDR    equ 0x1000
MMAP_ADDR        equ 0x1100
MMAP_MAX_ENTRIES equ 128
MMAP_ENTRY_SIZE  equ 24

PML4_ADDR        equ 0xA000
PDPT_ADDR        equ 0xB000
PD_ADDR          equ 0xC000

BOUNCE_SEG       equ 0x1000
BOUNCE_ADDR      equ 0x10000
BOUNCE_SECTORS   equ 64

BOOTINFO_MAGIC   equ 0x4F464E49544F4F42

BI_MAGIC         equ 0
BI_MMAP_ADDR     equ 8
BI_MMAP_COUNT    equ 16
BI_BOOT_DRIVE    equ 20
BI_SIZE          equ 24

%if STAGE2_LOAD + STAGE2_SECTORS * 512 > PML4_ADDR
    %error "stage2 would overlap the page tables"
%endif
%if MMAP_ADDR + MMAP_MAX_ENTRIES * MMAP_ENTRY_SIZE > 0x2000
    %error "memory map buffer too large"
%endif
