//! VGA text mode: 80x25 cells at 0xB8000, each a character byte and a color byte.
//!
//! Every task draws into its own `Screen`, a copy of the 80x25 cells in memory with its own
//! cursor and colors. Only the screen marked `on_hardware` (the one being shown) also writes
//! through to the real VGA memory and moves the hardware cursor.

use crate::port::{inb, outb};
use core::fmt;
use core::ptr::write_volatile;

const BUFFER: *mut u16 = 0xB8000 as *mut u16;
pub const WIDTH: usize = 80;
pub const HEIGHT: usize = 25;
const BACKSPACE: u8 = 0x08;

const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;
const CRTC_CURSOR_START: u8 = 0x0A;
const CRTC_CURSOR_END: u8 = 0x0B;
const CRTC_CURSOR_HIGH: u8 = 0x0E;
const CRTC_CURSOR_LOW: u8 = 0x0F;
/// Bit 5 of the cursor start register turns the cursor off.
const CURSOR_DISABLE: u8 = 1 << 5;

/// Light gray on black.
pub const DEFAULT_ATTR: u8 = (Color::Black as u8) << 4 | Color::LightGray as u8;

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    Pink = 13,
    Yellow = 14,
    White = 15,
}

impl Color {
    pub const ALL: [(&'static str, Color); 16] = [
        ("black", Color::Black),
        ("blue", Color::Blue),
        ("green", Color::Green),
        ("cyan", Color::Cyan),
        ("red", Color::Red),
        ("magenta", Color::Magenta),
        ("brown", Color::Brown),
        ("lightgray", Color::LightGray),
        ("darkgray", Color::DarkGray),
        ("lightblue", Color::LightBlue),
        ("lightgreen", Color::LightGreen),
        ("lightcyan", Color::LightCyan),
        ("lightred", Color::LightRed),
        ("pink", Color::Pink),
        ("yellow", Color::Yellow),
        ("white", Color::White),
    ];

    pub fn from_name(name: &str) -> Option<Color> {
        Self::ALL.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|&(_, c)| c)
    }
}

/// The color byte for `fg` on `bg`.
pub const fn attr(fg: Color, bg: Color) -> u8 {
    (bg as u8) << 4 | fg as u8
}

pub const fn cell(byte: u8, attr: u8) -> u16 {
    (attr as u16) << 8 | byte as u16
}

/// Writes one cell of the real screen.
pub fn hw_put(row: usize, col: usize, cell: u16) {
    if row < HEIGHT && col < WIDTH {
        unsafe { write_volatile(BUFFER.add(row * WIDTH + col), cell) };
    }
}

/// Shows the hardware cursor as an underline (scanlines 14-15) at `row`, `col`, or hides it.
pub fn hw_cursor(row: usize, col: usize, visible: bool) {
    let pos = (row.min(HEIGHT - 1) * WIDTH + col.min(WIDTH - 1)) as u16;
    unsafe {
        outb(CRTC_INDEX, CRTC_CURSOR_START);
        let start = inb(CRTC_DATA) & 0xC0;
        outb(CRTC_DATA, if visible { start | 14 } else { start | CURSOR_DISABLE });
        outb(CRTC_INDEX, CRTC_CURSOR_END);
        outb(CRTC_DATA, (inb(CRTC_DATA) & 0xE0) | 15);
        outb(CRTC_INDEX, CRTC_CURSOR_LOW);
        outb(CRTC_DATA, pos as u8);
        outb(CRTC_INDEX, CRTC_CURSOR_HIGH);
        outb(CRTC_DATA, (pos >> 8) as u8);
    }
}

/// A text screen in memory: cells, the output position, colors and the cursor.
#[derive(Clone)]
pub struct Screen {
    cells: [u16; WIDTH * HEIGHT],
    row: usize,
    /// May be `WIDTH`: the row wraps when the next character comes.
    col: usize,
    attr: u8,
    /// Rows used by this screen: 25, or 24 while the desktop's taskbar takes the last one.
    rows: usize,
    cursor_visible: bool,
    /// This screen is the one shown: every change is also made on the real screen.
    pub on_hardware: bool,
}

impl Screen {
    pub const fn new() -> Self {
        Self {
            cells: [cell(b' ', DEFAULT_ATTR); WIDTH * HEIGHT],
            row: 0,
            col: 0,
            attr: DEFAULT_ATTR,
            rows: HEIGHT,
            cursor_visible: true,
            on_hardware: false,
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Changes the number of rows (the cells below are kept but not shown).
    pub fn set_rows(&mut self, rows: usize) {
        self.rows = rows.clamp(1, HEIGHT);
        self.row = self.row.min(self.rows - 1);
    }

    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.attr = attr(fg, bg);
    }

    pub fn attr(&self) -> u8 {
        self.attr
    }

    pub fn set_attr(&mut self, attr: u8) {
        self.attr = attr;
    }

    pub fn cell(&self, row: usize, col: usize) -> u16 {
        self.cells[row * WIDTH + col]
    }

    pub fn position(&self) -> (usize, usize) {
        (self.row, self.col.min(WIDTH - 1))
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
        self.update_cursor();
    }

    /// The characters of one row, without colors (for the self-tests).
    pub fn row_bytes(&self, row: usize) -> [u8; WIDTH] {
        core::array::from_fn(|col| self.cell(row, col) as u8)
    }

    fn put(&mut self, row: usize, col: usize, cell: u16) {
        self.cells[row * WIDTH + col] = cell;
        if self.on_hardware {
            hw_put(row, col, cell);
        }
    }

    pub fn clear(&mut self) {
        for row in 0..self.rows {
            self.clear_row(row);
        }
        self.row = 0;
        self.col = 0;
        self.update_cursor();
    }

    /// Copies the whole screen and the cursor to the real screen.
    pub fn sync(&self) {
        for row in 0..self.rows {
            for col in 0..WIDTH {
                hw_put(row, col, self.cell(row, col));
            }
        }
        self.update_cursor();
    }

    /// Fills one row with `text` (padded with blanks) in colors `attr`, without moving the
    /// output position. Bytes outside printable ASCII are shown as a block.
    pub fn write_row(&mut self, row: usize, text: &[u8], attr: u8) {
        if row >= self.rows {
            return;
        }
        for col in 0..WIDTH {
            let byte = match text.get(col) {
                Some(&b) if (0x20..0x7F).contains(&b) => b,
                Some(_) => 0xFE,
                None => b' ',
            };
            self.put(row, col, cell(byte, attr));
        }
    }

    /// Fills one row with colored pieces of text, padded with blanks in `pad_attr`. The bytes
    /// are shown as they are, so code page 437 symbols (box lines, blocks) can be used.
    pub fn write_spans(&mut self, row: usize, spans: &[(&[u8], u8)], pad_attr: u8) {
        if row >= self.rows {
            return;
        }
        let mut col = 0;
        for &(text, attr) in spans {
            for &b in text {
                if col < WIDTH {
                    self.put(row, col, cell(b, attr));
                    col += 1;
                }
            }
        }
        while col < WIDTH {
            self.put(row, col, cell(b' ', pad_attr));
            col += 1;
        }
    }

    /// Moves the output position and the cursor.
    pub fn set_position(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows - 1);
        self.col = col.min(WIDTH - 1);
        self.update_cursor();
    }

    pub fn update_cursor(&self) {
        if self.on_hardware {
            hw_cursor(self.row, self.col, self.cursor_visible);
        }
    }

    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            b'\r' => self.col = 0,
            BACKSPACE => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = WIDTH - 1;
                }
            }
            byte => {
                if self.col >= WIDTH {
                    self.new_line();
                }
                self.put(self.row, self.col, cell(byte, self.attr));
                self.col += 1;
            }
        }
    }

    fn new_line(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
            return;
        }
        let end = self.rows * WIDTH;
        self.cells.copy_within(WIDTH..end, 0);
        self.clear_row(self.rows - 1);
        if self.on_hardware {
            self.sync();
        }
    }

    fn clear_row(&mut self, row: usize) {
        for col in 0..WIDTH {
            self.put(row, col, cell(b' ', self.attr));
        }
    }
}

impl fmt::Write for Screen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            match c {
                ' '..='~' | '\n' | '\r' | '\x08' => self.write_byte(c as u8),
                _ => self.write_byte(0xFE),
            }
        }
        self.update_cursor();
        Ok(())
    }
}
