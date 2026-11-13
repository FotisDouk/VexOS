use crate::serial::{Serial, COM1};
use crate::sync::SpinLock;
use crate::vga::{Color, Vga};
use core::fmt::{self, Write};

struct Console {
    vga: Vga,
    serial: Serial,
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.vga.write_str(s)?;
        self.serial.write_str(s)
    }
}

static CONSOLE: SpinLock<Console> =
    SpinLock::new(Console { vga: Vga::new(), serial: Serial::new(COM1) });

pub fn init() {
    let mut console = CONSOLE.lock();
    console.vga.clear();
    console.vga.hide_cursor();
    console.serial.init();
}

pub fn set_color(fg: Color, bg: Color) {
    CONSOLE.lock().vga.set_color(fg, bg);
}

pub unsafe fn force_unlock() {
    unsafe { CONSOLE.force_unlock() };
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = CONSOLE.lock().write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
