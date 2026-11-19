use core::arch::asm;

const RFLAGS_IF: u64 = 1 << 9;

/// Operand of `lgdt` / `lidt`.
#[repr(C, packed)]
pub struct DescriptorTablePointer {
    pub limit: u16,
    pub base: u64,
}

pub fn read_cr2() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

pub fn read_cr3() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

pub fn interrupts_enabled() -> bool {
    let rflags: u64;
    unsafe { asm!("pushfq", "pop {}", out(reg) rflags, options(nomem, preserves_flags)) };
    rflags & RFLAGS_IF != 0
}

/// Disables maskable interrupts. Also acts as a compiler barrier, so memory accesses are not
/// moved out of a critical section.
pub fn cli() {
    unsafe { asm!("cli", options(nostack)) };
}

pub fn sti() {
    unsafe { asm!("sti", options(nostack)) };
}

pub fn halt() -> ! {
    loop {
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Enables interrupts and halts until the next one. `sti` only takes effect after the following
/// instruction, so an interrupt that arrives in between still wakes the `hlt`. Call this with
/// interrupts disabled, right after checking that there is nothing to do, to avoid a lost wakeup.
pub fn enable_and_hlt() {
    unsafe { asm!("sti", "hlt", options(nostack)) };
}

/// Waits for the next interrupt. Interrupts must be enabled, or this never returns.
pub fn hlt() {
    unsafe { asm!("hlt", options(nostack)) };
}

pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

pub fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
    #[allow(unused_unsafe)]
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, subleaf) };
    CpuidResult { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
}

/// Resets the machine: first through the keyboard controller, then the chipset reset register
/// at 0xCF9, and as a last resort by triple faulting with an empty IDT.
pub fn reboot() -> ! {
    use crate::port::{inb, outb};
    cli();
    unsafe {
        for _ in 0..100_000 {
            if inb(0x64) & 2 == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        outb(0x64, 0xFE);
        spin_delay(1_000_000);
        outb(0xCF9, 0x02);
        outb(0xCF9, 0x06);
        spin_delay(1_000_000);
        let empty = DescriptorTablePointer { limit: 0, base: 0 };
        asm!("lidt [{}]", "int3", in(reg) &empty, options(noreturn));
    }
}

fn spin_delay(iterations: u32) {
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}
