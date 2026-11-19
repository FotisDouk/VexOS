//! Keyboard and serial input merged into one stream of keys.

use crate::sync::SpinLock;
use crate::{console, cpu, keyboard};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A printable ASCII character or `\t`.
    Char(u8),
    /// Ctrl plus a lowercase letter, e.g. `Ctrl(b'c')`.
    Ctrl(u8),
    Enter,
    Backspace,
    Delete,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

struct Decoders {
    keyboard: keyboard::Decoder,
    serial: SerialDecoder,
}

static DECODERS: SpinLock<Decoders> =
    SpinLock::new(Decoders { keyboard: keyboard::Decoder::new(), serial: SerialDecoder::new() });

/// Returns the next key from the PS/2 keyboard or COM1, halting the CPU while there is none.
/// Needs the timer running: serial input is polled, and the tick is what wakes the CPU for it.
pub fn read_key() -> Key {
    loop {
        if let Some(key) = poll_key() {
            return key;
        }
        cpu::cli();
        if keyboard::has_scancode() || console::serial_has_input() {
            cpu::sti();
        } else {
            cpu::enable_and_hlt();
        }
    }
}

fn poll_key() -> Option<Key> {
    let mut decoders = DECODERS.lock();
    while let Some(scancode) = keyboard::pop_scancode() {
        if let Some(key) = decoders.keyboard.feed(scancode) {
            return Some(key);
        }
    }
    while let Some(byte) = console::read_serial() {
        if let Some(key) = decoders.serial.feed(byte) {
            return Some(key);
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq)]
enum EscState {
    None,
    /// Got ESC.
    Escape,
    /// Got `ESC [` or `ESC O`, collecting a numeric parameter.
    Sequence,
}

/// Decodes what a terminal sends over serial: `\r` for Enter, DEL for Backspace, and ANSI escape
/// sequences such as `ESC [ A` for the arrow keys.
pub struct SerialDecoder {
    state: EscState,
    param: u16,
    last_was_cr: bool,
}

impl SerialDecoder {
    pub const fn new() -> Self {
        Self { state: EscState::None, param: 0, last_was_cr: false }
    }

    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        let last_was_cr = core::mem::replace(&mut self.last_was_cr, byte == b'\r');
        match self.state {
            EscState::Escape => {
                self.state = match byte {
                    b'[' | b'O' => EscState::Sequence,
                    _ => EscState::None,
                };
                self.param = 0;
                None
            }
            EscState::Sequence => {
                if byte.is_ascii_digit() {
                    self.param = self.param.saturating_mul(10).saturating_add((byte - b'0') as u16);
                    return None;
                }
                if byte == b';' {
                    return None;
                }
                self.state = EscState::None;
                match (byte, self.param) {
                    (b'A', _) => Some(Key::Up),
                    (b'B', _) => Some(Key::Down),
                    (b'C', _) => Some(Key::Right),
                    (b'D', _) => Some(Key::Left),
                    (b'H', _) | (b'~', 1 | 7) => Some(Key::Home),
                    (b'F', _) | (b'~', 4 | 8) => Some(Key::End),
                    (b'~', 3) => Some(Key::Delete),
                    (b'~', 5) => Some(Key::PageUp),
                    (b'~', 6) => Some(Key::PageDown),
                    _ => None,
                }
            }
            EscState::None => match byte {
                0x1B => {
                    self.state = EscState::Escape;
                    None
                }
                b'\r' => Some(Key::Enter),
                b'\n' if last_was_cr => None,
                b'\n' => Some(Key::Enter),
                0x7F | 0x08 => Some(Key::Backspace),
                b'\t' => Some(Key::Char(b'\t')),
                0x01..=0x1A => Some(Key::Ctrl(b'a' + byte - 1)),
                b' '..=b'~' => Some(Key::Char(byte)),
                _ => None,
            },
        }
    }
}
