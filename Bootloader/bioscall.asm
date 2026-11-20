; BIOS call trampoline. The kernel copies this to BIOSCALL_ADDR and calls it from long mode
; (System V ABI, no arguments) after filling the request block at BIOSCALL_DATA. It drops to real
; mode, runs `int N` with the requested registers, stores the resulting registers and flags back
; into the block, and returns to long mode exactly as it was entered.
;
; Long mode -> compatibility mode -> paging off, LME off (32-bit protected mode) -> 16-bit
; protected mode -> real mode. On the way the PIC goes back to the BIOS vectors (8 / 0x70) and
; masks, and the PIT back to the BIOS rate (18.2 Hz), then everything is restored afterwards.
; Interrupts are enabled only while in real mode.

%include "config.inc"

ORG BIOSCALL_ADDR

CODE64_SEL equ thunk_gdt.code64 - thunk_gdt
CODE32_SEL equ thunk_gdt.code32 - thunk_gdt
DATA32_SEL equ thunk_gdt.data32 - thunk_gdt
CODE16_SEL equ thunk_gdt.code16 - thunk_gdt
DATA16_SEL equ thunk_gdt.data16 - thunk_gdt

EFER_MSR   equ 0xC0000080
EFER_LME   equ 1 << 8
EFER_LMA   equ 1 << 10

BITS 64
entry:
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    pushfq
    cli
    mov [saved_rsp], rsp
    sgdt [saved_gdt]
    sidt [saved_idt]
    mov ax, cs
    mov [saved_cs], ax
    mov ax, ds
    mov [saved_ds], ax
    mov ax, ss
    mov [saved_ss], ax
    mov rax, cr3
    mov [saved_cr3], eax
    mov rax, cr4
    mov [saved_cr4], eax
    mov ecx, EFER_MSR
    rdmsr
    and eax, ~EFER_LMA
    mov [saved_efer], eax
    mov [saved_efer + 4], edx

    lgdt [thunk_gdt_desc]
    push CODE32_SEL
    mov eax, compat32
    push rax
    o64 retf

BITS 32
compat32:
    mov ax, DATA32_SEL
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, cr0
    and eax, ~(1 << 31)                 ; paging off: leaves long mode
    mov cr0, eax
    mov ecx, EFER_MSR
    rdmsr
    and eax, ~EFER_LME
    wrmsr
    jmp CODE16_SEL:pmode16

BITS 16
pmode16:
    mov ax, DATA16_SEL
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, cr0
    and eax, ~1
    mov cr0, eax
    jmp 0:real_mode

real_mode:
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, BIOSCALL_STACK
    lidt [real_idt_desc]

    mov bx, 0x7008                      ; BIOS vectors: master 0x08, slave 0x70
    mov cl, [BIOSCALL_DATA + BC_BIOS_MASKS]
    mov ch, [BIOSCALL_DATA + BC_BIOS_MASKS + 1]
    call setup_pic
    xor ax, ax                          ; divisor 65536: the BIOS's 18.2 Hz
    call setup_pit

    mov al, [BIOSCALL_DATA + BC_INT]
    mov [int_insn + 1], al
    jmp short .flush                    ; flush the prefetch queue after patching
.flush:
    mov eax, [BIOSCALL_DATA + BC_EAX]
    mov ebx, [BIOSCALL_DATA + BC_EBX]
    mov ecx, [BIOSCALL_DATA + BC_ECX]
    mov edx, [BIOSCALL_DATA + BC_EDX]
    mov esi, [BIOSCALL_DATA + BC_ESI]
    mov edi, [BIOSCALL_DATA + BC_EDI]
    mov ebp, [BIOSCALL_DATA + BC_EBP]
    mov es, [BIOSCALL_DATA + BC_ES]
    mov ds, [BIOSCALL_DATA + BC_DS]     ; last: DS is needed to read the block
    sti
int_insn:
    int 0x00                            ; the vector byte is patched above
    cli
    pushfd
    push ds
    push word 0
    pop ds
    mov [BIOSCALL_DATA + BC_EAX], eax
    mov [BIOSCALL_DATA + BC_EBX], ebx
    mov [BIOSCALL_DATA + BC_ECX], ecx
    mov [BIOSCALL_DATA + BC_EDX], edx
    mov [BIOSCALL_DATA + BC_ESI], esi
    mov [BIOSCALL_DATA + BC_EDI], edi
    mov [BIOSCALL_DATA + BC_EBP], ebp
    mov [BIOSCALL_DATA + BC_ES], es
    pop word [BIOSCALL_DATA + BC_DS]
    pop dword [BIOSCALL_DATA + BC_EFLAGS]
    xor ax, ax
    mov es, ax

    mov bx, [BIOSCALL_DATA + BC_KERNEL_BASES]
    mov cx, [BIOSCALL_DATA + BC_KERNEL_MASKS]
    call setup_pic
    mov ax, [BIOSCALL_DATA + BC_PIT_DIVISOR]
    call setup_pit

    lgdt [thunk_gdt_desc]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword CODE32_SEL:pmode32

BITS 32
pmode32:
    mov ax, DATA32_SEL
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, [saved_cr4]
    mov cr4, eax
    mov eax, [saved_cr3]
    mov cr3, eax
    mov ecx, EFER_MSR
    mov eax, [saved_efer]
    mov edx, [saved_efer + 4]
    wrmsr
    mov eax, cr0
    or eax, 1 << 31                     ; paging on: back in long mode (compatibility)
    mov cr0, eax
    jmp CODE64_SEL:long64

BITS 64
long64:
    lgdt [saved_gdt]
    lidt [saved_idt]
    mov ax, [saved_ds]
    mov ds, ax
    mov es, ax
    mov ax, [saved_ss]
    mov ss, ax
    mov rsp, [saved_rsp]
    movzx rax, word [saved_cs]
    push rax
    mov rax, .kernel_cs
    push rax
    o64 retf
.kernel_cs:
    popfq
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret

BITS 16
; Reinitializes both PICs: BL = master vector base, BH = slave vector base, CL/CH = masks.
setup_pic:
    mov al, 0x11                        ; ICW1: init, ICW4 follows
    out 0x20, al
    call io_wait
    out 0xA0, al
    call io_wait
    mov al, bl
    out 0x21, al
    call io_wait
    mov al, bh
    out 0xA1, al
    call io_wait
    mov al, 4                           ; slave on IRQ2
    out 0x21, al
    call io_wait
    mov al, 2
    out 0xA1, al
    call io_wait
    mov al, 1                           ; 8086 mode
    out 0x21, al
    call io_wait
    out 0xA1, al
    call io_wait
    mov al, cl
    out 0x21, al
    mov al, ch
    out 0xA1, al
    ret

; Programs PIT channel 0 as a rate generator with divisor AX (0 = 65536).
setup_pit:
    push ax
    mov al, 0x36
    out 0x43, al
    pop ax
    out 0x40, al
    mov al, ah
    out 0x40, al
    ret

io_wait:
    push ax
    xor al, al
    out 0x80, al
    pop ax
    ret

align 8
thunk_gdt:
    dq 0
.code64: dq 0x00AF9A000000FFFF
.code32: dq 0x00CF9A000000FFFF
.data32: dq 0x00CF92000000FFFF
.code16: dq 0x00009A000000FFFF
.data16: dq 0x000092000000FFFF
.end:

thunk_gdt_desc:
    dw thunk_gdt.end - thunk_gdt - 1
    dq thunk_gdt

real_idt_desc:
    dw 0x3FF
    dq 0

align 8
saved_rsp:  dq 0
saved_gdt:  times 10 db 0
saved_idt:  times 10 db 0
saved_cs:   dw 0
saved_ds:   dw 0
saved_ss:   dw 0
saved_cr3:  dd 0
saved_cr4:  dd 0
saved_efer: dq 0

%if $ - $$ > BIOSCALL_DATA - BIOSCALL_ADDR
    %error "bioscall.asm is too large for the space before BIOSCALL_DATA"
%endif
