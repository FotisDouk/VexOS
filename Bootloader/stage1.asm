%include "config.inc"

BITS 16
ORG STAGE1_LOAD

    jmp short start
    nop
    ; BPB area: some BIOSes overwrite it when booting from USB
    times 90 - ($ - $$) db 0

start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, STAGE1_LOAD
    sti
    jmp 0x0000:.normalized_cs
.normalized_cs:
    cld
    mov [boot_drive], dl

    mov si, msg_boot
    call print

    mov ah, 0x41
    mov bx, 0x55AA
    mov dl, [boot_drive]
    int 0x13
    jc .no_lba
    cmp bx, 0xAA55
    jne .no_lba
    test cx, 1
    jz .no_lba

    mov di, 3
.read:
    mov word [dap.count], STAGE2_SECTORS
    mov si, dap
    mov dl, [boot_drive]
    mov ah, 0x42
    int 0x13
    jnc .loaded
    xor ah, ah
    mov dl, [boot_drive]
    int 0x13
    dec di
    jnz .read

    mov si, msg_disk_error
    jmp fatal

.no_lba:
    mov si, msg_no_lba
    jmp fatal

.loaded:
    mov dl, [boot_drive]
    jmp 0x0000:STAGE2_LOAD

fatal:
    call print
.halt:
    cli
    hlt
    jmp .halt

print:
    pusha
    mov ah, 0x0E
    xor bx, bx
.loop:
    lodsb
    test al, al
    jz .done
    int 0x10
    jmp .loop
.done:
    popa
    ret

boot_drive:     db 0
msg_boot:       db "Stage 1", 13, 10, 0
msg_disk_error: db "Disk read error", 0
msg_no_lba:     db "No LBA disk support", 0

align 4
dap:
    db 0x10
    db 0
.count:
    dw STAGE2_SECTORS
    dw STAGE2_LOAD
    dw 0
    dq 1

    times 0x1B8 - ($ - $$) db 0
    dd 0x4B54534B
    dw 0

    db 0x80
    db 0x00, 0x02, 0x00
    db 0x7F
    db 0xFE, 0xFF, 0xFF
    dd 1
    dd IMAGE_SECTORS - 1
    times 3 * 16 db 0

    dw 0xAA55
