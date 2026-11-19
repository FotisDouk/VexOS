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
    console.vga.show_cursor();
    console.serial.init();
}

/// Clears the screen, and the serial terminal through an ANSI escape sequence.
pub fn clear() {
    let mut console = CONSOLE.lock();
    console.vga.clear();
    let _ = console.serial.write_str("\x1b[2J\x1b[H");
}

pub fn serial_has_input() -> bool {
    CONSOLE.lock().serial.has_input()
}

pub fn read_serial() -> Option<u8> {
    CONSOLE.lock().serial.try_read()
}

/// `fmt::Write` sink for the console, for code that writes to a `&mut dyn Write`.
pub struct Stdout;

impl Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        CONSOLE.lock().write_str(s)
    }
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
