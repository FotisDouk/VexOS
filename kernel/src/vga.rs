//! Screens: grids of text cells, each a character byte and a color byte (the VGA text format).
//!
//! Every task draws into its own `Screen`, a copy of its cells in memory with its own cursor and
//! colors. Only the screen marked `on_hardware` (the one being shown) also writes through to the
//! display (`display.rs`): VGA text memory, or a graphics mode where the cells are drawn as pixels.
//! A screen is as large as the display's text grid (80x25 in VGA text mode, more in graphics
//! modes); `resize` follows a change of display mode.

use crate::display;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

const BACKSPACE: u8 = 0x08;

/// Light gray on black: the serial terminal's own colors.
pub const DEFAULT_ATTR: u8 = (Color::Black as u8) << 4 | Color::LightGray as u8;

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

    pub fn from_index(index: u8) -> Color {
        Self::ALL[(index & 0x0F) as usize].1
    }

    pub fn name(self) -> &'static str {
        Self::ALL[self as usize].0
    }
}

/// The color byte for `fg` on `bg`.
pub const fn attr(fg: Color, bg: Color) -> u8 {
    (bg as u8) << 4 | fg as u8
}

pub const fn cell(byte: u8, attr: u8) -> u16 {
    (attr as u16) << 8 | byte as u16
}

/// A text screen in memory: cells, the output position, colors and the cursor.
#[derive(Clone)]
pub struct Screen {
    /// `width * height` cells. Empty for the boot screen until the heap exists: then the VGA
    /// text memory is the only copy.
    cells: Vec<u16>,
    width: usize,
    height: usize,
    row: usize,
    /// May be `width`: the row wraps when the next character comes.
    col: usize,
    attr: u8,
    /// Rows used by this screen: `height`, or one less while the desktop's taskbar takes the last.
    rows: usize,
    cursor_visible: bool,
    /// This screen is the one shown: every change is also made on the display.
    pub on_hardware: bool,
}

impl Screen {
    /// A screen without cells of its own, for the boot screen before the heap is set up: it
    /// writes straight to VGA text memory. `back` gives it cells later.
    pub const fn unbacked(width: usize, height: usize) -> Self {
        Self {
            cells: Vec::new(),
            width,
            height,
            row: 0,
            col: 0,
            attr: DEFAULT_ATTR,
            rows: height,
            cursor_visible: true,
            on_hardware: false,
        }
    }

    /// A blank screen of `width` x `height` cells in colors `attr`.
    pub fn new(width: usize, height: usize, attr: u8) -> Self {
        Self {
            cells: vec![cell(b' ', attr); width * height],
            width,
            height,
            row: 0,
            col: 0,
            attr,
            rows: height,
            cursor_visible: true,
            on_hardware: false,
        }
    }

    /// Gives an unbacked screen cells, filled with what the text display shows now.
    pub fn back(&mut self) {
        if self.cells.is_empty() {
            let width = self.width;
            self.cells = (0..width * self.height).map(|i| display::text_cell(i / width, i % width)).collect();
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Changes the number of rows (the cells below are kept but not shown).
    pub fn set_rows(&mut self, rows: usize) {
        self.rows = rows.clamp(1, self.height);
        self.row = self.row.min(self.rows - 1);
    }

    /// Changes the size to `width` x `height` cells, of which `rows` are used. What the screen
    /// shows stays at the top left; if there are fewer rows, the top ones go, so the row with the
    /// output position stays on the screen. New cells are blank in the current colors.
    pub fn resize(&mut self, width: usize, height: usize, rows: usize) {
        let rows = rows.clamp(1, height);
        if width == self.width && height == self.height {
            self.set_rows(rows);
            return;
        }
        if !self.cells.is_empty() {
            let mut cells = vec![cell(b' ', self.attr); width * height];
            let skip = (self.row + 1).saturating_sub(rows);
            for row in 0..self.rows.saturating_sub(skip).min(rows) {
                let from = (row + skip) * self.width;
                let len = width.min(self.width);
                cells[row * width..row * width + len].copy_from_slice(&self.cells[from..from + len]);
            }
            self.cells = cells;
            self.row -= skip;
        }
        self.width = width;
        self.height = height;
        self.rows = rows;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(width);
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
        match self.cells.get(row * self.width + col) {
            Some(&c) => c,
            None if self.cells.is_empty() => display::text_cell(row, col),
            None => cell(b' ', self.attr),
        }
    }

    /// Changes the colors `from` to `to` in every cell, and in the colors used from now on.
    pub fn recolor(&mut self, from: u8, to: u8) {
        for c in self.cells.iter_mut() {
            if (*c >> 8) as u8 == from {
                *c = cell(*c as u8, to);
            }
        }
        if self.attr == from {
            self.attr = to;
        }
    }

    pub fn position(&self) -> (usize, usize) {
        (self.row, self.col.min(self.width - 1))
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
        self.update_cursor();
    }

    /// The characters of one row, without colors (for the self-tests).
    pub fn row_bytes(&self, row: usize) -> Vec<u8> {
        (0..self.width).map(|col| self.cell(row, col) as u8).collect()
    }

    fn put(&mut self, row: usize, col: usize, cell: u16) {
        if let Some(c) = self.cells.get_mut(row * self.width + col) {
            *c = cell;
        }
        if self.on_hardware {
            display::put(row, col, cell);
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

    /// Copies the whole screen and the cursor to the display.
    pub fn sync(&self) {
        if !self.cells.is_empty() {
            for row in 0..self.rows {
                display::put_row(row, &self.cells[row * self.width..(row + 1) * self.width]);
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
        for col in 0..self.width {
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
                if col < self.width {
                    self.put(row, col, cell(b, attr));
                    col += 1;
                }
            }
        }
        while col < self.width {
            self.put(row, col, cell(b' ', pad_attr));
            col += 1;
        }
    }

    /// Writes ready-made cells from column `col` of `row` on, cut at the right edge; the rest of
    /// the row stays as it is.
    pub fn write_cells_at(&mut self, row: usize, col: usize, cells: &[u16]) {
        if row >= self.rows {
            return;
        }
        for (i, &c) in cells.iter().enumerate().take(self.width.saturating_sub(col)) {
            self.put(row, col + i, c);
        }
    }

    /// Fills one row with ready-made cells (`char | attr << 8`), padded with blanks in
    /// `pad_attr`. For pictures drawn as colored half blocks.
    pub fn write_cells(&mut self, row: usize, cells: &[u16], pad_attr: u8) {
        if row >= self.rows {
            return;
        }
        for col in 0..self.width {
            self.put(row, col, cells.get(col).copied().unwrap_or(cell(b' ', pad_attr)));
        }
    }

    /// Moves the output position and the cursor.
    pub fn set_position(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows - 1);
        self.col = col.min(self.width - 1);
        self.update_cursor();
    }

    pub fn update_cursor(&self) {
        if self.on_hardware {
            let (row, col) = self.position();
            display::cursor(row, col, self.cursor_visible);
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
                    self.col = self.width - 1;
                }
            }
            byte => {
                if self.col >= self.width {
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
        if self.cells.is_empty() {
            if self.on_hardware {
                display::scroll_text(self.rows, cell(b' ', self.attr));
            }
            return;
        }
        let end = self.rows * self.width;
        self.cells.copy_within(self.width..end, 0);
        let last = self.rows - 1;
        for col in 0..self.width {
            self.cells[last * self.width + col] = cell(b' ', self.attr);
        }
        if self.on_hardware {
            self.sync();
        }
    }

    fn clear_row(&mut self, row: usize) {
        for col in 0..self.width {
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
