//! QEMU's `isa-debug-exit` device, added by `cargo xtask test`. Writing `code` to its port makes
//! QEMU exit with status `(code << 1) | 1`, so Success = 33 and Failed = 35.

use crate::{cpu, port};

const DEBUG_EXIT_PORT: u16 = 0xF4;

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum QemuExit {
    Success = 0x10,
    Failed = 0x11,
}

pub fn exit(code: QemuExit) -> ! {
    unsafe { port::outl(DEBUG_EXIT_PORT, code as u32) };
    cpu::halt()
}
