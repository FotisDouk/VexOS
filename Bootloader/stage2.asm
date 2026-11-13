%include "config.inc"

BITS 16
ORG STAGE2_LOAD

stage2_start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, STAGE1_LOAD
    sti
    cld
    mov [boot_drive], dl

    mov ax, 0x0003
    int 0x10

    mov si, msg_stage2
    call print

    call check_cpu
    call enable_a20
    call init_boot_info
    call read_memory_map
    call load_kernel
    call build_page_tables

    mov si, msg_jump
    call print

    cli
    lgdt [gdt_desc]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp CODE32_SEL:protected_mode

check_cpu:
    pushfd
    pop eax
    mov ecx, eax
    xor eax, 1 << 21
    push eax
    popfd
    pushfd
    pop eax
    push ecx
    popfd
    xor eax, ecx
    jz .fail

    mov eax, 0x80000000
    cpuid
    cmp eax, 0x80000001
    jb .fail
    mov eax, 0x80000001
    cpuid
    test edx, 1 << 29
    jz .fail
    ret
.fail:
    mov si, msg_no_long_mode
    jmp fatal

enable_a20:
    call a20_wait
    jnz .done

    mov ax, 0x2401
    int 0x15
    call a20_wait
    jnz .done

    call a20_keyboard_controller
    call a20_wait
    jnz .done

    in al, 0x92
    test al, 2
    jnz .last_check
    or al, 2
    and al, 0xFE                       ; bit 0 resets the machine
    out 0x92, al
.last_check:
    call a20_wait
    jnz .done

    mov si, msg_no_a20
    jmp fatal
.done:
    ret

a20_wait:
    push cx
    mov cx, 0x1000
.loop:
    call a20_enabled
    jnz .done
    loop .loop
    cmp al, al
.done:
    pop cx
    ret

a20_enabled:
    push ds
    push es
    push si
    push di
    push ax
    xor ax, ax
    mov ds, ax
    not ax
    mov es, ax
    mov si, 0x0500
    mov di, 0x0510
    mov al, [ds:si]
    push ax
    mov byte [ds:si], 0x00
    mov byte [es:di], 0xFF
    cmp byte [ds:si], 0xFF
    pop ax
    mov [ds:si], al
    pop ax
    pop di
    pop si
    pop es
    pop ds
    ret

a20_keyboard_controller:
    cli
    call kbc_wait_input
    mov al, 0xAD
    out 0x64, al
    call kbc_wait_input
    mov al, 0xD0
    out 0x64, al
    call kbc_wait_output
    in al, 0x60
    push ax
    call kbc_wait_input
    mov al, 0xD1
    out 0x64, al
    call kbc_wait_input
    pop ax
    or al, 2
    out 0x60, al
    call kbc_wait_input
    mov al, 0xAE
    out 0x64, al
    call kbc_wait_input
    sti
    ret

kbc_wait_input:
    push cx
    mov cx, 0xFFFF
.loop:
    in al, 0x64
    test al, 2
    jz .done
    loop .loop
.done:
    pop cx
    ret

kbc_wait_output:
    push cx
    mov cx, 0xFFFF
.loop:
    in al, 0x64
    test al, 1
    jnz .done
    loop .loop
.done:
    pop cx
    ret

init_boot_info:
    xor eax, eax
    mov di, BOOTINFO_ADDR
    mov cx, BI_SIZE / 4
    rep stosd
    mov dword [BOOTINFO_ADDR + BI_MAGIC], BOOTINFO_MAGIC & 0xFFFFFFFF
    mov dword [BOOTINFO_ADDR + BI_MAGIC + 4], BOOTINFO_MAGIC >> 32
    mov dword [BOOTINFO_ADDR + BI_MMAP_ADDR], MMAP_ADDR
    movzx eax, byte [boot_drive]
    mov [BOOTINFO_ADDR + BI_BOOT_DRIVE], eax
    ret

read_memory_map:
    mov di, MMAP_ADDR
    xor ebx, ebx
    xor bp, bp
.next:
    mov eax, 0xE820
    mov edx, 0x534D4150
    mov ecx, MMAP_ENTRY_SIZE
    mov dword [di + 20], 1
    int 0x15
    jc .end
    cmp eax, 0x534D4150
    jne .end
    cmp cl, 20
    jb .skip
    mov eax, [di + 8]
    or eax, [di + 12]
    jz .skip
    test byte [di + 20], 1
    jz .skip
    inc bp
    add di, MMAP_ENTRY_SIZE
    cmp bp, MMAP_MAX_ENTRIES
    je .end
.skip:
    test ebx, ebx
    jnz .next
.end:
    test bp, bp
    jz .fail
    mov [BOOTINFO_ADDR + BI_MMAP_COUNT], bp
    ret
.fail:
    mov si, msg_no_mmap
    jmp fatal

load_kernel:
    mov si, msg_loading
    call print
    mov dword [kernel_lba], KERNEL_LBA
    mov dword [kernel_dest], KERNEL_LOAD
    mov dword [kernel_left], KERNEL_SECTORS
.next_chunk:
    mov eax, [kernel_left]
    test eax, eax
    jz .done
    cmp eax, BOUNCE_SECTORS
    jbe .size_ok
    mov eax, BOUNCE_SECTORS
.size_ok:
    mov [chunk], ax

    mov eax, [kernel_lba]
    mov [dap.lba], eax
    mov dword [dap.lba + 4], 0
    mov word [dap.offset], 0
    mov word [dap.segment], BOUNCE_SEG
    call read_disk

    call enter_unreal                  ; BIOS calls can reset segment limits
    cli
    movzx ecx, word [chunk]
    shl ecx, 7
    mov esi, BOUNCE_ADDR
    mov edi, [kernel_dest]
    a32 rep movsd
    mov [kernel_dest], edi
    sti

    movzx eax, word [chunk]
    add [kernel_lba], eax
    sub [kernel_left], eax
    mov al, '.'
    call print_char
    jmp .next_chunk
.done:
    mov si, msg_crlf
    call print
    ret

read_disk:
    pushad
    mov di, 3
.try:
    mov ax, [chunk]
    mov [dap.count], ax
    mov si, dap
    mov dl, [boot_drive]
    mov ah, 0x42
    int 0x13
    jnc .ok
    xor ah, ah
    mov dl, [boot_drive]
    int 0x13
    dec di
    jnz .try
    mov si, msg_disk_error
    jmp fatal
.ok:
    popad
    ret

enter_unreal:
    pushf
    cli
    push ds
    push es
    lgdt [gdt_desc]
    mov eax, cr0
    or al, 1
    mov cr0, eax
    jmp short .pmode
.pmode:
    mov bx, DATA_SEL
    mov ds, bx
    mov es, bx
    and al, 0xFE
    mov cr0, eax
    pop es
    pop ds
    popf
    ret

build_page_tables:
    xor eax, eax
    mov di, PML4_ADDR
    mov ecx, 6 * 4096 / 4
    rep stosd

    mov dword [PML4_ADDR], PDPT_ADDR | 0x3

    mov di, PDPT_ADDR
    mov eax, PD_ADDR | 0x3
    mov cx, 4
.pdpt:
    mov [di], eax
    add eax, 0x1000
    add di, 8
    loop .pdpt

    mov di, PD_ADDR
    mov eax, 0x83
    mov cx, 4 * 512
.pd:
    mov [di], eax
    add eax, 0x200000
    add di, 8
    loop .pd
    ret

fatal:
    call print
.halt:
    cli
    hlt
    jmp .halt

print:
    pusha
.loop:
    lodsb
    test al, al
    jz .done
    call print_char
    jmp .loop
.done:
    popa
    ret

print_char:
    pusha
    mov ah, 0x0E
    xor bx, bx
    int 0x10
    popa
    ret

BITS 32
protected_mode:
    mov ax, DATA_SEL
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    mov esp, STAGE1_LOAD

    mov eax, cr4
    or eax, 1 << 5
    mov cr4, eax

    mov eax, PML4_ADDR
    mov cr3, eax

    mov ecx, 0xC0000080
    rdmsr
    or eax, 1 << 8
    wrmsr

    mov eax, cr0
    or eax, 1 << 31
    mov cr0, eax

    jmp CODE64_SEL:long_mode

BITS 64
long_mode:
    mov ax, DATA_SEL
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    mov rsp, STAGE1_LOAD

    mov edi, BOOTINFO_ADDR
    mov eax, KERNEL_LOAD
    jmp rax

align 8
gdt:
    dq 0
.code32:
    dq 0x00CF9A000000FFFF
.data:
    dq 0x00CF92000000FFFF
.code64:
    dq 0x00AF9A000000FFFF
.end:

CODE32_SEL equ gdt.code32 - gdt
DATA_SEL   equ gdt.data - gdt
CODE64_SEL equ gdt.code64 - gdt

gdt_desc:
    dw gdt.end - gdt - 1
    dd gdt

align 4
dap:
    db 0x10
    db 0
.count:   dw 0
.offset:  dw 0
.segment: dw 0
.lba:     dq 0

boot_drive:  db 0
chunk:       dw 0
kernel_lba:  dd 0
kernel_dest: dd 0
kernel_left: dd 0

msg_stage2:       db "Stage 2", 13, 10, 0
msg_loading:      db "Loading kernel", 0
msg_jump:         db "Entering long mode", 13, 10, 0
msg_crlf:         db 13, 10, 0
msg_no_long_mode: db "Error: CPU does not support 64-bit long mode", 0
msg_no_a20:       db "Error: could not enable the A20 line", 0
msg_no_mmap:      db "Error: BIOS returned no E820 memory map", 0
msg_disk_error:   db "Error: disk read failed", 0

    times STAGE2_SECTORS * 512 - ($ - $$) db 0
