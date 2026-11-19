//! The kernel's own GDT and TSS, replacing the temporary GDT that stage2 left in low memory.

use crate::cpu::DescriptorTablePointer;
use core::arch::asm;
use core::mem::size_of;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
// Ring 3 selectors. The data-before-code order is what `sysret` expects.
#[allow(dead_code)]
pub const USER_DATA: u16 = 0x18 | 3;
#[allow(dead_code)]
pub const USER_CODE: u16 = 0x20 | 3;
pub const TSS: u16 = 0x28;

/// Interrupt stack table slots (1-based, as stored in IDT gates).
pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MACHINE_CHECK: u8 = 3;

const IST_STACK_SIZE: usize = 16 * 1024;

#[repr(C, packed)]
struct TaskStateSegment {
    _reserved0: u32,
    rsp: [u64; 3],
    _reserved1: u64,
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iomap_base: u16,
}

const _: () = assert!(size_of::<TaskStateSegment>() == 104);

#[repr(C, align(16))]
struct Stack([u8; IST_STACK_SIZE]);

static mut DOUBLE_FAULT_STACK: Stack = Stack([0; IST_STACK_SIZE]);
static mut NMI_STACK: Stack = Stack([0; IST_STACK_SIZE]);
static mut MACHINE_CHECK_STACK: Stack = Stack([0; IST_STACK_SIZE]);

static mut TSS_SEGMENT: TaskStateSegment = TaskStateSegment {
    _reserved0: 0,
    rsp: [0; 3],
    _reserved1: 0,
    ist: [0; 7],
    _reserved2: 0,
    _reserved3: 0,
    iomap_base: size_of::<TaskStateSegment>() as u16,
};

static mut GDT: [u64; 7] = [
    0,                     // 0x00 null
    0x00AF_9A00_0000_FFFF, // 0x08 kernel code, 64-bit
    0x00CF_9200_0000_FFFF, // 0x10 kernel data
    0x00CF_F200_0000_FFFF, // 0x18 user data, DPL 3
    0x00AF_FA00_0000_FFFF, // 0x20 user code, 64-bit, DPL 3
    0,                     // 0x28 TSS, low half (filled in by init)
    0,                     // 0x30 TSS, high half
];

fn stack_top(stack: *const Stack) -> u64 {
    stack as u64 + IST_STACK_SIZE as u64
}

pub fn init() {
    unsafe {
        let mut ist = [0u64; 7];
        ist[IST_DOUBLE_FAULT as usize - 1] = stack_top(&raw const DOUBLE_FAULT_STACK);
        ist[IST_NMI as usize - 1] = stack_top(&raw const NMI_STACK);
        ist[IST_MACHINE_CHECK as usize - 1] = stack_top(&raw const MACHINE_CHECK_STACK);
        (&raw mut TSS_SEGMENT).write(TaskStateSegment {
            _reserved0: 0,
            rsp: [0; 3],
            _reserved1: 0,
            ist,
            _reserved2: 0,
            _reserved3: 0,
            iomap_base: size_of::<TaskStateSegment>() as u16,
        });

        // 64-bit available TSS descriptor (type 0x9, present).
        let base = &raw const TSS_SEGMENT as u64;
        let limit = size_of::<TaskStateSegment>() as u64 - 1;
        let low = (limit & 0xFFFF)
            | (base & 0xFF_FFFF) << 16
            | 0x89 << 40
            | ((limit >> 16) & 0xF) << 48
            | ((base >> 24) & 0xFF) << 56;
        let gdt = &raw mut GDT;
        (*gdt)[TSS as usize / 8] = low;
        (*gdt)[TSS as usize / 8 + 1] = base >> 32;

        let pointer = DescriptorTablePointer {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: gdt as u64,
        };
        asm!(
            "lgdt [{gdtr}]",
            // Reload CS with a far return: push the selector and the target, then retfq.
            "push {code}",
            "lea {tmp}, [rip + 55f]",
            "push {tmp}",
            "retfq",
            "55:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "mov fs, {zero:x}",
            "mov gs, {zero:x}",
            "ltr {tss:x}",
            gdtr = in(reg) &pointer,
            code = const KERNEL_CODE as u64,
            data = in(reg) KERNEL_DATA,
            zero = in(reg) 0u16,
            tss = in(reg) TSS,
            tmp = out(reg) _,
        );
    }
}
