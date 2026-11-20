//! Full-screen text editor (`edit <file>`), drawn on the VGA screen and mirrored to a serial
//! terminal. Rows 0-22 show the text, row 23 is the status bar.
//!
//! Keys: arrows, Home/End, PgUp/PgDn, Backspace/Delete, Enter, Tab (4 spaces).
//! Ctrl-S or Ctrl-O saves; Ctrl-Q or Ctrl-X quits (twice if there are unsaved changes).

use crate::console::{self, Style};
use crate::fs;
use crate::input::{self, Key};
use crate::vga::WIDTH;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const TEXT_ROWS: usize = 23;
const STATUS_ROW: usize = 23;
const TAB: usize = 4;

pub struct Editor {
    path: String,
    lines: Vec<Vec<u8>>,
    /// Cursor position in the text.
    line: usize,
    col: usize,
    /// First visible line and column.
    top: usize,
    left: usize,
    modified: bool,
    quit_armed: bool,
    message: String,
    /// What each screen row currently shows, so only changed rows are redrawn.
    shown: Vec<Option<(Vec<u8>, Style)>>,
}

impl Editor {
    /// Opens `path`, or starts an empty buffer if the file does not exist yet.
    pub fn open(path: &str) -> fs::Result<Editor> {
        let (lines, message) = match fs::with(|fs| fs.read(path)) {
            Ok(data) if !survives_text_editing(&data) => (
                split_lines(&data),
                String::from("Note: tabs, CR, binary bytes change on save (edit -x: hex)"),
            ),
            Ok(data) => (split_lines(&data), alloc::format!("{} bytes", data.len())),
            Err(fs::FsError::Fs(kfs::Error::NotFound)) => (vec![Vec::new()], String::from("new file")),
            Err(e) => return Err(e),
        };
        Ok(Editor {
            path: String::from(path),
            lines,
            line: 0,
            col: 0,
            top: 0,
            left: 0,
            modified: false,
            quit_armed: false,
            message,
            shown: vec![None; TEXT_ROWS + 1],
        })
    }

    /// Puts the cursor at the start of line `number` (1-based; past the end = the last line).
    pub fn go_to_line(mut self, number: usize) -> Editor {
        self.line = number.saturating_sub(1).min(self.lines.len() - 1);
        self
    }

    /// Runs until the user quits. Clears the screen afterwards.
    pub fn run(mut self) {
        console::clear();
        loop {
            self.render();
            let key = input::read_key();
            if !self.handle(key) {
                break;
            }
        }
        console::clear();
    }

    /// Applies one key. Returns false to quit.
    pub fn handle(&mut self, key: Key) -> bool {
        let quit_armed = core::mem::take(&mut self.quit_armed);
        match key {
            Key::Ctrl(b's' | b'o') => self.save(),
            Key::Ctrl(b'q' | b'x') => {
                if !self.modified || quit_armed {
                    return false;
                }
                self.quit_armed = true;
                self.message = String::from("Unsaved changes! Press Ctrl-Q again to quit without saving.");
            }
            Key::Char(b'\t') => {
                let spaces = TAB - self.col % TAB;
                for _ in 0..spaces {
                    self.insert(b' ');
                }
            }
            Key::Char(c) => self.insert(c),
            Key::Enter => {
                let rest = self.lines[self.line].split_off(self.col);
                self.lines.insert(self.line + 1, rest);
                self.line += 1;
                self.col = 0;
                self.modified = true;
            }
            Key::Backspace => {
                if self.col > 0 {
                    self.col -= 1;
                    self.lines[self.line].remove(self.col);
                    self.modified = true;
                } else if self.line > 0 {
                    let current = self.lines.remove(self.line);
                    self.line -= 1;
                    self.col = self.lines[self.line].len();
                    self.lines[self.line].extend(current);
                    self.modified = true;
                }
            }
            Key::Delete => {
                if self.col < self.lines[self.line].len() {
                    self.lines[self.line].remove(self.col);
                    self.modified = true;
                } else if self.line + 1 < self.lines.len() {
                    let next = self.lines.remove(self.line + 1);
                    self.lines[self.line].extend(next);
                    self.modified = true;
                }
            }
            Key::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.line > 0 {
                    self.line -= 1;
                    self.col = self.lines[self.line].len();
                }
            }
            Key::Right => {
                if self.col < self.lines[self.line].len() {
                    self.col += 1;
                } else if self.line + 1 < self.lines.len() {
                    self.line += 1;
                    self.col = 0;
                }
            }
            Key::Up => self.move_lines(-1),
            Key::Down => self.move_lines(1),
            Key::PageUp => self.move_lines(-(TEXT_ROWS as isize - 1)),
            Key::PageDown => self.move_lines(TEXT_ROWS as isize - 1),
            Key::Home => self.col = 0,
            Key::End => self.col = self.lines[self.line].len(),
            _ => {}
        }
        true
    }

    fn insert(&mut self, c: u8) {
        self.lines[self.line].insert(self.col, c);
        self.col += 1;
        self.modified = true;
    }

    fn move_lines(&mut self, delta: isize) {
        let target = (self.line as isize + delta).clamp(0, self.lines.len() as isize - 1);
        self.line = target as usize;
        self.col = self.col.min(self.lines[self.line].len());
    }

    pub fn text(&self) -> Vec<u8> {
        let mut data = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            data.extend_from_slice(line);
            if i + 1 < self.lines.len() || !line.is_empty() {
                data.push(b'\n');
            }
        }
        data
    }

    fn save(&mut self) {
        let data = self.text();
        self.message = match fs::with(|fs| fs.write(&self.path, &data)) {
            Ok(()) => {
                self.modified = false;
                alloc::format!("Saved {} bytes", data.len())
            }
            Err(e) => alloc::format!("Save failed: {e}"),
        };
    }

    fn scroll_into_view(&mut self) {
        if self.line < self.top {
            self.top = self.line;
        } else if self.line >= self.top + TEXT_ROWS {
            self.top = self.line + 1 - TEXT_ROWS;
        }
        if self.col < self.left {
            self.left = self.col;
        } else if self.col >= self.left + WIDTH {
            self.left = self.col + 1 - WIDTH;
        }
    }

    fn draw(&mut self, row: usize, text: Vec<u8>, style: Style) {
        let wanted = Some((text, style));
        if self.shown[row] != wanted {
            let (text, style) = wanted.as_ref().unwrap();
            console::draw_row(row, text, *style);
            self.shown[row] = wanted;
        }
    }

    fn render(&mut self) {
        self.scroll_into_view();
        for row in 0..TEXT_ROWS {
            let (text, style) = match self.lines.get(self.top + row) {
                Some(line) => {
                    let visible = line.get(self.left..).unwrap_or(&[]);
                    (visible[..visible.len().min(WIDTH)].to_vec(), Style::Normal)
                }
                None => (b"~".to_vec(), Style::Dim),
            };
            self.draw(row, text, style);
        }
        let name = alloc::format!(" {}{}", self.path, if self.modified { " [modified]" } else { "" });
        let position = alloc::format!("Ln {}, Col {} ", self.line + 1, self.col + 1);
        let hint = if self.message.is_empty() { "^S save  ^Q quit" } else { self.message.as_str() };
        let mut status = alloc::format!("{name}  |  {hint}");
        let pad = WIDTH.saturating_sub(status.len() + position.len());
        status.extend(core::iter::repeat_n(' ', pad));
        status.push_str(&position);
        status.truncate(WIDTH);
        self.draw(STATUS_ROW, status.into_bytes(), Style::Inverse);
        self.message.clear();
        console::place_cursor(self.line - self.top, self.col - self.left);
    }
}

/// False if saving the text unchanged would still change the file: the editor expands tabs,
/// drops `\r`, and shows other control or non-ASCII bytes as they are on screen.
fn survives_text_editing(data: &[u8]) -> bool {
    data.iter().all(|&b| b == b'\n' || (0x20..0x7F).contains(&b))
}

/// Splits file contents into lines, dropping `\r` and expanding tabs.
fn split_lines(data: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = vec![Vec::new()];
    for &b in data {
        match b {
            b'\n' => lines.push(Vec::new()),
            b'\r' => {}
            b'\t' => {
                let line = lines.last_mut().unwrap();
                let spaces = TAB - line.len() % TAB;
                line.extend(core::iter::repeat_n(b' ', spaces));
            }
            _ => lines.last_mut().unwrap().push(b),
        }
    }
    if lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines
}
