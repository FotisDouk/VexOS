; guess.asm - guess the secret number between 1 and 100.
; Shows reading input, parsing a number, loops and comparisons.

_start:
    rdtsc                       ; CPU cycle counter as a random seed (edx:eax)
    xor edx, edx                ; only use the low 32 bits
    mov ecx, 100
    div ecx                     ; edx = seed % 100
    lea r12d, [rdx + 1]         ; r12 = secret, 1-100
    xor r13d, r13d              ; r13 = number of guesses

    lea rdi, [intro]
    mov esi, intro_len
    call print

ask:
    lea rdi, [prompt]
    mov esi, prompt_len
    call print

    mov eax, 3                  ; 3 = read_line(buffer, size) -> length
    lea rdi, [line]
    mov esi, 16
    int 0x80
    test rax, rax
    jz ask
    mov r8, rax

    xor ebx, ebx                ; ebx = the number typed
    xor ecx, ecx                ; ecx = position in the line
.digit:
    cmp rcx, r8
    jae .parsed
    movzx edx, byte [line + rcx]
    sub edx, '0'
    cmp edx, 9
    ja not_a_number             ; anything but 0-9
    imul ebx, ebx, 10
    add ebx, edx
    inc rcx
    jmp .digit

.parsed:
    inc r13d
    cmp ebx, r12d
    je won
    jb .too_low
    lea rdi, [lower]
    mov esi, lower_len
    call print
    jmp ask
.too_low:
    lea rdi, [higher]
    mov esi, higher_len
    call print
    jmp ask

not_a_number:
    lea rdi, [numbers_only]
    mov esi, numbers_only_len
    call print
    jmp ask

won:
    lea rdi, [correct]
    mov esi, correct_len
    call print
    mov eax, 11                 ; 11 = print_dec(value)
    mov rdi, r13
    int 0x80
    lea rdi, [tries]
    mov esi, tries_len
    call print
    xor eax, eax
    ret

; print(rdi = text, rsi = length)
print:
    mov eax, 1
    int 0x80
    ret

intro:        db "I picked a number between 1 and 100. Ctrl-C gives up.", 10
intro_len     equ $ - intro
prompt:       db "Your guess: "
prompt_len    equ $ - prompt
lower:        db "Lower!", 10
lower_len     equ $ - lower
higher:       db "Higher!", 10
higher_len    equ $ - higher
numbers_only: db "Please type a number.", 10
numbers_only_len equ $ - numbers_only
correct:      db "Correct! You needed "
correct_len   equ $ - correct
tries:        db " guesses.", 10
tries_len     equ $ - tries
line:         resb 16
