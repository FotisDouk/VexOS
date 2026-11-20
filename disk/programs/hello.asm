; hello.asm - prints a greeting, plus any words you give it.
;
;   hello.asm               run the source directly (assembled in memory)
;   hello.asm some words    ...with arguments
;   asm hello.asm           build hello.vex, then run it with just: hello
;
; System calls: put the number in rax and the arguments in rdi, rsi, rdx, r10,
; then `int 0x80`. The result comes back in rax. See /docs/syscalls.txt.

_start:
    mov eax, 1                  ; 1 = print(text, length)
    lea rdi, [greeting]
    mov esi, greeting_len
    int 0x80

    mov eax, 8                  ; 8 = args(buffer, size) -> length
    lea rdi, [args]
    mov esi, 256
    int 0x80
    test rax, rax
    jz .done                    ; no arguments

    mov rbx, rax                ; keep the length
    mov eax, 1
    lea rdi, [you_said]
    mov esi, you_said_len
    int 0x80
    mov eax, 1
    lea rdi, [args]
    mov rsi, rbx
    int 0x80
    mov eax, 1
    lea rdi, [newline]
    mov esi, 1
    int 0x80

.done:
    xor eax, eax                ; exit code 0
    ret

greeting:     db "Hello from an assembly program!", 10
greeting_len  equ $ - greeting
you_said:     db "You said: "
you_said_len  equ $ - you_said
newline:      db 10
args:         resb 256
