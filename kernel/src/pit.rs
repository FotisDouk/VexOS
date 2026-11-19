//! PIT channel 0 as the system tick.

use crate::cpu;
use crate::port::outb;
use core::sync::atomic::{AtomicU64, Ordering};

const PIT_FREQUENCY: u64 = 1_193_182;
pub const TICK_HZ: u64 = 100;

const CHANNEL0: u16 = 0x40;
const COMMAND: u16 = 0x43;
/// Channel 0, lobyte/hibyte access, mode 3 (square wave), binary.
const MODE_CHANNEL0_SQUARE: u8 = 0x36;

static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let divisor = (PIT_FREQUENCY / TICK_HZ) as u16;
    unsafe {
        outb(COMMAND, MODE_CHANNEL0_SQUARE);
        outb(CHANNEL0, divisor as u8);
        outb(CHANNEL0, (divisor >> 8) as u8);
    }
}

/// Called from the IRQ0 handler.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn uptime_ms() -> u64 {
    ticks() * 1000 / TICK_HZ
}

/// Sleeps for at least `ms` milliseconds (rounded up to whole ticks). Needs interrupts enabled.
pub fn sleep_ms(ms: u64) {
    assert!(cpu::interrupts_enabled(), "sleep_ms called with interrupts disabled");
    let wait = (ms * TICK_HZ).div_ceil(1000);
    // +1: the current tick is already partly over.
    let target = ticks() + wait + 1;
    while ticks() < target {
        cpu::hlt();
    }
}
