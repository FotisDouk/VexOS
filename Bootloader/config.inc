%ifndef IMAGE_SECTORS
    %error "IMAGE_SECTORS must be defined; build with `cargo xtask build`"
%endif

STAGE1_LOAD      equ 0x7C00
STAGE2_LOAD      equ 0x7E00
STAGE2_SECTORS   equ 16
KERNEL_LOAD      equ 0x100000

; KFS (kfs/src/lib.rs): the superblock follows the boot area and points at /system/kernel.sys.
; BOOT_SECTORS in kfs = 1 + STAGE2_SECTORS; the SB_* offsets must match kfs.
SUPERBLOCK_LBA   equ 1 + STAGE2_SECTORS
KFS_MAGIC        equ 0x5346544B          ; "KTFS"
SB_MAGIC         equ 0
SB_KERNEL_START  equ 28
SB_KERNEL_SECTORS equ 32

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

; BIOS call trampoline (bioscall.asm), copied here by the kernel. The request block layout
; (BC_*) must match `Block` in kernel/src/bios.rs; BIOSCALL_DATA + 0x40 holds a disk address
; packet, and the bounce buffer above carries disk data.
BIOSCALL_ADDR    equ 0x2000
BIOSCALL_DATA    equ 0x3000
BIOSCALL_STACK   equ 0x7000
BC_EAX           equ 0
BC_EBX           equ 4
BC_ECX           equ 8
BC_EDX           equ 12
BC_ESI           equ 16
BC_EDI           equ 20
BC_EBP           equ 24
BC_DS            equ 28
BC_ES            equ 30
BC_EFLAGS        equ 32
BC_INT           equ 36
BC_BIOS_MASKS    equ 37
BC_KERNEL_MASKS  equ 39
BC_KERNEL_BASES  equ 41
BC_PIT_DIVISOR   equ 44

BOOTINFO_MAGIC   equ 0x4F464E49544F4F42

BI_MAGIC         equ 0
BI_MMAP_ADDR     equ 8
BI_MMAP_COUNT    equ 16
BI_BOOT_DRIVE    equ 20
BI_SIZE          equ 24

%if STAGE2_LOAD + STAGE2_SECTORS * 512 > PML4_ADDR
    %error "stage2 would overlap the page tables"
%endif
%if MMAP_ADDR + MMAP_MAX_ENTRIES * MMAP_ENTRY_SIZE > BIOSCALL_ADDR
    %error "memory map buffer too large"
%endif
