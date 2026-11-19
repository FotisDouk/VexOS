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

/// How a full-screen row is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Normal,
    /// Dimmed, e.g. the `~` markers past the end of a file.
    Dim,
    /// Swapped colors, e.g. a status bar.
    Inverse,
}

/// Draws one full row of the screen without moving the scrolling output. On serial this uses
/// ANSI cursor positioning, so the terminal shows the same screen.
pub fn draw_row(row: usize, text: &[u8], style: Style) {
    let mut console = CONSOLE.lock();
    let normal = console.vga.attr();
    let attr = match style {
        Style::Normal => normal,
        Style::Dim => (normal & 0xF0) | Color::DarkGray as u8,
        Style::Inverse => normal.rotate_left(4),
    };
    console.vga.write_row(row, text, attr);
    let ansi = match style {
        Style::Normal => "",
        Style::Dim => "\x1b[90m",
        Style::Inverse => "\x1b[7m",
    };
    let serial = &mut console.serial;
    let _ = write!(serial, "\x1b[{};1H{ansi}", row + 1);
    for &b in text.iter().take(crate::vga::WIDTH) {
        serial.write_byte(if (0x20..0x7F).contains(&b) { b } else { b'?' });
    }
    if style == Style::Inverse {
        for _ in text.len()..crate::vga::WIDTH {
            serial.write_byte(b' ');
        }
    }
    let _ = serial.write_str("\x1b[K\x1b[0m");
}

/// Places the cursor (screen and serial terminal) for full-screen programs.
pub fn place_cursor(row: usize, col: usize) {
    let mut console = CONSOLE.lock();
    console.vga.set_position(row, col);
    let _ = write!(console.serial, "\x1b[{};{}H", row + 1, col + 1);
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

/// The current VGA colors, to restore later with `set_attr`.
pub fn attr() -> u8 {
    CONSOLE.lock().vga.attr()
}

pub fn set_attr(attr: u8) {
    CONSOLE.lock().vga.set_attr(attr);
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
