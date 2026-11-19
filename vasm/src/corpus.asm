; Assembled by both NASM and vasm in the tests; the outputs must be identical.
; Jumps use `strict near` or `short`, because NASM shrinks plain `near` jumps itself.
bits 64
default rel
org 0x4000000

_start:
    mov rax, rbx
    mov eax, ebx
    mov ax, bx
    mov al, bl
    mov ah, bh
    mov r8, r15
    mov r9d, eax
    mov sil, dil
    mov rax, 5
    mov rax, -5
    mov rax, 0x123456789
    mov eax, 0xFFFFFFFF
    mov r12, 7
    mov cx, 1000
    mov dl, 'A'
    mov r10b, 3
    mov rax, [rbx]
    mov rax, [rbp]
    mov rax, [r12]
    mov rax, [r13]
    mov rax, [rsp]
    mov rax, [rsp + 8]
    mov [rsp + 8], rax
    mov rax, [rbx + rcx*8 + 16]
    mov rax, [rbx + rcx*8 - 0x1000]
    mov rax, [r8 + r9*2]
    mov rax, [rcx*4 + 0x100]
    mov eax, [rbx + 0x12345]
    mov byte [rbx + 8], 'A'
    mov word [rbx], 0x1234
    mov dword [rbx], 0x12345678
    mov qword [rbx], -1
    mov qword [rsi + rdi], 100
    mov rax, [data_label]
    mov [data_label], cl
    lea rsi, [message]
    lea rdi, [rbx + rcx*2 + 3]
    lea eax, [rax + 1]
    mov rax, cr0
    mov cr3, rax
    mov rbx, cr4

    add rax, rbx
    add rax, 1
    add rax, 1000
    add eax, 1000
    add al, 5
    add bl, 5
    add rbx, 1000
    sub rsp, 8
    sub rsp, 128
    and rax, 0xFF
    or ecx, 0x80000000
    xor eax, eax
    xor r11, r11
    cmp rax, -1
    cmp al, 'z'
    cmp byte [rsi], 0
    cmp qword [rbp - 8], 10
    cmp rcx, [rdx]
    adc rax, rdx
    sbb ecx, 3
    add [rbx], rax
    sub [rbx + 4], dword 7

    test rax, rax
    test al, 1
    test eax, 0x100
    test rbx, 0x10
    test byte [rdi], 0x80
    test [rdi], rcx

    inc rax
    inc byte [rbx]
    dec r9
    dec dword [rsp + 4]
    neg rax
    not ecx
    not byte [rbx]
    mul rcx
    imul rbx
    div rcx
    idiv qword [rsi]
    imul rax, rbx
    imul rax, rbx, 10
    imul rax, rbx, 1000
    imul eax, [rsi], 3
    imul rcx, 12

    shl rax, 1
    shl rax, 4
    shr eax, cl
    sar rdx, 63
    sar rdx, cl
    rol bl, 1
    ror word [rax], 2

    push rax
    push r12
    push 5
    push 1000
    push qword [rbx]
    pop rbx
    pop r15
    pop qword [rax + 8]

    movzx eax, bl
    movzx eax, byte [rbx]
    movzx rax, word [rsi + 2]
    movsx rax, byte [rsi]
    movsx ecx, dx
    movsxd rax, ecx
    xchg rax, rbx
    xchg rbx, rax
    xchg ecx, edx
    xchg [rbx], al
    cmovz rax, rbx
    cmovne ecx, [rsi]
    setz al
    setg byte [rbx]
    setb r8b

    in al, 0x60
    in al, dx
    in ax, dx
    in eax, dx
    out 0x80, al
    out dx, al
    out dx, eax
    int 0x80
    int3
    lgdt [rax]
    lidt [rbx + 8]
    invlpg [rcx]

    nop
    hlt
    cli
    sti
    cld
    std
    cqo
    cdq
    cdqe
    pushfq
    popfq
    cpuid
    rdtsc
    rdmsr
    wrmsr
    iretq
    leave
    pause
    ud2
    rep movsb
    rep stosq
    rep movsq
    repne scasb
    lodsb
    stosb

backward:
    jmp strict near backward
    jmp strict near forward
    jmp short backward
    jz strict near backward
    jne strict near forward
    jl short backward
    jge strict near forward
    call backward
    call forward
    call rax
    call qword [rbx]
    jmp rax
    jmp qword [rbx + 8]
    loop backward
    ret
    ret 8
forward:
    nop

message: db "Hello, world!", 10, 0
         db 'single', 0
data_label: dq 0x1122334455667788
    dw 0x1234, 5
    dd 0xDEADBEEF, -1
    dq message, forward - backward
    db 1, 2, 3
    align 8
    times 3 db 0xAA
    times 2 nop
length equ $ - message
    dq length
    resb 4
    dd $ - $$
