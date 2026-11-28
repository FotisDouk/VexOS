//! PS/2 keyboard through the 8042 controller.
//!
//! The IRQ1 handler only moves raw scancodes into a lock-free queue; decoding happens on the
//! consumer side (`input::read_key`) with a `Decoder`. On real PCs with a USB keyboard this still
//! works when the BIOS emulates a PS/2 keyboard ("USB legacy support").

use crate::input::Key;
use crate::port::{inb, outb};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
const STATUS_AUX_DATA: u8 = 1 << 5;

const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_ENABLE_PORT1: u8 = 0xAE;

const CONFIG_PORT1_IRQ: u8 = 1 << 0;
const CONFIG_PORT1_CLOCK_DISABLED: u8 = 1 << 4;
const CONFIG_TRANSLATION: u8 = 1 << 6;

/// Sets up the controller so the keyboard raises IRQ1 with scancode set 1. The controller
/// self-test and device resets are skipped on purpose: they are not needed after the BIOS has
/// initialized the keyboard, and they confuse the PS/2 emulation of some BIOSes' USB legacy
/// support. Returns false if no controller answers.
pub fn init() -> bool {
    unsafe {
        if inb(STATUS) == 0xFF {
            return false;
        }
        drain();
        if !wait_input_empty() {
            return false;
        }
        outb(COMMAND, CMD_READ_CONFIG);
        if !wait_output_full() {
            return false;
        }
        let config = inb(DATA);
        let config = (config | CONFIG_PORT1_IRQ | CONFIG_TRANSLATION) & !CONFIG_PORT1_CLOCK_DISABLED;
        wait_input_empty();
        outb(COMMAND, CMD_WRITE_CONFIG);
        wait_input_empty();
        outb(DATA, config);
        wait_input_empty();
        outb(COMMAND, CMD_ENABLE_PORT1);
        drain();
    }
    true
}

unsafe fn drain() {
    for _ in 0..64 {
        if unsafe { inb(STATUS) } & STATUS_OUTPUT_FULL == 0 {
            break;
        }
        unsafe { inb(DATA) };
    }
}

fn wait_input_empty() -> bool {
    (0..100_000).any(|_| {
        core::hint::spin_loop();
        unsafe { inb(STATUS) & STATUS_INPUT_FULL == 0 }
    })
}

fn wait_output_full() -> bool {
    (0..100_000).any(|_| {
        core::hint::spin_loop();
        unsafe { inb(STATUS) & STATUS_OUTPUT_FULL != 0 }
    })
}

// Single-producer (IRQ1) / single-consumer (`input::pump`, under its lock) ring buffer of raw
// scancodes.
const QUEUE_SIZE: usize = 128;
static QUEUE: [AtomicU8; QUEUE_SIZE] = [const { AtomicU8::new(0) }; QUEUE_SIZE];
static HEAD: AtomicUsize = AtomicUsize::new(0); // next slot to write, only the IRQ moves it
static TAIL: AtomicUsize = AtomicUsize::new(0); // next slot to read, only the consumer moves it
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
const SC_C: u8 = 0x2E;

/// The IRQ1 and IRQ12 handler: the keyboard and the mouse share the controller's output buffer,
/// so whichever IRQ comes reads the byte waiting and hands it to its owner.
pub fn handle_irq() {
    let status = unsafe { inb(STATUS) };
    if status & STATUS_OUTPUT_FULL == 0 {
        return;
    }
    let byte = unsafe { inb(DATA) };
    if status & STATUS_AUX_DATA != 0 {
        crate::mouse::irq_byte(byte);
    } else {
        scancode_in(byte);
    }
}

/// Reads every byte the controller still holds, as the IRQ handler would. `bios::call` runs this
/// after the trampoline: the trampoline re-initializes the 8259 (ICW1 clears its request
/// register), so a byte that arrived while the PIC was being reprogrammed has lost its IRQ. The
/// controller keeps the byte and holds its IRQ line high, and the 8259 needs a new edge, so
/// without this read the byte, and every key typed after it, would wait in the controller until
/// something else (a mouse packet) read port 0x60. Runs with interrupts off. Bounded: a status
/// of 0xFF (no controller, as in `init`) stops it at once, and a controller stuck at "full"
/// costs at most 32 reads.
pub fn poll_pending() {
    for _ in 0..32 {
        let status = unsafe { inb(STATUS) };
        if status == 0xFF || status & STATUS_OUTPUT_FULL == 0 {
            break;
        }
        handle_irq();
    }
}

/// Puts `byte` in the controller's output buffer as if the keyboard had sent it (controller
/// command 0xD2), for the self-tests.
#[cfg(feature = "qemu-test")]
pub fn inject_for_test(byte: u8) {
    unsafe {
        drain();
        wait_input_empty();
        outb(COMMAND, 0xD2);
        wait_input_empty();
        outb(DATA, byte);
    }
}

/// A scancode from the keyboard (from the IRQ, or read by the mouse set-up while it waited for
/// the controller). Runs with interrupts off.
pub fn scancode_in(scancode: u8) {
    // Ctrl-C stops the program of the task in front; track Ctrl here because decoding happens
    // later (and maybe not at all, if the program never reads input).
    let foreground = crate::task::foreground();
    match scancode {
        SC_CTRL => CTRL_DOWN.store(true, Ordering::Relaxed),
        0x9D => CTRL_DOWN.store(false, Ordering::Relaxed),
        SC_C if CTRL_DOWN.load(Ordering::Relaxed) && crate::program::is_running_in(foreground) => {
            crate::task::request_break(foreground);
            return;
        }
        _ => {}
    }
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % QUEUE_SIZE;
    if next == TAIL.load(Ordering::Acquire) {
        return; // full: drop the key
    }
    QUEUE[head].store(scancode, Ordering::Relaxed);
    HEAD.store(next, Ordering::Release);
}

pub fn pop_scancode() -> Option<u8> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let scancode = QUEUE[tail].load(Ordering::Relaxed);
    TAIL.store((tail + 1) % QUEUE_SIZE, Ordering::Release);
    Some(scancode)
}

pub fn has_scancode() -> bool {
    TAIL.load(Ordering::Relaxed) != HEAD.load(Ordering::Acquire)
}

// Scancode set 1, US layout, indexed by make code. 0 = no character.
const NORMAL: &[u8; 0x3A] =
    b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: &[u8; 0x3A] =
    b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";
/// Keypad 0x47..=0x53 with Num Lock on.
const KEYPAD: &[u8; 13] = b"789-456+1230.";

const SC_ESCAPE: u8 = 0x01;
const SC_BACKSPACE: u8 = 0x0E;
const SC_ENTER: u8 = 0x1C;
const SC_CTRL: u8 = 0x1D;
const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_ALT: u8 = 0x38;
const SC_CAPS_LOCK: u8 = 0x3A;
const SC_NUM_LOCK: u8 = 0x45;
const SC_KEYPAD_FIRST: u8 = 0x47;
const SC_KEYPAD_LAST: u8 = 0x53;
const SC_UP: u8 = 0x48;
const SC_LEFT: u8 = 0x4B;
const SC_RIGHT: u8 = 0x4D;
const SC_DOWN: u8 = 0x50;
const SC_HOME: u8 = 0x47;
const SC_PAGE_UP: u8 = 0x49;
const SC_PAGE_DOWN: u8 = 0x51;
const SC_END: u8 = 0x4F;
const SC_DELETE: u8 = 0x53;
const SC_INSERT: u8 = 0x52;
const SC_KEYPAD_SLASH: u8 = 0x35;

const PREFIX_EXTENDED: u8 = 0xE0;
const PREFIX_PAUSE: u8 = 0xE1;
const RELEASE: u8 = 0x80;

/// Turns scancode set 1 bytes into keys, tracking modifier and lock state.
pub struct Decoder {
    extended: bool,
    skip: u8,
    shift_left: bool,
    shift_right: bool,
    ctrl: bool,
    /// Left Alt or right Alt (AltGr).
    alt: bool,
    caps_lock: bool,
    num_lock: bool,
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            extended: false,
            skip: 0,
            shift_left: false,
            shift_right: false,
            ctrl: false,
            alt: false,
            caps_lock: false,
            num_lock: true,
        }
    }

    /// True while an Alt key is held down (the task switcher stays open until it is let go).
    pub fn alt_held(&self) -> bool {
        self.alt
    }

    pub fn feed(&mut self, scancode: u8) -> Option<Key> {
        if self.skip > 0 {
            self.skip -= 1;
            return None;
        }
        match scancode {
            PREFIX_EXTENDED => {
                self.extended = true;
                return None;
            }
            PREFIX_PAUSE => {
                // Pause sends E1 1D 45 E1 9D C5 and has no release.
                self.skip = 5;
                return None;
            }
            _ => {}
        }
        let extended = core::mem::take(&mut self.extended);
        let released = scancode & RELEASE != 0;
        let code = scancode & !RELEASE;

        match code {
            SC_LSHIFT | SC_RSHIFT if extended => return None, // fake shifts around PrtSc etc.
            SC_LSHIFT => self.shift_left = !released,
            SC_RSHIFT => self.shift_right = !released,
            SC_CTRL => self.ctrl = !released,
            SC_ALT => self.alt = !released,
            SC_CAPS_LOCK if !released => self.caps_lock = !self.caps_lock,
            SC_NUM_LOCK if !released => self.num_lock = !self.num_lock,
            _ if released => {}
            _ if extended => return Self::extended_key(code),
            _ => return self.normal_key(code),
        }
        None
    }

    fn extended_key(code: u8) -> Option<Key> {
        Some(match code {
            SC_UP => Key::Up,
            SC_DOWN => Key::Down,
            SC_LEFT => Key::Left,
            SC_RIGHT => Key::Right,
            SC_HOME => Key::Home,
            SC_END => Key::End,
            SC_DELETE => Key::Delete,
            SC_INSERT => Key::Insert,
            SC_PAGE_UP => Key::PageUp,
            SC_PAGE_DOWN => Key::PageDown,
            SC_ENTER => Key::Enter,
            SC_KEYPAD_SLASH => Key::Char(b'/'),
            _ => return None,
        })
    }

    fn normal_key(&self, code: u8) -> Option<Key> {
        match code {
            SC_ESCAPE => return Some(Key::Escape),
            SC_BACKSPACE => return Some(Key::Backspace),
            SC_ENTER => return Some(Key::Enter),
            SC_KEYPAD_FIRST..=SC_KEYPAD_LAST => {
                return if self.num_lock {
                    Some(Key::Char(KEYPAD[(code - SC_KEYPAD_FIRST) as usize]))
                } else {
                    Self::extended_key(code)
                };
            }
            _ => {}
        }
        let normal = *NORMAL.get(code as usize)?;
        if normal == 0 {
            return None;
        }
        if self.alt {
            return Some(Key::Alt(normal));
        }
        if self.ctrl && normal.is_ascii_lowercase() {
            return Some(Key::Ctrl(normal));
        }
        let shift = self.shift_left || self.shift_right;
        let use_shifted = if normal.is_ascii_lowercase() { shift != self.caps_lock } else { shift };
        Some(Key::Char(if use_shifted { SHIFTED[code as usize] } else { normal }))
    }
}
