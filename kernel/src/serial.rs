use crate::port::{inb, outb};
use core::fmt;

pub const COM1: u16 = 0x3F8;

pub struct Serial {
    base: u16,
    present: bool,
}

impl Serial {
    pub const fn new(base: u16) -> Self {
        Self { base, present: false }
    }

    pub fn init(&mut self) -> bool {
        let b = self.base;
        unsafe {
            outb(b + 1, 0x00);
            outb(b + 3, 0x80);
            outb(b, 0x01);
            outb(b + 1, 0x00);
            outb(b + 3, 0x03);
            outb(b + 2, 0xC7);
            outb(b + 4, 0x1E);
            outb(b, 0xAE);
            self.present = inb(b) == 0xAE;
            outb(b + 4, 0x0F);
        }
        self.present
    }

    pub fn write_byte(&mut self, byte: u8) {
        if !self.present {
            return;
        }
        unsafe {
            for _ in 0..100_000 {
                if inb(self.base + 5) & 0x20 != 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            outb(self.base, byte);
        }
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}
