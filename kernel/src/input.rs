//! Keyboard and serial input: decoded into keys and handed to the task in front.
//!
//! `pump` drains the PS/2 scancode queue and COM1, decodes both into `Key`s, lets the desktop
//! take its hotkeys (Alt+Tab), and puts every other key in the key queue of the foreground task.
//! Each task reads its own queue with `read_key`. While the desktop runs only its manager task
//! pumps, so keys always go to the task in front even when that task is busy; otherwise whoever
//! reads input pumps.

use crate::sync::SpinLock;
use crate::task::{self, MAX_TASKS};
use crate::{console, keyboard, program};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A printable ASCII character or `\t`.
    Char(u8),
    /// Ctrl plus a lowercase letter, e.g. `Ctrl(b'c')`.
    Ctrl(u8),
    Enter,
    Backspace,
    Delete,
    Insert,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// Alt plus a printable key or Tab, e.g. `Alt(b'\t')`. On serial: ESC followed by the key.
    Alt(u8),
    /// Not a key: the task was asked to close (`kill`, or leaving the desktop). Whatever is
    /// waiting for input should give up and return.
    Close,
}

struct Decoders {
    keyboard: keyboard::Decoder,
    serial: SerialDecoder,
}

static DECODERS: SpinLock<Decoders> =
    SpinLock::new(Decoders { keyboard: keyboard::Decoder::new(), serial: SerialDecoder::new() });

const QUEUE_LEN: usize = 64;

/// Keys waiting for one task.
struct Queue {
    keys: [Key; QUEUE_LEN],
    head: usize,
    len: usize,
}

static QUEUES: SpinLock<[Queue; MAX_TASKS]> =
    SpinLock::new([const { Queue { keys: [Key::Enter; QUEUE_LEN], head: 0, len: 0 } }; MAX_TASKS]);

/// Adds a key to task `slot`'s queue (dropped if the queue is full).
pub fn push(slot: usize, key: Key) {
    let mut queues = QUEUES.lock();
    let Some(q) = queues.get_mut(slot) else { return };
    if q.len < QUEUE_LEN {
        let at = (q.head + q.len) % QUEUE_LEN;
        q.keys[at] = key;
        q.len += 1;
    }
}

fn pop(slot: usize) -> Option<Key> {
    let mut queues = QUEUES.lock();
    let q = queues.get_mut(slot)?;
    if q.len == 0 {
        return None;
    }
    let key = q.keys[q.head];
    q.head = (q.head + 1) % QUEUE_LEN;
    q.len -= 1;
    Some(key)
}

/// Throws away keys typed for task `slot` that it has not read.
pub fn clear_queue(slot: usize) {
    if let Some(q) = QUEUES.lock().get_mut(slot) {
        q.len = 0;
    }
}

enum Event {
    Key(Key, bool),
    /// Alt was let go on the PS/2 keyboard.
    AltUp,
}

/// Decodes all pending input and delivers it (see the module comment).
pub fn pump() {
    const MAX: usize = 32;
    let mut events: [Option<Event>; MAX] = [const { None }; MAX];
    let mut n = 0;
    {
        let mut decoders = DECODERS.lock();
        while n < MAX {
            let Some(scancode) = keyboard::pop_scancode() else { break };
            let had_alt = decoders.keyboard.alt_held();
            if let Some(key) = decoders.keyboard.feed(scancode) {
                events[n] = Some(Event::Key(key, true));
                n += 1;
            }
            if had_alt && !decoders.keyboard.alt_held() && n < MAX {
                events[n] = Some(Event::AltUp);
                n += 1;
            }
        }
        while n < MAX {
            if let Some(key) = decoders.serial.take_pending() {
                events[n] = Some(Event::Key(key, false));
                n += 1;
                continue;
            }
            let Some(byte) = console::read_serial() else { break };
            if let Some(key) = decoders.serial.feed(byte) {
                events[n] = Some(Event::Key(key, false));
                n += 1;
            }
        }
        if n < MAX {
            if let Some(key) = decoders.serial.timeout(crate::pit::ticks()) {
                events[n] = Some(Event::Key(key, false));
                n += 1;
            }
        }
    }
    for event in events.into_iter().take(n).flatten() {
        match event {
            Event::Key(key, from_keyboard) => route(key, from_keyboard),
            Event::AltUp => crate::desktop::alt_released(),
        }
    }
}

fn route(key: Key, from_keyboard: bool) {
    if crate::desktop::intercept(key, from_keyboard) {
        return;
    }
    let foreground = task::foreground();
    // Ctrl-C from the keyboard is caught in the IRQ; from serial it arrives here, and stops a
    // program the same way, even one that never reads input.
    if key == Key::Ctrl(b'c') && !from_keyboard && program::is_running_in(foreground) {
        task::request_break(foreground);
        return;
    }
    push(foreground, key);
}

fn may_pump(slot: usize) -> bool {
    !crate::desktop::is_active() || crate::desktop::is_manager(slot)
}

/// Returns the next key for the running task, sleeping while there is none.
///
/// Returns `Key::Close` (every time) once the task was asked to close, and `Ctrl-C` for a
/// Ctrl-C that the keyboard IRQ caught for this task's program.
pub fn read_key() -> Key {
    let me = task::current();
    loop {
        if may_pump(me) {
            pump();
        }
        if let Some(key) = pop(me) {
            return key;
        }
        if task::close_requested(me) {
            return Key::Close;
        }
        if task::take_break(me) {
            return Key::Ctrl(b'c');
        }
        task::wait_input();
    }
}

/// The next key for the running task, if one is waiting.
pub fn try_read_key() -> Option<Key> {
    let me = task::current();
    if may_pump(me) {
        pump();
    }
    pop(me)
}

#[derive(Clone, Copy, PartialEq)]
enum EscState {
    None,
    /// Got ESC.
    Escape,
    /// Got `ESC [` or `ESC O`, collecting a numeric parameter.
    Sequence,
}

/// A lone ESC becomes the Escape key if nothing follows within this many timer ticks (50 ms).
/// Terminals send the bytes of a sequence together, so they arrive much faster than that.
pub const ESC_TIMEOUT_TICKS: u64 = 5;

/// Decodes what a terminal sends over serial: `\r` for Enter, DEL for Backspace, ANSI escape
/// sequences such as `ESC [ A` for the arrow keys, and ESC plus a key for Alt plus that key.
pub struct SerialDecoder {
    state: EscState,
    param: u16,
    /// A `;` was seen: later parameters (modifiers) are ignored.
    param_done: bool,
    /// When the pending ESC arrived, in timer ticks.
    escape_tick: u64,
    last_was_cr: bool,
    /// A second key produced by the last byte (ESC followed by a control character).
    pending: Option<Key>,
}

impl SerialDecoder {
    pub const fn new() -> Self {
        Self {
            state: EscState::None,
            param: 0,
            param_done: false,
            escape_tick: 0,
            last_was_cr: false,
            pending: None,
        }
    }

    /// A key left over from the last `feed`; check it after every call.
    pub fn take_pending(&mut self) -> Option<Key> {
        self.pending.take()
    }

    /// Call regularly with the current tick: turns an ESC that nothing followed into Escape.
    pub fn timeout(&mut self, now: u64) -> Option<Key> {
        if self.state == EscState::None || now.saturating_sub(self.escape_tick) < ESC_TIMEOUT_TICKS {
            return None;
        }
        let was = core::mem::replace(&mut self.state, EscState::None);
        (was == EscState::Escape).then_some(Key::Escape)
    }

    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        let last_was_cr = core::mem::replace(&mut self.last_was_cr, byte == b'\r');
        match self.state {
            EscState::Escape => match byte {
                b'[' | b'O' => {
                    self.state = EscState::Sequence;
                    self.param = 0;
                    self.param_done = false;
                    None
                }
                // ESC ESC: the first one was a lone Escape.
                0x1B => {
                    self.escape_tick = crate::pit::ticks();
                    Some(Key::Escape)
                }
                b'\t' | b' '..=b'~' => {
                    self.state = EscState::None;
                    Some(Key::Alt(byte))
                }
                _ => {
                    self.state = EscState::None;
                    self.pending = self.feed(byte);
                    Some(Key::Escape)
                }
            },
            EscState::Sequence => {
                if byte.is_ascii_digit() {
                    if !self.param_done {
                        self.param = self.param.saturating_mul(10).saturating_add((byte - b'0') as u16);
                    }
                    return None;
                }
                if byte == b';' {
                    self.param_done = true;
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
                    (b'~', 2) => Some(Key::Insert),
                    (b'~', 3) => Some(Key::Delete),
                    (b'~', 5) => Some(Key::PageUp),
                    (b'~', 6) => Some(Key::PageDown),
                    _ => None,
                }
            }
            EscState::None => match byte {
                0x1B => {
                    self.state = EscState::Escape;
                    self.escape_tick = crate::pit::ticks();
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
