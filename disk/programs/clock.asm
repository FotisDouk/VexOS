; clock.asm - shows the time once a second until you press Ctrl-C.
;
; Uses system call 13 (time: seconds since 1970, as set with 'date -s') and
; 5 (sleep). On the desktop, start it in a terminal (or open it from the home
; screen) and switch away with Alt+Tab: it keeps counting while you do other
; things, because every program is a task of its own.

_start:
    xor r12d, r12d              ; r12 = how many times the time was shown

.tick:
    mov eax, 13                 ; 13 = time() -> seconds since 1970
    int 0x80
    xor edx, edx
    mov ecx, 86400
    div rcx                     ; rdx = seconds since midnight
    mov rax, rdx
    xor edx, edx
    mov ecx, 3600
    div rcx                     ; rax = hours, rdx = seconds into the hour
    mov rbx, rdx
    mov rdi, rax
    call two_digits
    call colon
    mov rax, rbx
    xor edx, edx
    mov ecx, 60
    div rcx                     ; rax = minutes, rdx = seconds
    mov rbx, rdx
    mov rdi, rax
    call two_digits
    call colon
    mov rdi, rbx
    call two_digits

    mov eax, 1                  ; 1 = print(text, length)
    lea rdi, [running]
    mov esi, running_len
    int 0x80
    inc r12
    mov eax, 11                 ; 11 = print_dec(number)
    mov rdi, r12
    int 0x80
    mov eax, 1
    lea rdi, [times]
    mov esi, times_len
    int 0x80

    mov eax, 5                  ; 5 = sleep(milliseconds)
    mov edi, 1000
    int 0x80
    jmp .tick

; Prints rdi (0-99) as two digits.
two_digits:
    cmp rdi, 10
    jae .print
    push rdi
    mov eax, 1
    lea rdi, [zero]
    mov esi, 1
    int 0x80
    pop rdi
.print:
    mov eax, 11
    int 0x80
    ret

colon:
    mov eax, 1
    lea rdi, [colon_char]
    mov esi, 1
    int 0x80
    ret

zero:         db "0"
colon_char:   db ":"
running:      db "   shown "
running_len   equ $ - running
times:        db " times (Ctrl-C stops)", 10
times_len     equ $ - times
