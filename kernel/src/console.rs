//! The console: one text screen per task, of which one is shown.
//!
//! Everything a task prints or draws goes to its own `Screen` (see `vga.rs`). The screen being
//! shown also writes through to VGA memory, and is mirrored to COM1: plain output as text, with
//! ANSI color codes when the colors change, and full-screen drawing with ANSI cursor
//! positioning, so a serial terminal shows the same screen.
//!
//! While the desktop runs, the bottom row (row 24) is the taskbar. It belongs to no screen:
//! screens then have 24 rows, and the serial terminal gets a scroll region of rows 1-24.

use crate::serial::{Serial, COM1};
use crate::sync::SpinLock;
use crate::task::{self, MAX_TASKS};
use crate::vga::{self, Color, Screen, DEFAULT_ATTR, HEIGHT, WIDTH};
use alloc::boxed::Box;
use core::fmt::{self, Write};

/// The taskbar row, while the desktop runs.
pub const BAR_ROW: usize = HEIGHT - 1;

struct Console {
    /// The boot task's screen, usable before the heap exists.
    boot: Screen,
    /// The other tasks' screens, by task slot (index 0 is unused).
    screens: [Option<Box<Screen>>; MAX_TASKS],
    /// The task slot whose screen is shown.
    visible: usize,
    serial: Serial,
    /// The colors the serial terminal is set to, as a VGA color byte.
    serial_attr: u8,
    /// The desktop runs: row 24 is the taskbar.
    desktop: bool,
    bar: [u16; WIDTH],
}

fn screen_of<'a>(boot: &'a mut Screen, screens: &'a mut [Option<Box<Screen>>], slot: usize) -> Option<&'a mut Screen> {
    if slot == task::BOOT {
        Some(boot)
    } else {
        screens.get_mut(slot)?.as_deref_mut()
    }
}

/// VGA color numbers in ANSI order: VGA 1 (blue) is ANSI 4, and so on.
const ANSI_COLOR: [u8; 8] = [0, 4, 2, 6, 1, 5, 3, 7];

/// Sets the serial terminal's colors to a VGA color byte. Light gray on black is the terminal's
/// own default colors.
fn write_sgr(serial: &mut Serial, attr: u8) {
    if attr == DEFAULT_ATTR {
        let _ = serial.write_str("\x1b[0m");
        return;
    }
    let fg = attr & 0x0F;
    let bg = (attr >> 4) & 0x0F;
    let fg_code = if fg < 8 { 30 } else { 90 } + ANSI_COLOR[(fg & 7) as usize];
    let bg_code = if bg < 8 { 40 } else { 100 } + ANSI_COLOR[(bg & 7) as usize];
    let _ = write!(serial, "\x1b[0;{fg_code};{bg_code}m");
}

/// The ASCII a serial terminal gets for a screen byte: code page 437 lines and blocks become
/// `+ - | #`, anything else that is not printable becomes `?`.
fn ascii(byte: u8) -> u8 {
    match byte {
        0x20..=0x7E => byte,
        0xB3 | 0xBA => b'|',
        0xC4 | 0xCD => b'-',
        0xB0..=0xB2 | 0xDB..=0xDF | 0xFE => b'#',
        0xB4..=0xB9 | 0xBB..=0xC3 | 0xC5..=0xCC | 0xCE..=0xDA => b'+',
        0x10 | 0x1A => b'>',
        0x11 | 0x1B => b'<',
        _ => b'?',
    }
}

impl Console {
    fn current(&mut self) -> (Option<&mut Screen>, bool, &mut Serial, &mut u8) {
        let slot = task::current();
        let visible = slot == self.visible;
        let Console { boot, screens, serial, serial_attr, .. } = self;
        (screen_of(boot, screens, slot), visible, serial, serial_attr)
    }

    /// Sends a run of cells to the serial terminal at the current terminal position.
    fn serial_cells(serial: &mut Serial, serial_attr: &mut u8, cells: impl Iterator<Item = u16>) {
        for cell in cells {
            let attr = (cell >> 8) as u8;
            if attr != *serial_attr {
                write_sgr(serial, attr);
                *serial_attr = attr;
            }
            serial.write_byte(ascii(cell as u8));
        }
    }

    /// Redraws one row of the shown screen (or the taskbar) on the serial terminal.
    fn serial_row(&mut self, row: usize) {
        let cells: [u16; WIDTH] = if row == BAR_ROW && self.desktop {
            self.bar
        } else {
            let visible = self.visible;
            let Console { boot, screens, .. } = self;
            match screen_of(boot, screens, visible) {
                Some(screen) => core::array::from_fn(|col| screen.cell(row, col)),
                None => [vga::cell(b' ', DEFAULT_ATTR); WIDTH],
            }
        };
        // Trailing blanks in the default colors are left to "erase to end of line".
        let blank = vga::cell(b' ', DEFAULT_ATTR);
        let len = cells.iter().rposition(|&c| c != blank).map_or(0, |i| i + 1);
        let _ = write!(self.serial, "\x1b[{};1H", row + 1);
        Self::serial_cells(&mut self.serial, &mut self.serial_attr, cells[..len].iter().copied());
        write_sgr(&mut self.serial, DEFAULT_ATTR);
        self.serial_attr = DEFAULT_ATTR;
        let _ = self.serial.write_str("\x1b[K");
    }

    /// Puts the shown screen on VGA and repaints the serial terminal with it.
    fn show(&mut self, slot: usize) {
        let old = self.visible;
        self.visible = slot;
        let desktop = self.desktop;
        let bar = self.bar;
        let Console { boot, screens, .. } = self;
        if let Some(screen) = screen_of(boot, screens, old) {
            screen.on_hardware = false;
        }
        let (rows, position, cursor) = match screen_of(boot, screens, slot) {
            Some(screen) => {
                screen.on_hardware = true;
                screen.sync();
                (screen.rows(), screen.position(), screen.cursor_visible())
            }
            None => (HEIGHT, (0, 0), false),
        };
        if desktop {
            for (col, &cell) in bar.iter().enumerate() {
                vga::hw_put(BAR_ROW, col, cell);
            }
        }
        let _ = self.serial.write_str("\x1b[0m\x1b[2J");
        self.serial_attr = DEFAULT_ATTR;
        if desktop {
            let _ = self.serial.write_str("\x1b[1;24r");
        }
        for row in 0..rows {
            self.serial_row(row);
        }
        if desktop {
            self.serial_row(BAR_ROW);
        }
        let _ = write!(self.serial, "\x1b[{};{}H{}", position.0 + 1, position.1 + 1, if cursor { "\x1b[?25h" } else { "\x1b[?25l" });
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let (screen, visible, serial, serial_attr) = self.current();
        let Some(screen) = screen else { return Ok(()) };
        screen.write_str(s)?;
        if visible {
            if *serial_attr != screen.attr() {
                write_sgr(serial, screen.attr());
                *serial_attr = screen.attr();
            }
            serial.write_str(s)?;
        }
        Ok(())
    }
}

static CONSOLE: SpinLock<Console> = SpinLock::new(Console {
    boot: Screen::new(),
    screens: [const { None }; MAX_TASKS],
    visible: task::BOOT,
    serial: Serial::new(COM1),
    serial_attr: DEFAULT_ATTR,
    desktop: false,
    bar: [vga::cell(b' ', DEFAULT_ATTR); WIDTH],
});

pub fn init() {
    let mut console = CONSOLE.lock();
    console.boot.on_hardware = true;
    console.boot.clear();
    console.boot.set_cursor_visible(true);
    console.serial.init();
}

/// Clears the current task's screen (and the serial terminal, if it is shown).
pub fn clear() {
    let mut console = CONSOLE.lock();
    let desktop = console.desktop;
    let (screen, visible, serial, serial_attr) = console.current();
    let Some(screen) = screen else { return };
    screen.clear();
    if visible {
        *serial_attr = DEFAULT_ATTR;
        if desktop {
            // Keep the taskbar: erase the rows above it one by one.
            let _ = serial.write_str("\x1b[0m");
            for row in 1..=BAR_ROW {
                let _ = write!(serial, "\x1b[{row};1H\x1b[K");
            }
            let _ = serial.write_str("\x1b[H");
        } else {
            let _ = serial.write_str("\x1b[0m\x1b[2J\x1b[H");
        }
    }
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

/// Draws one full row of the current task's screen without moving the scrolling output. On
/// serial this uses ANSI cursor positioning, so the terminal shows the same screen.
pub fn draw_row(row: usize, text: &[u8], style: Style) {
    let mut console = CONSOLE.lock();
    let (screen, visible, serial, serial_attr) = console.current();
    let Some(screen) = screen else { return };
    if row >= screen.rows() {
        return;
    }
    let normal = screen.attr();
    let attr = match style {
        Style::Normal => normal,
        Style::Dim => (normal & 0xF0) | Color::DarkGray as u8,
        Style::Inverse => normal.rotate_left(4),
    };
    screen.write_row(row, text, attr);
    if !visible {
        return;
    }
    let ansi = match style {
        Style::Normal => "",
        Style::Dim => "\x1b[90m",
        Style::Inverse => "\x1b[7m",
    };
    let _ = write!(serial, "\x1b[0m\x1b[{};1H{ansi}", row + 1);
    for &b in text.iter().take(WIDTH) {
        serial.write_byte(if (0x20..0x7F).contains(&b) { b } else { b'?' });
    }
    if style == Style::Inverse {
        for _ in text.len()..WIDTH {
            serial.write_byte(b' ');
        }
    }
    let _ = serial.write_str("\x1b[K\x1b[0m");
    *serial_attr = DEFAULT_ATTR;
}

/// Draws one full row of the current task's screen from colored pieces of text, padded with
/// blanks in `pad_attr`. Code page 437 symbols show on VGA; serial gets ASCII stand-ins.
pub fn draw_spans(row: usize, spans: &[(&[u8], u8)], pad_attr: u8) {
    let mut console = CONSOLE.lock();
    let (screen, visible, serial, serial_attr) = console.current();
    let Some(screen) = screen else { return };
    if row >= screen.rows() {
        return;
    }
    screen.write_spans(row, spans, pad_attr);
    if visible {
        let cells: [u16; WIDTH] = core::array::from_fn(|col| screen.cell(row, col));
        let _ = write!(serial, "\x1b[{};1H", row + 1);
        Console::serial_cells(serial, serial_attr, cells.iter().copied());
        write_sgr(serial, DEFAULT_ATTR);
        *serial_attr = DEFAULT_ATTR;
    }
}

/// Places the cursor (screen and serial terminal) for full-screen programs.
pub fn place_cursor(row: usize, col: usize) {
    let mut console = CONSOLE.lock();
    let (screen, visible, serial, _) = console.current();
    let Some(screen) = screen else { return };
    screen.set_position(row, col);
    if visible {
        let (row, col) = screen.position();
        let _ = write!(serial, "\x1b[{};{}H", row + 1, col + 1);
    }
}

/// Shows or hides the cursor of the current task's screen.
pub fn set_cursor_visible(on: bool) {
    let mut console = CONSOLE.lock();
    let (screen, visible, serial, _) = console.current();
    let Some(screen) = screen else { return };
    screen.set_cursor_visible(on);
    if visible {
        let _ = serial.write_str(if on { "\x1b[?25h" } else { "\x1b[?25l" });
    }
}

/// Rows of the current task's screen: 25, or 24 under the desktop.
pub fn rows() -> usize {
    let mut console = CONSOLE.lock();
    console.current().0.map_or(HEIGHT, |s| s.rows())
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
    if let (Some(screen), ..) = CONSOLE.lock().current() {
        screen.set_color(fg, bg);
    }
}

/// The current colors, to restore later with `set_attr`.
pub fn attr() -> u8 {
    CONSOLE.lock().current().0.map_or(DEFAULT_ATTR, |s| s.attr())
}

pub fn set_attr(attr: u8) {
    if let (Some(screen), ..) = CONSOLE.lock().current() {
        screen.set_attr(attr);
    }
}

pub unsafe fn force_unlock() {
    unsafe { CONSOLE.force_unlock() };
}

/// For the panic handler (after `force_unlock`): shows the current task's screen, or the boot
/// screen if it has none, so the message is seen and reaches serial.
pub fn show_current_for_panic() {
    let mut console = CONSOLE.lock();
    let slot = task::current();
    let Console { boot, screens, .. } = &mut *console;
    let slot = if screen_of(boot, screens, slot).is_some() { slot } else { task::BOOT };
    if console.visible != slot {
        console.show(slot);
    }
}

// --- Screens, for the scheduler and the desktop ---

/// Gives task `slot` a new, empty screen.
pub fn create_screen(slot: usize) {
    if slot == task::BOOT || slot >= MAX_TASKS {
        return;
    }
    let mut screen = Box::new(Screen::new());
    let mut console = CONSOLE.lock();
    screen.set_rows(if console.desktop { BAR_ROW } else { HEIGHT });
    console.screens[slot] = Some(screen);
}

/// Throws away task `slot`'s screen. If it was shown, the boot task's screen is shown instead.
pub fn remove_screen(slot: usize) {
    let old = {
        let mut console = CONSOLE.lock();
        if console.visible == slot {
            console.show(task::BOOT);
        }
        console.screens.get_mut(slot).and_then(Option::take)
    };
    drop(old);
}

/// The task slot whose screen is shown.
pub fn visible() -> usize {
    CONSOLE.lock().visible
}

/// Shows task `slot`'s screen (on VGA, and repainted on serial).
pub fn show(slot: usize) {
    CONSOLE.lock().show(slot);
}

/// Redraws the shown screen, e.g. after an overlay.
pub fn repaint() {
    let mut console = CONSOLE.lock();
    let visible = console.visible;
    console.show(visible);
}

/// Turns the desktop layout on or off: 24-row screens with the taskbar below, or 25 rows.
/// Repaints the shown screen.
pub fn set_desktop(on: bool) {
    let mut console = CONSOLE.lock();
    console.desktop = on;
    let rows = if on { BAR_ROW } else { HEIGHT };
    console.boot.set_rows(rows);
    for screen in console.screens.iter_mut().flatten() {
        screen.set_rows(rows);
    }
    console.bar = [vga::cell(b' ', DEFAULT_ATTR); WIDTH];
    if !on {
        // Back to a full-height terminal.
        let _ = console.serial.write_str("\x1b[r");
    }
    let visible = console.visible;
    console.show(visible);
}

/// Draws the taskbar (row 24) on VGA and serial. Does nothing unless the desktop runs.
pub fn draw_bar(spans: &[(&[u8], u8)], pad_attr: u8) {
    let mut console = CONSOLE.lock();
    if !console.desktop {
        return;
    }
    let mut bar = [vga::cell(b' ', pad_attr); WIDTH];
    let mut col = 0;
    for &(text, attr) in spans {
        for &b in text {
            if col < WIDTH {
                bar[col] = vga::cell(b, attr);
                col += 1;
            }
        }
    }
    if bar == console.bar {
        return;
    }
    console.bar = bar;
    for (col, &cell) in bar.iter().enumerate() {
        vga::hw_put(BAR_ROW, col, cell);
    }
    // Save the terminal's cursor, draw, and put it back.
    let _ = console.serial.write_str("\x1b7");
    let saved_attr = console.serial_attr;
    console.serial_row(BAR_ROW);
    let _ = console.serial.write_str("\x1b8");
    // `ESC 8` restores the colors too.
    console.serial_attr = saved_attr;
}

/// Draws colored text at `row`, `col` over the shown screen, on VGA and serial only: the
/// screens are not changed, and `repaint` removes it.
pub fn overlay(row: usize, col: usize, spans: &[(&[u8], u8)]) {
    let mut console = CONSOLE.lock();
    let mut cells = alloc::vec::Vec::new();
    for &(text, attr) in spans {
        cells.extend(text.iter().map(|&b| vga::cell(b, attr)));
    }
    cells.truncate(WIDTH.saturating_sub(col));
    for (i, &cell) in cells.iter().enumerate() {
        vga::hw_put(row, col + i, cell);
    }
    let _ = write!(console.serial, "\x1b[{};{}H", row + 1, col + 1);
    let Console { serial, serial_attr, .. } = &mut *console;
    Console::serial_cells(serial, serial_attr, cells.iter().copied());
    write_sgr(serial, DEFAULT_ATTR);
    *serial_attr = DEFAULT_ATTR;
}

/// A copy of task `slot`'s screen (the desktop saves the shell's screen this way).
pub fn snapshot(slot: usize) -> Option<Box<Screen>> {
    let screen = CONSOLE.lock().current_or(slot).cloned();
    screen.map(Box::new)
}

/// Puts back a screen saved with `snapshot`, keeping the current number of rows.
pub fn restore(slot: usize, saved: &Screen) {
    let mut console = CONSOLE.lock();
    let visible = console.visible == slot;
    let Console { boot, screens, .. } = &mut *console;
    if let Some(screen) = screen_of(boot, screens, slot) {
        let rows = screen.rows();
        *screen = saved.clone();
        screen.set_rows(rows);
        screen.on_hardware = false;
    }
    if visible {
        console.show(slot);
    }
}

/// The text of one row of task `slot`'s screen (for the self-tests).
pub fn row_text(slot: usize, row: usize) -> Option<alloc::string::String> {
    let mut console = CONSOLE.lock();
    let screen = console.current_or(slot)?;
    let bytes = screen.row_bytes(row);
    Some(alloc::string::String::from_utf8_lossy(&bytes).trim_end().into())
}

impl Console {
    fn current_or(&mut self, slot: usize) -> Option<&mut Screen> {
        let Console { boot, screens, .. } = self;
        screen_of(boot, screens, slot)
    }
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
