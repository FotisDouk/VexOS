//! The console: one text screen per task, of which one is shown.
//!
//! Everything a task prints or draws goes to its own `Screen` (see `vga.rs`). The screen being
//! shown also writes through to the display (`display.rs`: VGA text memory or a graphics mode),
//! and is mirrored to COM1: plain output as text, with ANSI color codes when the colors change,
//! and full-screen drawing with ANSI cursor positioning, so a serial terminal shows the same
//! screen.
//!
//! Screens are as large as the display's text grid, and follow it when the mode changes
//! (`set_display`). While the desktop runs, the bottom row is the taskbar. It belongs to no
//! screen: screens then have one row less, and the serial terminal gets a scroll region above it.

use crate::display::{self, CursorShape};
use crate::serial::{Serial, COM1};
use crate::sync::SpinLock;
use crate::task::{self, MAX_TASKS};
use crate::theme::{self, Item};
use crate::vga::{self, Color, Screen, DEFAULT_ATTR};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

/// Counts every time a screen is put on the display (`show`, `repaint`, mode and theme
/// changes). A task that draws pixels (`draw_pixels`) draws them again when this moves, because
/// showing a screen draws its cells over any pixels.
static SHOWS: AtomicU64 = AtomicU64::new(0);

struct Console {
    /// The boot task's screen. It has no cells of its own until `init_heap`.
    boot: Screen,
    /// The other tasks' screens, by task slot (index 0 is unused).
    screens: [Option<Box<Screen>>; MAX_TASKS],
    /// The task slot whose screen is shown.
    visible: usize,
    serial: Serial,
    /// The colors the serial terminal is set to, as a VGA color byte.
    serial_attr: u8,
    /// The desktop runs: the last row is the taskbar.
    desktop: bool,
    bar: Vec<u16>,
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

    fn current_or(&mut self, slot: usize) -> Option<&mut Screen> {
        let Console { boot, screens, .. } = self;
        screen_of(boot, screens, slot)
    }

    /// The taskbar's row (the display's last row).
    fn bar_row(&self) -> usize {
        display::size().1 - 1
    }

    /// Rows every screen has: all of the display's, or all but the taskbar.
    fn screen_rows(&self) -> usize {
        let rows = display::size().1;
        if self.desktop {
            rows - 1
        } else {
            rows
        }
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
        let width = display::size().0;
        let cells: Vec<u16> = if row == self.bar_row() && self.desktop {
            self.bar.clone()
        } else {
            let visible = self.visible;
            match self.current_or(visible) {
                Some(screen) => (0..screen.width()).map(|col| screen.cell(row, col)).collect(),
                None => vec![vga::cell(b' ', DEFAULT_ATTR); width],
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

    /// Puts screen `slot` on the display, and with `serial` repaints the serial terminal too.
    fn show_on(&mut self, slot: usize, serial: bool) {
        SHOWS.fetch_add(1, Ordering::AcqRel);
        let old = self.visible;
        self.visible = slot;
        let desktop = self.desktop;
        let bar_row = self.bar_row();
        if let Some(screen) = self.current_or(old) {
            screen.on_hardware = false;
        }
        let (rows, position, cursor) = match self.current_or(slot) {
            Some(screen) => {
                screen.on_hardware = true;
                screen.sync();
                (screen.rows(), screen.position(), screen.cursor_visible())
            }
            None => (display::size().1, (0, 0), false),
        };
        if desktop {
            display::put_row(bar_row, &self.bar);
        }
        if !serial {
            return;
        }
        let _ = self.serial.write_str("\x1b[0m\x1b[2J");
        self.serial_attr = DEFAULT_ATTR;
        if desktop {
            let _ = write!(self.serial, "\x1b[1;{bar_row}r");
        }
        for row in 0..rows {
            self.serial_row(row);
        }
        if desktop {
            self.serial_row(bar_row);
        }
        let _ = write!(self.serial, "\x1b[{};{}H{}", position.0 + 1, position.1 + 1, if cursor { "\x1b[?25h" } else { "\x1b[?25l" });
    }

    fn show(&mut self, slot: usize) {
        self.show_on(slot, true);
    }

    /// Fits every screen and the taskbar to the display's text grid.
    fn fit_screens(&mut self) {
        let (width, height) = display::size();
        let rows = self.screen_rows();
        self.boot.resize(width, height, rows);
        for screen in self.screens.iter_mut().flatten() {
            screen.resize(width, height, rows);
        }
        self.bar = vec![vga::cell(b' ', DEFAULT_ATTR); width];
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
    boot: Screen::unbacked(80, 25),
    screens: [const { None }; MAX_TASKS],
    visible: task::BOOT,
    serial: Serial::new(COM1),
    serial_attr: DEFAULT_ATTR,
    desktop: false,
    bar: Vec::new(),
});

pub fn init() {
    let mut console = CONSOLE.lock();
    console.boot.on_hardware = true;
    console.boot.clear();
    console.boot.set_cursor_visible(true);
    console.serial.init();
}

/// Once the heap exists: gives the boot screen cells of its own (with what it shows so far).
pub fn init_heap() {
    let mut console = CONSOLE.lock();
    console.boot.back();
    let width = display::size().0;
    console.bar = vec![vga::cell(b' ', DEFAULT_ATTR); width];
}

/// Clears the current task's screen (and the serial terminal, if it is shown).
pub fn clear() {
    let mut console = CONSOLE.lock();
    let desktop = console.desktop;
    let bar_row = console.bar_row();
    let (screen, visible, serial, serial_attr) = console.current();
    let Some(screen) = screen else { return };
    screen.clear();
    if visible {
        *serial_attr = DEFAULT_ATTR;
        if desktop {
            // Keep the taskbar: erase the rows above it one by one.
            let _ = serial.write_str("\x1b[0m");
            for row in 1..=bar_row {
                let _ = write!(serial, "\x1b[{row};1H\x1b[K");
            }
            let _ = serial.write_str("\x1b[H");
        } else {
            let _ = serial.write_str("\x1b[0m\x1b[2J\x1b[H");
        }
    }
}

/// How a full-screen row is drawn (the colors come from the theme's editor settings).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Normal,
    /// Dimmed, e.g. the `~` markers past the end of a file.
    Dim,
    /// A status bar.
    Inverse,
}

/// Draws one full row of the current task's screen without moving the scrolling output. On
/// serial this uses ANSI cursor positioning, so the terminal shows the same screen.
pub fn draw_row(row: usize, text: &[u8], style: Style) {
    let attr = theme::attr(match style {
        Style::Normal => Item::Editor,
        Style::Dim => Item::EditorDim,
        Style::Inverse => Item::EditorStatus,
    });
    let mut console = CONSOLE.lock();
    let (screen, visible, serial, serial_attr) = console.current();
    let Some(screen) = screen else { return };
    if row >= screen.rows() {
        return;
    }
    screen.write_row(row, text, attr);
    if !visible {
        return;
    }
    let width = screen.width();
    let _ = write!(serial, "\x1b[{};1H", row + 1);
    write_sgr(serial, attr);
    for &b in text.iter().take(width) {
        serial.write_byte(if (0x20..0x7F).contains(&b) { b } else { b'?' });
    }
    if attr != DEFAULT_ATTR {
        for _ in text.len()..width {
            serial.write_byte(b' ');
        }
    }
    let _ = serial.write_str("\x1b[K\x1b[0m");
    *serial_attr = DEFAULT_ATTR;
}

/// Draws one full row of the current task's screen from colored pieces of text, padded with
/// blanks in `pad_attr`. Code page 437 symbols show on the display; serial gets ASCII stand-ins.
pub fn draw_spans(row: usize, spans: &[(&[u8], u8)], pad_attr: u8) {
    let mut console = CONSOLE.lock();
    let (screen, visible, serial, serial_attr) = console.current();
    let Some(screen) = screen else { return };
    if row >= screen.rows() {
        return;
    }
    screen.write_spans(row, spans, pad_attr);
    if visible {
        let cells: Vec<u16> = (0..screen.width()).map(|col| screen.cell(row, col)).collect();
        let _ = write!(serial, "\x1b[{};1H", row + 1);
        Console::serial_cells(serial, serial_attr, cells.iter().copied());
        write_sgr(serial, DEFAULT_ATTR);
        *serial_attr = DEFAULT_ATTR;
    }
}

/// Draws one full row of the current task's screen from ready-made cells (`char | attr << 8`),
/// padded with blanks in `pad_attr`. Serial gets ASCII stand-ins for the characters.
pub fn draw_cells(row: usize, cells: &[u16], pad_attr: u8) {
    let mut console = CONSOLE.lock();
    let (screen, visible, serial, serial_attr) = console.current();
    let Some(screen) = screen else { return };
    if row >= screen.rows() {
        return;
    }
    screen.write_cells(row, cells, pad_attr);
    if visible {
        let cells: Vec<u16> = (0..screen.width()).map(|col| screen.cell(row, col)).collect();
        let _ = write!(serial, "\x1b[{};1H", row + 1);
        Console::serial_cells(serial, serial_attr, cells.iter().copied());
        write_sgr(serial, DEFAULT_ATTR);
        *serial_attr = DEFAULT_ATTR;
    }
}

/// Draws ready-made cells from column `col` of `row` on, leaving the rest of the row alone: a
/// full-screen program that changed part of a row writes only that part (on the display, and on
/// serial), and in a graphics mode the pixels drawn over the rest of the row stay.
pub fn draw_cells_at(row: usize, col: usize, cells: &[u16]) {
    let mut console = CONSOLE.lock();
    let (screen, visible, serial, serial_attr) = console.current();
    let Some(screen) = screen else { return };
    if row >= screen.rows() || col >= screen.width() {
        return;
    }
    screen.write_cells_at(row, col, cells);
    if visible {
        let end = (col + cells.len()).min(screen.width());
        let _ = write!(serial, "[{};{}H", row + 1, col + 1);
        Console::serial_cells(serial, serial_attr, (col..end).map(|c| screen.cell(row, c)));
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

/// Rows of the current task's screen: the display's rows, one less under the desktop.
pub fn rows() -> usize {
    let mut console = CONSOLE.lock();
    let rows = console.screen_rows();
    console.current().0.map_or(rows, |s| s.rows())
}

/// Columns of the current task's screen.
pub fn cols() -> usize {
    let mut console = CONSOLE.lock();
    console.current().0.map_or_else(|| display::size().0, |s| s.width())
}

/// See `SHOWS`: pixel-drawing tasks wait for this to change (with `display::generation`) and
/// then draw their pixels again.
pub fn shows() -> u64 {
    SHOWS.load(Ordering::Acquire)
}

/// Graphics modes: draws a row of 0x00RRGGBB pixels at (x, y) in area pixels (see
/// `display::draw_pixels`), if the current task's screen is the one shown. Returns false (and
/// draws nothing) otherwise, or in text mode; the task draws again when `shows()` moves.
pub fn draw_pixels(x: usize, y: usize, rgb: &[u32]) -> bool {
    let console = CONSOLE.lock();
    if console.visible != task::current() || !display::is_graphics() {
        return false;
    }
    display::draw_pixels(x, y, rgb);
    drop(console);
    true
}

/// Like `draw_pixels`, for a rectangle of one color.
pub fn fill_pixels(x: usize, y: usize, w: usize, h: usize, rgb: u32) -> bool {
    for line in y..y.saturating_add(h) {
        let console = CONSOLE.lock();
        if console.visible != task::current() || !display::is_graphics() {
            return false;
        }
        display::fill_pixels(x, line, w, 1, rgb);
        drop(console);
    }
    true
}

/// The taskbar's row under the desktop (the display's last row).
pub fn bar_row() -> usize {
    CONSOLE.lock().bar_row()
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
    unsafe {
        CONSOLE.force_unlock();
        display::force_unlock();
    }
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

/// Gives task `slot` a new, empty screen in the theme's shell colors.
pub fn create_screen(slot: usize) {
    if slot == task::BOOT || slot >= MAX_TASKS {
        return;
    }
    let (width, height) = display::size();
    let mut screen = Box::new(Screen::new(width, height, theme::attr(Item::Shell)));
    let mut console = CONSOLE.lock();
    // The display may have changed while the screen was made.
    let (width, height) = display::size();
    let rows = console.screen_rows();
    screen.resize(width, height, rows);
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

/// Shows task `slot`'s screen (on the display, and repainted on serial).
pub fn show(slot: usize) {
    CONSOLE.lock().show(slot);
}

/// Redraws the shown screen, e.g. after an overlay.
pub fn repaint() {
    let mut console = CONSOLE.lock();
    let visible = console.visible;
    console.show(visible);
}

/// Turns the desktop layout on or off: screens one row shorter with the taskbar below, or the
/// full height. Repaints the shown screen.
pub fn set_desktop(on: bool) {
    let mut console = CONSOLE.lock();
    console.desktop = on;
    let rows = console.screen_rows();
    console.boot.set_rows(rows);
    for screen in console.screens.iter_mut().flatten() {
        screen.set_rows(rows);
    }
    let width = display::size().0;
    console.bar = vec![vga::cell(b' ', DEFAULT_ATTR); width];
    if !on {
        // Back to a full-height terminal.
        let _ = console.serial.write_str("\x1b[r");
    }
    let visible = console.visible;
    console.show(visible);
}

/// Switches the display mode (`display.rs`) and fits every screen to the new text grid. What the
/// screens show is kept. Fails without changing anything if the screens would not fit in memory.
pub fn set_display(request: display::Request) -> Result<display::Mode, alloc::string::String> {
    if let display::Request::Graphics { width, height, scale } = request {
        let (cols, rows) = display::grid(width, height, scale)?;
        // Every screen, a copy of each while it is resized, and the display's own copy.
        let screens = 2 * (MAX_TASKS + 1) + 1;
        let needed = cols * rows * 2 * screens + 1024 * 1024;
        let free = crate::heap::stats().free();
        if needed > free {
            return Err(alloc::format!(
                "a {cols}x{rows} text grid needs about {} KiB of memory, and {} KiB are free: use a larger scale",
                needed / 1024,
                free / 1024
            ));
        }
    }
    let mut console = CONSOLE.lock();
    let mode = display::set(request)?;
    console.fit_screens();
    let visible = console.visible;
    // The serial terminal shows the same text as before, unless the taskbar moved.
    let desktop = console.desktop;
    console.show_on(visible, desktop);
    Ok(mode)
}

/// A new theme: screens showing the old shell colors take the new ones, and the palette and the
/// cursor shape change. Redraws the shown screen.
pub fn retheme(old_shell: u8, new_shell: u8, palette: [[u8; 3]; 16], cursor: CursorShape) {
    let mut console = CONSOLE.lock();
    if old_shell != new_shell {
        console.boot.recolor(old_shell, new_shell);
        for screen in console.screens.iter_mut().flatten() {
            screen.recolor(old_shell, new_shell);
        }
    }
    display::set_cursor_shape(cursor);
    display::set_palette(palette);
    let visible = console.visible;
    let desktop = console.desktop;
    console.show_on(visible, desktop && old_shell != new_shell);
}

/// Draws the taskbar (the last row) on the display and serial. Does nothing unless the desktop
/// runs.
pub fn draw_bar(spans: &[(&[u8], u8)], pad_attr: u8) {
    let mut console = CONSOLE.lock();
    if !console.desktop {
        return;
    }
    let width = display::size().0;
    let mut bar = vec![vga::cell(b' ', pad_attr); width];
    let mut col = 0;
    for &(text, attr) in spans {
        for &b in text {
            if col < width {
                bar[col] = vga::cell(b, attr);
                col += 1;
            }
        }
    }
    if bar == console.bar {
        return;
    }
    let bar_row = console.bar_row();
    display::put_row(bar_row, &bar);
    console.bar = bar;
    // Save the terminal's cursor, draw, and put it back.
    let _ = console.serial.write_str("\x1b7");
    let saved_attr = console.serial_attr;
    console.serial_row(bar_row);
    let _ = console.serial.write_str("\x1b8");
    // `ESC 8` restores the colors too.
    console.serial_attr = saved_attr;
}

/// Draws colored text at `row`, `col` over the shown screen, on the display and serial only:
/// the screens are not changed, and `repaint` removes it.
pub fn overlay(row: usize, col: usize, spans: &[(&[u8], u8)]) {
    let mut console = CONSOLE.lock();
    let mut cells = Vec::new();
    for &(text, attr) in spans {
        cells.extend(text.iter().map(|&b| vga::cell(b, attr)));
    }
    cells.truncate(display::size().0.saturating_sub(col));
    for (i, &cell) in cells.iter().enumerate() {
        display::put(row, col + i, cell);
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

/// Puts back a screen saved with `snapshot`, fitted to the current display and number of rows.
pub fn restore(slot: usize, saved: &Screen) {
    let mut console = CONSOLE.lock();
    let visible = console.visible == slot;
    let (width, height) = display::size();
    let Console { boot, screens, .. } = &mut *console;
    if let Some(screen) = screen_of(boot, screens, slot) {
        let rows = screen.rows();
        *screen = saved.clone();
        screen.on_hardware = false;
        screen.resize(width, height, rows);
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
