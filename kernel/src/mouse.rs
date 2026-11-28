//! PS/2 mouse on the 8042 controller's auxiliary port (IRQ12).
//!
//! The keyboard and the mouse share the controller's one output buffer, so both IRQ handlers
//! read it the same way (`keyboard::handle_irq`): a byte with the "auxiliary" status bit goes to
//! `irq_byte` here, anything else to the keyboard. `irq_byte` puts bytes together into 3-byte
//! packets, or 4-byte ones once the mouse has agreed to IntelliMouse mode (a wheel), resyncing on
//! bit 3 of the first byte (always set) and after a gap, and queues them, lock-free. `poll`, called by `input::pump`, turns packets into a position
//! and events, and hands the events to whoever uses the mouse:
//!
//! - on the desktop, the desktop manager (the taskbar, and the home screen when it is in front);
//! - a task in front that asked for events with `grab` (paint, `mouse -t`).
//!
//! Nobody else gets mouse events. A press is "captured": its target gets every event until the
//! last button is let go, so a drag that leaves an area still ends where it started.
//!
//! The pointer is drawn by the display (`display::set_pointer`), only on the desktop and for a
//! task that grabbed it with `Use::Pointer`; it shows from the first mouse event and hides on a
//! key press until the mouse moves again.
//!
//! Positions are in "area pixels": in a graphics mode the pixels of the text area (1 count = 1
//! pixel), in text mode a virtual 8x16 grid per cell. `display::cell_px` gives the cell size, so
//! the cell under the pointer is `(y / cell_h, x / cell_w)` in every mode.
//!
//! No mouse is not an error: every wait on the controller is bounded, a failed set-up puts the
//! controller's configuration back as it was, and the mouse is then "not found". A mouse without
//! a wheel (or one that does not answer the IntelliMouse knock) stays in 3-byte mode.

use crate::port::{inb, outb};
use crate::sync::SpinLock;
use crate::task::MAX_TASKS;
use crate::theme::{self, Item};
use crate::{display, fs, pit};
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

pub const CONFIG_PATH: &str = "/system/mouse.cfg";

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
pub const STATUS_AUX_DATA: u8 = 1 << 5;
const STATUS_TIMEOUT: u8 = 1 << 6;
const STATUS_PARITY: u8 = 1 << 7;

const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_DISABLE_AUX: u8 = 0xA7;
const CMD_ENABLE_AUX: u8 = 0xA8;
const CMD_WRITE_AUX: u8 = 0xD4;

const CONFIG_AUX_IRQ: u8 = 1 << 1;
const CONFIG_AUX_CLOCK_DISABLED: u8 = 1 << 5;

const DEV_SET_DEFAULTS: u8 = 0xF6;
const DEV_SET_RATE: u8 = 0xF3;
const DEV_GET_ID: u8 = 0xF2;
/// The sample rate after `DEV_SET_DEFAULTS`, put back after the IntelliMouse knock.
const DEFAULT_RATE: u8 = 100;
const DEV_ENABLE_REPORTING: u8 = 0xF4;
const DEV_DISABLE_REPORTING: u8 = 0xF5;
const DEV_ACK: u8 = 0xFA;
const DEV_RESEND: u8 = 0xFE;

/// Polls of the status port before a controller wait gives up. A port read takes about a
/// microsecond on real hardware, so this is roughly 50 ms; the set-up waits at most a few times.
const WAIT_POLLS: u32 = 50_000;

/// Bytes of one packet that arrive this many ticks apart or more do not belong together: a byte
/// was lost (e.g. during a BIOS call), so the packet starts again.
const PACKET_GAP_TICKS: u64 = 2;

/// A double-click: two left presses within this many ticks (400 ms), on the same target.
pub const DOUBLE_CLICK_TICKS: u64 = 40;

// --- Status ---

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// `/system/mouse.cfg` says `mouse off`, so the port was never touched.
    NotProbed,
    Found,
    NotFound,
}

/// 0 = not probed, 1 = found, 2 = not found.
static STATUS_NOW: AtomicU8 = AtomicU8::new(0);
/// The user's choice (`mouse on` / `mouse off`).
static ENABLED: AtomicBool = AtomicBool::new(true);
/// The mouse sends 4-byte packets with a wheel byte (IntelliMouse, device id 3 or 4).
static WHEEL: AtomicBool = AtomicBool::new(false);
/// Serialises talking to the controller outside the IRQ handlers (a lock holder has interrupts
/// off, so no IRQ handler takes the controller's answers away meanwhile).
static CONTROLLER: SpinLock<()> = SpinLock::new(());

pub fn status() -> Status {
    match STATUS_NOW.load(Ordering::Acquire) {
        1 => Status::Found,
        2 => Status::NotFound,
        _ => Status::NotProbed,
    }
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

fn working() -> bool {
    status() == Status::Found && is_enabled()
}

/// The mouse has a wheel (it answered the IntelliMouse knock).
pub fn has_wheel() -> bool {
    WHEEL.load(Ordering::Acquire)
}

// --- Talking to the controller (only while holding `CONTROLLER`) ---

fn wait_input_empty() -> bool {
    (0..WAIT_POLLS).any(|_| unsafe { inb(STATUS) } & STATUS_INPUT_FULL == 0)
}

/// Waits for a byte from the controller: from the mouse if `aux`, else a controller answer.
/// Keyboard bytes that come meanwhile go to the keyboard, so typing during the set-up loses
/// nothing; stray mouse bytes are dropped.
fn read_byte(aux: bool) -> Option<u8> {
    for _ in 0..WAIT_POLLS {
        let status = unsafe { inb(STATUS) };
        if status & STATUS_OUTPUT_FULL == 0 {
            core::hint::spin_loop();
            continue;
        }
        let byte = unsafe { inb(DATA) };
        let from_aux = status & STATUS_AUX_DATA != 0;
        if from_aux == aux {
            if status & (STATUS_TIMEOUT | STATUS_PARITY) != 0 {
                return None;
            }
            return Some(byte);
        }
        if !from_aux {
            crate::keyboard::scancode_in(byte);
        }
    }
    None
}

fn controller_command(command: u8) -> bool {
    if !wait_input_empty() {
        return false;
    }
    unsafe { outb(COMMAND, command) };
    true
}

fn read_config() -> Option<u8> {
    if !controller_command(CMD_READ_CONFIG) {
        return None;
    }
    read_byte(false)
}

fn write_config(config: u8) -> bool {
    if !controller_command(CMD_WRITE_CONFIG) || !wait_input_empty() {
        return false;
    }
    unsafe { outb(DATA, config) };
    true
}

/// Sends a command to the mouse and waits for its acknowledgement (one resend allowed).
fn device_command(command: u8) -> bool {
    for _ in 0..2 {
        if !controller_command(CMD_WRITE_AUX) || !wait_input_empty() {
            return false;
        }
        unsafe { outb(DATA, command) };
        match read_byte(true) {
            Some(DEV_ACK) => return true,
            Some(DEV_RESEND) => continue,
            _ => return false,
        }
    }
    false
}

/// Empties the output buffer (keyboard bytes go to the keyboard, mouse bytes are dropped).
fn drain() {
    for _ in 0..64 {
        let status = unsafe { inb(STATUS) };
        if status & STATUS_OUTPUT_FULL == 0 {
            break;
        }
        let byte = unsafe { inb(DATA) };
        if status & STATUS_AUX_DATA == 0 {
            crate::keyboard::scancode_in(byte);
        }
    }
}

/// Sets the sample rate (two bytes, each acknowledged).
fn set_rate(rate: u8) -> bool {
    device_command(DEV_SET_RATE) && device_command(rate)
}

/// The IntelliMouse knock: sample rates 200, 100, 80, then the device id. A wheel mouse answers
/// 3 (or 4) and from then on sends 4-byte packets; any other mouse answers 0 and is unchanged.
/// The rate goes back to the default either way. True if the mouse now sends 4-byte packets.
fn try_wheel() -> bool {
    let knocked = set_rate(200) && set_rate(100) && set_rate(80);
    let id = if knocked && device_command(DEV_GET_ID) { read_byte(true) } else { None };
    set_rate(DEFAULT_RATE);
    matches!(id, Some(3 | 4))
}

/// The set-up steps; false as soon as one fails.
fn try_enable() -> bool {
    if !controller_command(CMD_ENABLE_AUX) {
        return false;
    }
    // A controller with an auxiliary port starts its clock now; a single-port one ignores the
    // command and the bit stays set.
    let Some(config) = read_config() else { return false };
    if config & CONFIG_AUX_CLOCK_DISABLED != 0 {
        return false;
    }
    if !device_command(DEV_SET_DEFAULTS) {
        return false;
    }
    // Before reporting is on, so no packet of the old size is on its way meanwhile.
    WHEEL.store(try_wheel(), Ordering::Release);
    if !device_command(DEV_ENABLE_REPORTING) {
        WHEEL.store(false, Ordering::Release);
        return false;
    }
    drain();
    write_config(config | CONFIG_AUX_IRQ)
}

/// Looks for a mouse and turns it on. Without one this takes well under a second, and the
/// controller's configuration (the keyboard's setup) is put back exactly as it was.
fn probe() -> bool {
    let _controller = CONTROLLER.lock();
    if unsafe { inb(STATUS) } == 0xFF {
        return false;
    }
    drain();
    let Some(original) = read_config() else { return false };
    let found = try_enable();
    if !found {
        controller_command(CMD_DISABLE_AUX);
        drain();
        write_config(original & !CONFIG_AUX_IRQ);
        drain();
    }
    reset_decoder();
    found
}

/// Probes the mouse (if not yet found) and records the result.
fn start() -> Status {
    if status() == Status::Found {
        return Status::Found;
    }
    let found = probe();
    STATUS_NOW.store(if found { 1 } else { 2 }, Ordering::Release);
    status()
}

/// At boot, after the file system: reads `/system/mouse.cfg` and, unless it says `mouse off`,
/// looks for a mouse.
pub fn init() -> Status {
    let on = match fs::with(|fs| fs.read(CONFIG_PATH)) {
        Ok(data) => parse_config(&String::from_utf8_lossy(&data)).unwrap_or(true),
        Err(_) => true,
    };
    ENABLED.store(on, Ordering::Release);
    if on {
        start()
    } else {
        Status::NotProbed
    }
}

/// `mouse on` / `mouse off`: takes effect at once and is saved. Turning it on looks for a mouse
/// if there was none yet.
pub fn set_enabled(on: bool) -> Result<Status, String> {
    let save = fs::with(|fs| fs.write(CONFIG_PATH, config_text(on).as_bytes()));
    if on {
        ENABLED.store(true, Ordering::Release);
        if status() == Status::Found {
            let _controller = CONTROLLER.lock();
            device_command(DEV_ENABLE_REPORTING);
            drain();
            reset_decoder();
        } else {
            start();
        }
    } else {
        ENABLED.store(false, Ordering::Release);
        if status() == Status::Found {
            // Best effort: the mouse stops sending; anything it still sends is read and dropped.
            let _controller = CONTROLLER.lock();
            device_command(DEV_DISABLE_REPORTING);
            drain();
            reset_decoder();
        }
        let mut state = STATE.lock();
        state.tracker.buttons = 0;
        state.capture = None;
        state.shown = false;
    }
    refresh();
    save.map_err(|e| alloc::format!("{CONFIG_PATH}: {e}"))?;
    Ok(status())
}

pub fn config_text(on: bool) -> String {
    String::from(if on { "mouse on\n" } else { "mouse off\n" })
}

/// `mouse on` or `mouse off` (`#` lines are comments).
pub fn parse_config(text: &str) -> Option<bool> {
    let mut result = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some("mouse"), Some("on"), None) => result = Some(true),
            (Some("mouse"), Some("off"), None) => result = Some(false),
            _ => return None,
        }
    }
    result
}

// --- Packets ---

/// Puts mouse bytes together into packets of 3 bytes, or 4 with a wheel. Pure, so the self-tests
/// drive it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PacketDecoder {
    bytes: [u8; 4],
    have: u8,
    last_tick: u64,
    /// 3, or 4 for a wheel mouse.
    len: u8,
}

impl PacketDecoder {
    /// A decoder for 3-byte packets.
    pub const fn new() -> Self {
        Self::with_wheel(false)
    }

    /// A decoder for 4-byte packets if `wheel`, else 3-byte ones.
    pub const fn with_wheel(wheel: bool) -> Self {
        Self { bytes: [0; 4], have: 0, last_tick: 0, len: if wheel { 4 } else { 3 } }
    }

    /// The next byte from the mouse, at timer tick `tick`; a whole packet when it is complete
    /// (the fourth byte is 0 in 3-byte packets).
    pub fn feed(&mut self, byte: u8, tick: u64) -> Option<[u8; 4]> {
        if self.have > 0 && tick.saturating_sub(self.last_tick) >= PACKET_GAP_TICKS {
            self.have = 0;
        }
        self.last_tick = tick;
        // The first byte always has bit 3 set: anything else is out of step, so skip it.
        if self.have == 0 && byte & 0x08 == 0 {
            return None;
        }
        self.bytes[self.have as usize] = byte;
        self.have += 1;
        if self.have < self.len {
            return None;
        }
        self.have = 0;
        let mut packet = self.bytes;
        if self.len == 3 {
            packet[3] = 0;
        }
        Some(packet)
    }
}

/// One packet: buttons held (bit 0 left, bit 1 right, bit 2 middle) and the motion, with the
/// hardware's "up" as positive `dy`; `dz` is the wheel, negative when it turns away from the
/// user (scrolling up).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Packet {
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
    pub dz: i8,
}

impl Packet {
    /// A packet from the decoder (the fourth byte is the wheel, 0 without one).
    pub fn parse(bytes: [u8; 4]) -> Packet {
        let [flags, x, y, z] = bytes;
        // 9-bit two's complement: the sign bits are in the first byte. An overflowed axis is
        // nonsense, so it counts as no motion.
        let delta = |low: u8, sign: bool, overflow: bool| -> i16 {
            match (overflow, sign) {
                (true, _) => 0,
                (false, true) => low as i16 - 256,
                (false, false) => low as i16,
            }
        };
        Packet {
            buttons: flags & 0x07,
            dx: delta(x, flags & 0x10 != 0, flags & 0x40 != 0),
            dy: delta(y, flags & 0x20 != 0, flags & 0x80 != 0),
            // Id 3 sends a signed byte, id 4 a signed nibble (with button bits above): the low
            // nibble, sign-extended, reads both, since one notch is +-1.
            dz: ((z << 4) as i8) >> 4,
        }
    }
}

// The IRQ side: decoder state in atomics (only the IRQ handlers and, with interrupts off, the
// set-up touch it), then a single-producer / single-consumer ring of packets (4 bytes each).
// `DECODER_BYTES` holds the first three bytes and the count of bytes so far.
static DECODER_BYTES: AtomicU32 = AtomicU32::new(0);
static DECODER_TICK: AtomicU64 = AtomicU64::new(0);
const RING: usize = 64;
static PACKETS: [AtomicU32; RING] = [const { AtomicU32::new(0) }; RING];
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

fn reset_decoder() {
    DECODER_BYTES.store(0, Ordering::Relaxed);
}

/// A byte from the auxiliary port (called from the IRQ1/IRQ12 handlers, interrupts off).
pub fn irq_byte(byte: u8) {
    if !working() {
        return;
    }
    let packed = DECODER_BYTES.load(Ordering::Relaxed);
    let mut decoder = PacketDecoder {
        bytes: [packed as u8, (packed >> 8) as u8, (packed >> 16) as u8, 0],
        have: (packed >> 24) as u8,
        last_tick: DECODER_TICK.load(Ordering::Relaxed),
        len: if WHEEL.load(Ordering::Relaxed) { 4 } else { 3 },
    };
    let packet = decoder.feed(byte, pit::ticks());
    DECODER_BYTES.store(
        decoder.bytes[0] as u32
            | (decoder.bytes[1] as u32) << 8
            | (decoder.bytes[2] as u32) << 16
            | (decoder.have as u32) << 24,
        Ordering::Relaxed,
    );
    DECODER_TICK.store(decoder.last_tick, Ordering::Relaxed);
    let Some(bytes) = packet else { return };
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % RING;
    if next == TAIL.load(Ordering::Acquire) {
        return; // full: drop the packet
    }
    PACKETS[head].store(u32::from_le_bytes(bytes), Ordering::Relaxed);
    HEAD.store(next, Ordering::Release);
}

/// A packet is waiting (wakes tasks waiting for input).
pub fn has_packet() -> bool {
    TAIL.load(Ordering::Relaxed) != HEAD.load(Ordering::Acquire)
}

fn pop_packet() -> Option<[u8; 4]> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let bytes = PACKETS[tail].load(Ordering::Relaxed).to_le_bytes();
    TAIL.store((tail + 1) % RING, Ordering::Release);
    Some(bytes)
}

// --- Events ---

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub const ALL: [Button; 3] = [Button::Left, Button::Right, Button::Middle];

    pub fn bit(self) -> u8 {
        match self {
            Button::Left => 1,
            Button::Right => 2,
            Button::Middle => 4,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Move,
    Down(Button),
    Up(Button),
    /// The wheel turned by this many notches: negative away from the user (scrolling up),
    /// positive towards. Only a wheel mouse sends it.
    Wheel(i8),
}

/// A mouse event, delivered as `input::Key::Mouse`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event {
    pub action: Action,
    /// The pointer's position after the event, in area pixels (see the module comment).
    pub x: u16,
    pub y: u16,
    /// The motion of a `Move`, in mouse counts, with down positive.
    pub dx: i16,
    pub dy: i16,
    /// Buttons held after the event (`Button::bit`).
    pub buttons: u8,
}

impl Event {
    /// The text cell under the pointer: (row, column), for a cell size from `display::cell_px`.
    pub fn cell(&self, cell_px: (usize, usize)) -> (usize, usize) {
        (self.y as usize / cell_px.1.max(1), self.x as usize / cell_px.0.max(1))
    }

    pub fn held(&self, button: Button) -> bool {
        self.buttons & button.bit() != 0
    }
}

/// The pointer's position and the buttons held. Pure, so the self-tests drive it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tracker {
    pub x: i32,
    pub y: i32,
    pub buttons: u8,
}

impl Tracker {
    pub const fn new() -> Self {
        Self { x: 0, y: 0, buttons: 0 }
    }

    /// Keeps the position inside an area of `w` x `h` area pixels.
    pub fn clamp(&mut self, (w, h): (usize, usize)) {
        self.x = self.x.clamp(0, (w as i32 - 1).max(0));
        self.y = self.y.clamp(0, (h as i32 - 1).max(0));
    }

    /// Applies one packet in an area of `w` x `h`: a `Move` (if it moved), a `Wheel` (if the
    /// wheel turned), then a `Down` or `Up` for each button that changed. Returns how many of
    /// `out` it filled (at most 5).
    pub fn apply(&mut self, packet: Packet, area: (usize, usize), out: &mut [Event; 5]) -> usize {
        let mut n = 0;
        let event = |t: &Tracker, action, dx, dy| Event {
            action,
            x: t.x as u16,
            y: t.y as u16,
            dx,
            dy,
            buttons: t.buttons,
        };
        if packet.dx != 0 || packet.dy != 0 {
            self.x += packet.dx as i32;
            self.y -= packet.dy as i32;
            self.clamp(area);
            out[n] = event(self, Action::Move, packet.dx, -packet.dy);
            n += 1;
        }
        if packet.dz != 0 {
            out[n] = event(self, Action::Wheel(packet.dz), 0, 0);
            n += 1;
        }
        for button in Button::ALL {
            let (was, now) = (self.buttons & button.bit() != 0, packet.buttons & button.bit() != 0);
            if was != now {
                self.buttons ^= button.bit();
                out[n] = event(self, if now { Action::Down(button) } else { Action::Up(button) }, 0, 0);
                n += 1;
            }
        }
        n
    }
}

/// Tells a double-click from two clicks: two left presses on the same target (an icon, a cell)
/// within `DOUBLE_CLICK_TICKS`. A third quick press starts a new count. Pure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DoubleClick {
    last: Option<(u64, usize)>,
}

impl DoubleClick {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// A left press on `target` at tick `now`: true if it completes a double-click.
    pub fn press(&mut self, target: usize, now: u64) -> bool {
        match self.last {
            Some((tick, t)) if t == target && now.saturating_sub(tick) <= DOUBLE_CLICK_TICKS => {
                self.last = None;
                true
            }
            _ => {
                self.last = Some((now, target));
                false
            }
        }
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

// --- Who uses the mouse ---

/// What a task asks for with `grab`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Use {
    None = 0,
    /// Mouse events while it is in front, no pointer (`mouse -t`).
    Events = 1,
    /// Events and the pointer (paint).
    Pointer = 2,
}

static USES: [AtomicU8; MAX_TASKS] = [const { AtomicU8::new(0) }; MAX_TASKS];
/// Per task: text cells (row0, col0, row1, col1; exclusive ends, packed as u16s) where the text
/// mode pointer is not drawn (paint's canvas, whose cursor follows the mouse instead). 0 = none.
static NO_HIGHLIGHT: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

fn use_of(slot: usize) -> Use {
    match USES.get(slot).map_or(0, |u| u.load(Ordering::Relaxed)) {
        1 => Use::Events,
        2 => Use::Pointer,
        _ => Use::None,
    }
}

/// While this lives, the current task gets mouse events when it is in front (and, with
/// `Use::Pointer`, the pointer). Dropping it gives them up. `task::spawn` also clears a slot's
/// use, for a task that was killed while it held one.
pub struct Grab {
    slot: usize,
}

pub fn grab(what: Use) -> Grab {
    let slot = crate::task::current();
    USES[slot].store(what as u8, Ordering::Relaxed);
    refresh();
    Grab { slot }
}

impl Drop for Grab {
    fn drop(&mut self) {
        forget(self.slot);
        refresh();
    }
}

impl Grab {
    /// Text mode: no pointer highlight over these cells (`None`: everywhere again).
    pub fn set_no_highlight(&self, cells: Option<(usize, usize, usize, usize)>) {
        let packed = cells.map_or(0, |(r0, c0, r1, c1)| {
            (r0 as u64 & 0xFFFF) | (c0 as u64 & 0xFFFF) << 16 | (r1 as u64 & 0xFFFF) << 32 | (c1 as u64 & 0xFFFF) << 48
        });
        NO_HIGHLIGHT[self.slot].store(packed, Ordering::Relaxed);
    }
}

/// Task `slot` uses the mouse no more.
pub fn forget(slot: usize) {
    if let Some(u) = USES.get(slot) {
        u.store(0, Ordering::Relaxed);
        NO_HIGHLIGHT[slot].store(0, Ordering::Relaxed);
    }
}

fn highlight_allowed(slot: usize, (row, col): (usize, usize)) -> bool {
    let packed = NO_HIGHLIGHT.get(slot).map_or(0, |p| p.load(Ordering::Relaxed));
    if packed == 0 {
        return true;
    }
    let part = |shift: u32| (packed >> shift & 0xFFFF) as usize;
    !(row >= part(0) && row < part(32) && col >= part(16) && col < part(48))
}

// --- Delivering events and showing the pointer ---

struct State {
    tracker: Tracker,
    /// The task a press went to: it gets everything until the last button is let go.
    capture: Option<usize>,
    /// The mouse was used since the last key press: the pointer may show.
    shown: bool,
    generation: u64,
}

static STATE: SpinLock<State> =
    SpinLock::new(State { tracker: Tracker::new(), capture: None, shown: false, generation: u64::MAX });

/// What `poll` needs to know about the screen, read before taking the state lock.
struct Context {
    desktop: Option<usize>,
    foreground: usize,
    bar_row: usize,
    cell_px: (usize, usize),
    area: (usize, usize),
    generation: u64,
}

impl Context {
    fn now() -> Context {
        Context {
            desktop: crate::desktop::manager(),
            foreground: crate::task::foreground(),
            bar_row: crate::console::bar_row(),
            cell_px: display::cell_px(),
            area: display::area_px(),
            generation: display::generation(),
        }
    }

    /// Who gets an event at `cell` when no press is captured.
    fn target(&self, cell: (usize, usize)) -> Option<usize> {
        let fg = self.foreground;
        let user = (use_of(fg) != Use::None).then_some(fg);
        match self.desktop {
            Some(manager) if cell.0 == self.bar_row || fg == manager => Some(manager),
            _ => user,
        }
    }
}

/// Called by `input::pump`: turns waiting packets into events and hands each one to `deliver`
/// with the task slot it goes to. Then updates the pointer.
pub fn poll(mut deliver: impl FnMut(usize, Event)) {
    if !has_packet() && !working() {
        return;
    }
    let ctx = Context::now();
    // Up to 8 packets per call; the rest wait for the next pump.
    let mut pending: [Option<(usize, Event)>; 40] = [None; 40];
    let mut n = 0;
    {
        let mut state = STATE.lock();
        if state.generation != ctx.generation {
            state.generation = ctx.generation;
            state.tracker.clamp(ctx.area);
        }
        for _ in 0..8 {
            let Some(bytes) = pop_packet() else { break };
            if !working() {
                continue;
            }
            let mut events = [Event { action: Action::Move, x: 0, y: 0, dx: 0, dy: 0, buttons: 0 }; 5];
            let count = state.tracker.apply(Packet::parse(bytes), ctx.area, &mut events);
            for event in &events[..count] {
                state.shown = true;
                let target = match state.capture {
                    Some(slot) => Some(slot),
                    None => {
                        let target = ctx.target(event.cell(ctx.cell_px));
                        if matches!(event.action, Action::Down(_)) {
                            state.capture = target;
                        }
                        target
                    }
                };
                if event.buttons == 0 {
                    state.capture = None;
                }
                if let Some(slot) = target {
                    if n < pending.len() {
                        pending[n] = Some((slot, *event));
                        n += 1;
                    }
                }
            }
        }
    }
    for (slot, event) in pending.into_iter().take(n).flatten() {
        deliver(slot, event);
    }
    refresh();
}

/// A key was pressed: the pointer hides until the mouse is used again.
pub fn key_pressed() {
    let changed = {
        let mut state = STATE.lock();
        core::mem::replace(&mut state.shown, false)
    };
    if changed {
        refresh();
    }
}

/// The pointer's position (area pixels) and the buttons held.
pub fn position() -> (i32, i32, u8) {
    let state = STATE.lock();
    (state.tracker.x, state.tracker.y, state.tracker.buttons)
}

/// Shows, moves or hides the pointer to match the state: only with a working mouse that was
/// used since the last key, and only on the desktop or for a task in front that grabbed it.
pub fn refresh() {
    let desktop = crate::desktop::manager().is_some();
    let foreground = crate::task::foreground();
    let allowed = working() && (desktop || use_of(foreground) == Use::Pointer);
    let fallback = theme::attr(Item::DesktopSelected);
    let cell_px = display::cell_px();
    let want = {
        let state = STATE.lock();
        (allowed && state.shown).then_some((state.tracker.x.max(0) as usize, state.tracker.y.max(0) as usize))
    };
    let highlight = want.is_some_and(|(x, y)| highlight_allowed(foreground, (y / cell_px.1, x / cell_px.0)));
    display::set_pointer(want, highlight, fallback);
}
