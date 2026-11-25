//! IDT, CPU exception handling and hardware IRQ dispatch.
//!
//! Vectors 0-31 are CPU exceptions; vectors 32-47 are the 8259 PIC's IRQ 0-15 (see `pic`).
//! Every vector enters through a small assembly stub that pushes an error code (a dummy 0 when
//! the CPU does not push one) and the vector number, then jumps to `isr_common`. That saves all
//! general-purpose registers and calls `interrupt_dispatch` with a pointer to the resulting
//! `InterruptFrame`. Returning from the dispatcher restores the (possibly modified) frame.

use crate::cpu::{self, DescriptorTablePointer};
use crate::{gdt, keyboard, pic, pit, program, task};
use crate::vga::Color;
use crate::{console, println};
use core::arch::{asm, global_asm};
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

const EXCEPTION_COUNT: usize = 32;
/// Number of vectors with a stub: the exceptions followed by the PIC IRQs.
const STUB_COUNT: usize = pic::IRQ_BASE as usize + pic::IRQ_COUNT as usize;
const _: () = assert!(pic::IRQ_BASE as usize == EXCEPTION_COUNT);

const VEC_NMI: u64 = 2;
const VEC_BREAKPOINT: u64 = 3;
const VEC_DOUBLE_FAULT: u64 = 8;
const VEC_INVALID_TSS: u64 = 10;
const VEC_SEGMENT_NOT_PRESENT: u64 = 11;
const VEC_STACK_FAULT: u64 = 12;
const VEC_GENERAL_PROTECTION: u64 = 13;
const VEC_PAGE_FAULT: u64 = 14;
const VEC_MACHINE_CHECK: u64 = 18;

const EXCEPTION_NAMES: [&str; EXCEPTION_COUNT] = [
    "#DE divide error",
    "#DB debug",
    "NMI",
    "#BP breakpoint",
    "#OF overflow",
    "#BR bound range exceeded",
    "#UD invalid opcode",
    "#NM device not available",
    "#DF double fault",
    "coprocessor segment overrun",
    "#TS invalid TSS",
    "#NP segment not present",
    "#SS stack-segment fault",
    "#GP general protection fault",
    "#PF page fault",
    "reserved",
    "#MF x87 floating-point error",
    "#AC alignment check",
    "#MC machine check",
    "#XM SIMD floating-point error",
    "#VE virtualization exception",
    "#CP control protection",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "#HV hypervisor injection",
    "#VC VMM communication",
    "#SX security exception",
    "reserved",
];

/// Register state saved by `isr_common`, lowest address first.
#[repr(C)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    // Pushed by the CPU.
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// `interrupt_dispatch` returns in rax the stack to resume (another task's after a switch) and in
// rdx the page tables to load first, or 0. Nothing touches the stack between loading CR3 and
// RSP, so the old stack may become unmapped.
global_asm!(
    r#"
    .pushsection .text.interrupts, "ax"

    .macro ISR_NOERR vec
    isr_stub_\vec:
        push 0
        push \vec
        jmp isr_common
    .endm

    .macro ISR_ERR vec
    isr_stub_\vec:
        push \vec
        jmp isr_common
    .endm

    .irp vec, 0,1,2,3,4,5,6,7,9,15,16,18,19,20,22,23,24,25,26,27,28,31
        ISR_NOERR \vec
    .endr
    .irp vec, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
        ISR_NOERR \vec
    .endr
    .irp vec, 8,10,11,12,13,14,17,21,29,30
        ISR_ERR \vec
    .endr
    .global isr_stub_128
    ISR_NOERR 128
    .global isr_stub_129
    ISR_NOERR 129

    isr_common:
        push rax
        push rbx
        push rcx
        push rdx
        push rsi
        push rdi
        push rbp
        push r8
        push r9
        push r10
        push r11
        push r12
        push r13
        push r14
        push r15
        mov rdi, rsp
        cld
        call {dispatch}
        test rdx, rdx
        jz 1f
        mov cr3, rdx
    1:
        mov rsp, rax
        pop r15
        pop r14
        pop r13
        pop r12
        pop r11
        pop r10
        pop r9
        pop r8
        pop rbp
        pop rdi
        pop rsi
        pop rdx
        pop rcx
        pop rbx
        pop rax
        add rsp, 16
        iretq

    .popsection

    .pushsection .rodata.isr_stub_table, "a"
    .balign 8
    .global isr_stub_table
    isr_stub_table:
    .irp vec, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        .quad isr_stub_\vec
    .endr
    .irp vec, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
        .quad isr_stub_\vec
    .endr
    .popsection
    "#,
    dispatch = sym interrupt_dispatch,
);

extern "C" {
    static isr_stub_table: [u64; STUB_COUNT];
    fn isr_stub_128();
    fn isr_stub_129();
}

/// Programs call the kernel with `int 0x80`.
pub const VEC_SYSCALL: u64 = 0x80;

#[derive(Clone, Copy)]
#[repr(C)]
struct Gate {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    _reserved: u32,
}

const _: () = assert!(size_of::<Gate>() == 16);

const GATE_INTERRUPT_DPL0: u8 = 0x8E;
const GATE_INTERRUPT_DPL3: u8 = 0xEE;
/// A trap gate leaves IF unchanged, so system calls run with interrupts enabled.
const GATE_TRAP_DPL3: u8 = 0xEF;

impl Gate {
    const MISSING: Gate = Gate {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        _reserved: 0,
    };

    fn new(handler: u64, ist: u8, type_attr: u8) -> Gate {
        Gate {
            offset_low: handler as u16,
            selector: gdt::KERNEL_CODE,
            ist,
            type_attr,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _reserved: 0,
        }
    }
}

static mut IDT: [Gate; 256] = [Gate::MISSING; 256];

pub fn init() {
    unsafe {
        let idt = &raw mut IDT;
        let stubs = &raw const isr_stub_table;
        for vector in 0..STUB_COUNT {
            let ist = match vector as u64 {
                VEC_DOUBLE_FAULT => gdt::IST_DOUBLE_FAULT,
                VEC_NMI => gdt::IST_NMI,
                VEC_MACHINE_CHECK => gdt::IST_MACHINE_CHECK,
                _ => 0,
            };
            let type_attr = if vector as u64 == VEC_BREAKPOINT {
                GATE_INTERRUPT_DPL3
            } else {
                GATE_INTERRUPT_DPL0
            };
            (*idt)[vector] = Gate::new((*stubs)[vector], ist, type_attr);
        }
        (*idt)[VEC_SYSCALL as usize] = Gate::new(isr_stub_128 as *const () as u64, 0, GATE_TRAP_DPL3);
        (*idt)[task::VEC_YIELD as usize] = Gate::new(isr_stub_129 as *const () as u64, 0, GATE_INTERRUPT_DPL0);

        let pointer = DescriptorTablePointer {
            limit: (size_of::<[Gate; 256]>() - 1) as u16,
            base: idt as u64,
        };
        asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }
}

// Expected-fault hook, used by the self-tests to check that faults are caught and reported.
// A test stores the vector it expects in EXPECT_VECTOR and the address to resume at in
// EXPECT_RESUME, then does the faulting access. The dispatcher records what it caught and
// continues at the resume address instead of treating the fault as fatal.
const NO_VECTOR: u64 = u64::MAX;
pub static EXPECT_VECTOR: AtomicU64 = AtomicU64::new(NO_VECTOR);
pub static EXPECT_RESUME: AtomicU64 = AtomicU64::new(0);
static CAUGHT_VECTOR: AtomicU64 = AtomicU64::new(NO_VECTOR);
static CAUGHT_CR2: AtomicU64 = AtomicU64::new(0);

/// A fault caught through the expected-fault hook.
pub struct CaughtFault {
    pub vector: u64,
    pub cr2: u64,
}

pub fn expect_fault(vector: u64) {
    CAUGHT_VECTOR.store(NO_VECTOR, Ordering::Relaxed);
    EXPECT_RESUME.store(0, Ordering::Relaxed);
    EXPECT_VECTOR.store(vector, Ordering::Release);
}

/// Disarms the hook and returns the fault it caught, if any.
pub fn take_caught_fault() -> Option<CaughtFault> {
    EXPECT_VECTOR.store(NO_VECTOR, Ordering::Relaxed);
    EXPECT_RESUME.store(0, Ordering::Relaxed);
    let vector = CAUGHT_VECTOR.swap(NO_VECTOR, Ordering::Acquire);
    (vector != NO_VECTOR).then(|| CaughtFault { vector, cr2: CAUGHT_CR2.load(Ordering::Relaxed) })
}

/// Returns where `isr_common` continues: the same frame, or another task's after a switch.
extern "C" fn interrupt_dispatch(frame: &mut InterruptFrame) -> task::Switch {
    let irq_base = pic::IRQ_BASE as u64;
    if (irq_base..irq_base + pic::IRQ_COUNT as u64).contains(&frame.vector) {
        let irq = (frame.vector - irq_base) as u8;
        handle_irq(irq);
        program::check_break(frame);
        let reason = if irq == pic::IRQ_TIMER { task::Reason::Tick } else { task::Reason::Irq };
        return task::schedule(frame, reason);
    }
    if frame.vector == task::VEC_YIELD {
        return task::schedule(frame, task::Reason::Yield);
    }
    let stay = task::Switch { rsp: frame as *mut InterruptFrame as u64, cr3: 0 };

    if frame.vector == VEC_SYSCALL {
        program::syscall(frame);
        return stay;
    }

    if frame.vector == VEC_BREAKPOINT {
        println!("Breakpoint at {:#x}", frame.rip);
        return stay;
    }

    if EXPECT_VECTOR.load(Ordering::Acquire) == frame.vector {
        let resume = EXPECT_RESUME.swap(0, Ordering::Relaxed);
        if resume != 0 {
            EXPECT_VECTOR.store(NO_VECTOR, Ordering::Relaxed);
            CAUGHT_CR2.store(cpu::read_cr2(), Ordering::Relaxed);
            CAUGHT_VECTOR.store(frame.vector, Ordering::Release);
            frame.rip = resume;
            return stay;
        }
    }

    // A fault while a program runs ends the program, not the kernel. NMI and machine checks
    // are hardware problems, so they stay fatal.
    if program::is_running() && !matches!(frame.vector, VEC_NMI | VEC_MACHINE_CHECK) {
        let saved = console::attr();
        print_exception(frame);
        console::set_attr(saved);
        program::abort_crashed(frame);
        return stay;
    }

    fatal_exception(frame)
}

/// Runs with interrupts disabled (the IDT entries are interrupt gates). Handlers must be short:
/// no printing, no blocking, and no locks, only atomics.
fn handle_irq(irq: u8) {
    if pic::is_spurious(irq) {
        return;
    }
    match irq {
        pic::IRQ_TIMER => pit::tick(),
        pic::IRQ_KEYBOARD => keyboard::handle_irq(),
        _ => {}
    }
    pic::eoi(irq);
}

fn fatal_exception(frame: &InterruptFrame) -> ! {
    print_exception(frame);
    let vector = frame.vector;
    let name = EXCEPTION_NAMES.get(vector as usize).copied().unwrap_or("unknown");
    panic!("unhandled CPU exception {vector} ({name})");
}

pub fn exception_name(vector: u64) -> &'static str {
    EXCEPTION_NAMES.get(vector as usize).copied().unwrap_or("unknown")
}

/// Prints the exception and a register dump in white on red.
fn print_exception(frame: &InterruptFrame) {
    // The fault may have happened while the console lock was held.
    unsafe { console::force_unlock() };
    console::set_color(Color::White, Color::Red);

    let vector = frame.vector;
    let name = EXCEPTION_NAMES.get(vector as usize).copied().unwrap_or("unknown");
    println!();
    println!("CPU EXCEPTION {vector} ({name}), error code {:#x}", frame.error_code);

    match vector {
        VEC_PAGE_FAULT => {
            let e = frame.error_code;
            println!(
                "  address {:#x}: {}, {} access, {} mode{}{}",
                cpu::read_cr2(),
                if e & 1 != 0 { "protection violation" } else { "page not present" },
                if e & 2 != 0 { "write" } else { "read" },
                if e & 4 != 0 { "user" } else { "kernel" },
                if e & 8 != 0 { ", reserved bit set" } else { "" },
                if e & 16 != 0 { ", instruction fetch" } else { "" },
            );
        }
        VEC_INVALID_TSS | VEC_SEGMENT_NOT_PRESENT | VEC_STACK_FAULT | VEC_GENERAL_PROTECTION
            if frame.error_code != 0 =>
        {
            let e = frame.error_code;
            let table = match (e >> 1) & 3 {
                0 => "GDT",
                2 => "LDT",
                _ => "IDT",
            };
            let external = if e & 1 != 0 { " (external)" } else { "" };
            println!("  selector: {table} index {}{external}", (e >> 3) & 0x1FFF);
        }
        _ => {}
    }

    let f = frame;
    println!("  RAX={:016x} RBX={:016x} RCX={:016x}", f.rax, f.rbx, f.rcx);
    println!("  RDX={:016x} RSI={:016x} RDI={:016x}", f.rdx, f.rsi, f.rdi);
    println!("  RBP={:016x} RSP={:016x} R8 ={:016x}", f.rbp, f.rsp, f.r8);
    println!("  R9 ={:016x} R10={:016x} R11={:016x}", f.r9, f.r10, f.r11);
    println!("  R12={:016x} R13={:016x} R14={:016x}", f.r12, f.r13, f.r14);
    println!("  R15={:016x} RIP={:016x} RFL={:016x}", f.r15, f.rip, f.rflags);
    println!("  CS={:04x} SS={:04x} CR2={:016x} CR3={:016x}", f.cs, f.ss, cpu::read_cr2(), cpu::read_cr3());
}
