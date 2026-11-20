//! The two cascaded 8259 PICs. The BIOS leaves the master on vectors 8-15, which collide with CPU
//! exceptions, so `init` remaps the master to `IRQ_BASE` and the slave to `IRQ_BASE + 8`.

use crate::port::{inb, io_wait, outb};
use core::sync::atomic::{AtomicU8, Ordering};

const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_CMD: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;

const ICW1_INIT_ICW4: u8 = 0x11;
const ICW4_8086: u8 = 0x01;
const OCW3_READ_ISR: u8 = 0x0B;
const EOI: u8 = 0x20;

pub const IRQ_BASE: u8 = 32;
pub const IRQ_COUNT: u8 = 16;
pub const IRQ_TIMER: u8 = 0;
pub const IRQ_KEYBOARD: u8 = 1;

/// IRQs that are unmasked. Everything on the slave stays masked for now.
const ENABLED_IRQS: u16 = 1 << IRQ_TIMER | 1 << IRQ_KEYBOARD;

/// The kernel's (master, slave) masks.
pub const KERNEL_MASKS: [u8; 2] = [!(ENABLED_IRQS as u8), !((ENABLED_IRQS >> 8) as u8)];

/// The masks the BIOS had set up, saved by `init` and restored during BIOS calls.
static BIOS_MASKS: [AtomicU8; 2] = [AtomicU8::new(0xFF), AtomicU8::new(0xFF)];

pub fn bios_masks() -> [u8; 2] {
    [BIOS_MASKS[0].load(Ordering::Relaxed), BIOS_MASKS[1].load(Ordering::Relaxed)]
}

pub fn init() {
    unsafe {
        BIOS_MASKS[0].store(inb(MASTER_DATA), Ordering::Relaxed);
        BIOS_MASKS[1].store(inb(SLAVE_DATA), Ordering::Relaxed);
        outb(MASTER_CMD, ICW1_INIT_ICW4);
        io_wait();
        outb(SLAVE_CMD, ICW1_INIT_ICW4);
        io_wait();
        outb(MASTER_DATA, IRQ_BASE);
        io_wait();
        outb(SLAVE_DATA, IRQ_BASE + 8);
        io_wait();
        outb(MASTER_DATA, 1 << 2); // slave on IRQ2
        io_wait();
        outb(SLAVE_DATA, 2); // cascade identity
        io_wait();
        outb(MASTER_DATA, ICW4_8086);
        io_wait();
        outb(SLAVE_DATA, ICW4_8086);
        io_wait();
        outb(MASTER_DATA, KERNEL_MASKS[0]);
        outb(SLAVE_DATA, KERNEL_MASKS[1]);
    }
}

fn in_service(cmd_port: u16) -> u8 {
    unsafe {
        outb(cmd_port, OCW3_READ_ISR);
        inb(cmd_port)
    }
}

/// IRQ7 and IRQ15 can be raised spuriously (for example when a line drops before the CPU
/// acknowledges it). A spurious IRQ is not in service and must not get an EOI from its own PIC;
/// a spurious IRQ15 still needs an EOI on the master, which did see a real cascade interrupt.
pub fn is_spurious(irq: u8) -> bool {
    match irq {
        7 => in_service(MASTER_CMD) & 0x80 == 0,
        15 => {
            let spurious = in_service(SLAVE_CMD) & 0x80 == 0;
            if spurious {
                unsafe { outb(MASTER_CMD, EOI) };
            }
            spurious
        }
        _ => false,
    }
}

pub fn eoi(irq: u8) {
    unsafe {
        if irq >= 8 {
            outb(SLAVE_CMD, EOI);
        }
        outb(MASTER_CMD, EOI);
    }
}
