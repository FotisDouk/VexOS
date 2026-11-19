use crate::port::{inb, outb};
use core::fmt;
use core::ptr::{read_volatile, write_volatile};

const BUFFER: *mut u16 = 0xB8000 as *mut u16;
const WIDTH: usize = 80;
const HEIGHT: usize = 25;
const BACKSPACE: u8 = 0x08;

const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;
const CRTC_CURSOR_START: u8 = 0x0A;
const CRTC_CURSOR_END: u8 = 0x0B;
const CRTC_CURSOR_HIGH: u8 = 0x0E;
const CRTC_CURSOR_LOW: u8 = 0x0F;

#[allow(dead_code)]
#[derive(Clone, Copy)]
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

pub struct Vga {
    row: usize,
    col: usize,
    attr: u8,
}

impl Vga {
    pub const fn new() -> Self {
        Self { row: 0, col: 0, attr: (Color::Black as u8) << 4 | Color::LightGray as u8 }
    }

    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.attr = (bg as u8) << 4 | fg as u8;
    }

    pub fn clear(&mut self) {
        for row in 0..HEIGHT {
            self.clear_row(row);
        }
        self.row = 0;
        self.col = 0;
        self.update_cursor();
    }

    /// Turns on the hardware cursor as an underline (scanlines 14-15).
    pub fn show_cursor(&mut self) {
        unsafe {
            outb(CRTC_INDEX, CRTC_CURSOR_START);
            outb(CRTC_DATA, (inb(CRTC_DATA) & 0xC0) | 14);
            outb(CRTC_INDEX, CRTC_CURSOR_END);
            outb(CRTC_DATA, (inb(CRTC_DATA) & 0xE0) | 15);
        }
        self.update_cursor();
    }

    fn update_cursor(&mut self) {
        let pos = (self.row * WIDTH + self.col.min(WIDTH - 1)) as u16;
        unsafe {
            outb(CRTC_INDEX, CRTC_CURSOR_LOW);
            outb(CRTC_DATA, pos as u8);
            outb(CRTC_INDEX, CRTC_CURSOR_HIGH);
            outb(CRTC_DATA, (pos >> 8) as u8);
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
                self.put(self.row, self.col, byte);
                self.col += 1;
            }
        }
    }

    fn put(&mut self, row: usize, col: usize, byte: u8) {
        let cell = (self.attr as u16) << 8 | byte as u16;
        unsafe { write_volatile(BUFFER.add(row * WIDTH + col), cell) };
    }

    fn new_line(&mut self) {
        self.col = 0;
        if self.row + 1 < HEIGHT {
            self.row += 1;
            return;
        }
        for i in 0..(HEIGHT - 1) * WIDTH {
            unsafe { write_volatile(BUFFER.add(i), read_volatile(BUFFER.add(i + WIDTH))) };
        }
        self.clear_row(HEIGHT - 1);
    }

    fn clear_row(&mut self, row: usize) {
        for col in 0..WIDTH {
            self.put(row, col, b' ');
        }
    }
}

impl fmt::Write for Vga {
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
