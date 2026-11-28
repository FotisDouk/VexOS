//! Full-screen hex editor: `edit -x <file>`, and what `edit` opens for binary types (.vex, .sys,
//! .boot, .raw). Every byte of any file can be changed, including the bootloader and the kernel.
//!
//! All rows but the last show 16 bytes each (8 on a narrow screen, 32 or 64 on a wide one), as
//! hex and as characters; the last row is the status bar.
//! Keys: arrows, PgUp/PgDn, Home/End (row), Ctrl-T/Ctrl-E (start/end of file), Ctrl-G (go to a
//! hex offset), Tab (hex or character column), Insert (insert or overwrite), Delete (byte at the
//! cursor), Backspace (byte before it). Ctrl-S or Ctrl-O saves; Ctrl-Q or Ctrl-X quits (twice if
//! there are unsaved changes).

use crate::console::{self, Style};
use crate::input::{self, Key};
use crate::theme::{self, Item};
use crate::{display, fs};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use kfs::FileType;

/// Screen column of the first hex digit.
const HEX_COL: usize = 10;

/// Screen columns a row of `per_row` bytes takes: offset, hex digits with a gap after every 8,
/// then the characters between bars.
fn row_width(per_row: usize) -> usize {
    HEX_COL + per_row * 3 + per_row / 8 + 1 + per_row
}

/// The most bytes per row (8, 16, 32 or 64) that fit in `width` columns.
pub fn bytes_per_row(width: usize) -> usize {
    [64, 32, 16].into_iter().find(|&n| row_width(n) <= width).unwrap_or(8)
}

/// Screen column of byte `j` of a row in the hex column, and of the first character.
fn hex_col(j: usize) -> usize {
    HEX_COL + j * 3 + j / 8
}

fn char_col(per_row: usize) -> usize {
    HEX_COL + per_row * 3 + per_row / 8
}

pub struct HexEditor {
    path: String,
    data: Vec<u8>,
    /// Byte offset of the cursor, `0..=data.len()`; `data.len()` is where new bytes are added.
    cursor: usize,
    /// First visible row (a row is 16 bytes).
    top: usize,
    /// The high digit of the byte at the cursor has been typed; the next digit is the low one.
    high_done: bool,
    /// Typing goes to the character column instead of the hex column.
    chars: bool,
    /// Typing inserts new bytes instead of overwriting.
    insert: bool,
    modified: bool,
    quit_armed: bool,
    message: String,
    /// The Ctrl-G prompt, while it is open.
    goto: Option<String>,
    /// The screen's size: rows of bytes (the status bar is below them), columns, bytes per row.
    text_rows: usize,
    width: usize,
    per_row: usize,
    /// `display::generation` when the screen was last drawn in full.
    generation: u64,
    /// What each screen row currently shows, so only changed rows are redrawn.
    shown: Vec<Option<(Vec<u8>, Style)>>,
}

impl HexEditor {
    /// Opens `path`, or starts an empty file if it does not exist yet.
    pub fn open(path: &str) -> fs::Result<HexEditor> {
        let (data, mut message) = match fs::with(|fs| fs.read(path)) {
            Ok(data) => {
                let message = alloc::format!("{} bytes", data.len());
                (data, message)
            }
            Err(fs::FsError::Fs(kfs::Error::NotFound)) => (Vec::new(), String::from("new file")),
            Err(e) => return Err(e),
        };
        // Kept short: the status bar also shows the path and the cursor position.
        if path == kfs::BOOT_PATH {
            message = alloc::format!("Bootloader: {} bytes max", kfs::BOOT_SECTORS * 512);
        } else if FileType::from_name(path).is_system() {
            message = String::from("Used at the next boot");
        }
        Ok(HexEditor {
            path: String::from(path),
            data,
            cursor: 0,
            top: 0,
            high_done: false,
            chars: false,
            insert: false,
            modified: false,
            quit_armed: false,
            message,
            goto: None,
            text_rows: console::rows().saturating_sub(1).max(1),
            width: console::cols(),
            per_row: bytes_per_row(console::cols()),
            generation: u64::MAX,
            shown: Vec::new(),
        })
    }

    /// Puts the cursor on byte `offset` (past the end = the end).
    pub fn go_to(mut self, offset: usize) -> HexEditor {
        self.cursor = offset.min(self.data.len());
        self
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Runs until the user quits. Clears the screen afterwards.
    pub fn run(mut self) {
        let saved = console::attr();
        console::set_attr(theme::attr(Item::Editor));
        console::clear();
        loop {
            self.render();
            // A new display mode or theme: draw everything again at once.
            let Some(key) = input::read_key_until(|| display::generation() != self.generation) else { continue };
            if !self.handle(key) {
                break;
            }
        }
        console::set_attr(saved);
        console::clear();
    }

    /// Applies one key. Returns false to quit.
    pub fn handle(&mut self, key: Key) -> bool {
        // The task is being closed: quit, without saving.
        if key == Key::Close {
            return false;
        }
        if self.goto.is_some() {
            self.handle_goto(key);
            return true;
        }
        let quit_armed = core::mem::take(&mut self.quit_armed);
        let per_row = self.per_row;
        let row_start = self.cursor - self.cursor % per_row;
        let page = per_row * self.text_rows.saturating_sub(1).max(1);
        match key {
            Key::Ctrl(b's' | b'o') => self.save(),
            Key::Ctrl(b'q' | b'x') => {
                if !self.modified || quit_armed {
                    return false;
                }
                self.quit_armed = true;
                self.message = String::from("Unsaved changes! Press Ctrl-Q again to quit without saving.");
            }
            Key::Ctrl(b'g') => self.goto = Some(String::new()),
            Key::Ctrl(b't') => self.move_to(0),
            Key::Ctrl(b'e') => self.move_to(self.data.len()),
            Key::Char(b'\t') => {
                self.chars = !self.chars;
                self.high_done = false;
            }
            Key::Insert => {
                self.insert = !self.insert;
                self.high_done = false;
            }
            Key::Left => self.move_to(self.cursor.saturating_sub(1)),
            Key::Right => self.move_to(self.cursor + 1),
            Key::Up => self.move_to(self.cursor.checked_sub(per_row).unwrap_or(self.cursor)),
            Key::Down if self.cursor + per_row <= self.data.len() => self.move_to(self.cursor + per_row),
            Key::Down => {}
            Key::PageUp => self.move_to(self.cursor.saturating_sub(page)),
            Key::PageDown => self.move_to(self.cursor + page),
            Key::Home => self.move_to(row_start),
            Key::End => self.move_to(row_start + per_row - 1),
            Key::Delete if self.cursor < self.data.len() => {
                self.data.remove(self.cursor);
                self.high_done = false;
                self.modified = true;
            }
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.data.remove(self.cursor);
                self.high_done = false;
                self.modified = true;
            }
            Key::Char(c) if self.chars => self.put_byte(c),
            Key::Char(c) => match (c as char).to_digit(16) {
                Some(digit) => self.put_digit(digit as u8),
                None => self.message = String::from("Type hex digits 0-9 a-f (Tab: type characters)"),
            },
            _ => {}
        }
        true
    }

    fn handle_goto(&mut self, key: Key) {
        let Some(text) = self.goto.as_mut() else { return };
        match key {
            Key::Char(c) if c.is_ascii_hexdigit() || c == b'x' => text.push(c as char),
            Key::Backspace => {
                text.pop();
            }
            Key::Enter => {
                let digits = text.trim_start_matches("0x");
                match usize::from_str_radix(digits, 16) {
                    Ok(offset) => {
                        self.cursor = offset.min(self.data.len());
                        self.high_done = false;
                    }
                    Err(_) => self.message = String::from("Not a hex offset"),
                }
                self.goto = None;
            }
            Key::Escape | Key::Ctrl(_) => self.goto = None,
            _ => {}
        }
    }

    fn move_to(&mut self, offset: usize) {
        self.cursor = offset.min(self.data.len());
        self.high_done = false;
    }

    /// Writes one whole byte at the cursor and moves past it.
    fn put_byte(&mut self, byte: u8) {
        if self.insert || self.cursor == self.data.len() {
            self.data.insert(self.cursor, byte);
        } else {
            self.data[self.cursor] = byte;
        }
        self.cursor += 1;
        self.high_done = false;
        self.modified = true;
    }

    /// Writes one hex digit: the high half of the byte first, then the low half.
    fn put_digit(&mut self, digit: u8) {
        if !self.high_done {
            if self.insert || self.cursor == self.data.len() {
                self.data.insert(self.cursor, digit << 4);
            } else {
                self.data[self.cursor] = (self.data[self.cursor] & 0x0F) | digit << 4;
            }
            self.high_done = true;
        } else {
            self.data[self.cursor] = (self.data[self.cursor] & 0xF0) | digit;
            self.cursor += 1;
            self.high_done = false;
        }
        self.modified = true;
    }

    fn save(&mut self) {
        self.message = match fs::with(|fs| fs.write(&self.path, &self.data)) {
            Ok(()) => {
                self.modified = false;
                alloc::format!("Saved {} bytes", self.data.len())
            }
            Err(e) => alloc::format!("Save failed: {e}"),
        };
    }

    fn draw(&mut self, row: usize, text: Vec<u8>, style: Style) {
        let wanted = Some((text, style));
        if self.shown[row] != wanted {
            let (text, style) = wanted.as_ref().unwrap();
            console::draw_row(row, text, *style);
            self.shown[row] = wanted;
        }
    }

    /// One screen row: offset, `per_row` bytes in hex, and the same bytes as characters.
    fn row_text(&self, start: usize) -> Vec<u8> {
        let mut line = alloc::format!("{start:08x}  ");
        for j in 0..self.per_row {
            if j > 0 && j % 8 == 0 {
                line.push(' ');
            }
            match self.data.get(start + j) {
                Some(b) => {
                    let _ = write!(line, "{b:02x} ");
                }
                None => line.push_str("   "),
            }
        }
        line.push('|');
        for j in 0..self.per_row {
            line.push(match self.data.get(start + j) {
                Some(&b) if (0x20..0x7F).contains(&b) => b as char,
                Some(_) => '.',
                None => ' ',
            });
        }
        line.push('|');
        line.into_bytes()
    }

    /// After a change of display mode or theme: takes the screen's new size, and forgets what
    /// the rows show so all of them are drawn.
    fn refit(&mut self) {
        let generation = display::generation();
        if generation == self.generation {
            return;
        }
        self.generation = generation;
        self.text_rows = console::rows().saturating_sub(1).max(1);
        self.width = console::cols();
        self.per_row = bytes_per_row(self.width);
        self.top = 0;
        self.shown = vec![None; self.text_rows + 1];
        console::set_attr(theme::attr(Item::Editor));
    }

    fn render(&mut self) {
        self.refit();
        let (per_row, text_rows, width) = (self.per_row, self.text_rows, self.width);
        let row = self.cursor / per_row;
        if row < self.top {
            self.top = row;
        } else if row >= self.top + text_rows {
            self.top = row + 1 - text_rows;
        }
        for screen_row in 0..text_rows {
            let start = (self.top + screen_row) * per_row;
            // The row that holds the end of the file is shown even when it is empty, so the
            // cursor can go there to add bytes.
            if start < self.data.len() || start == self.data.len() - self.data.len() % per_row {
                let text = self.row_text(start);
                self.draw(screen_row, text, Style::Normal);
            } else {
                self.draw(screen_row, b"~".to_vec(), Style::Dim);
            }
        }

        let (status, cursor_col) = match &self.goto {
            Some(text) => {
                let prompt = alloc::format!(" Go to offset (hex, Enter/Esc): {text}");
                let col = prompt.len();
                (prompt, Some(col))
            }
            None => {
                let name = alloc::format!(" {}{}", self.path, if self.modified { " [modified]" } else { "" });
                let hint = if self.message.is_empty() {
                    "^S save ^Q quit ^G go to Tab hex/chr Ins mode"
                } else {
                    self.message.as_str()
                };
                let right = alloc::format!(
                    "{} {} {:#x}/{:#x} ",
                    if self.insert { "INS" } else { "OVR" },
                    if self.chars { "CHR" } else { "HEX" },
                    self.cursor,
                    self.data.len()
                );
                let mut status = alloc::format!("{name} | {hint}");
                status.truncate(width.saturating_sub(right.len() + 1));
                let pad = width.saturating_sub(status.len() + right.len());
                status.extend(core::iter::repeat_n(' ', pad));
                status.push_str(&right);
                (status, None)
            }
        };
        let mut status = status;
        status.truncate(width);
        self.draw(text_rows, status.into_bytes(), Style::Inverse);
        self.message.clear();

        match cursor_col {
            Some(col) => console::place_cursor(text_rows, col.min(width - 1)),
            None => {
                let j = self.cursor % per_row;
                let col = if self.chars { char_col(per_row) + j } else { hex_col(j) + self.high_done as usize };
                console::place_cursor(row - self.top, col);
            }
        }
    }
}
