//! The desktop: a home screen of icons, a taskbar with a clock, and switching between tasks.
//!
//! `desktop` (a shell command) turns the task that runs it into the desktop's manager. The
//! manager owns the screen: it shows the home screen (its own screen) or the screen of the
//! task in front, draws the taskbar on row 24, and is the only task that reads the keyboard and
//! serial input (`input::pump`), handing keys to the task in front. That way Alt+Tab works
//! even while the task in front is busy.
//!
//! Every terminal, editor and program opened from the desktop is a task of its own (see
//! `task.rs`). Ctrl-Q on the home screen leaves the desktop, closing them all.

pub mod home;

use crate::console::{self, Stdout};
use crate::input::{self, Key};
use crate::shell::{self, CmdError, Shell};
use crate::sync::SpinLock;
use crate::vga::{attr, Color, WIDTH};
use crate::{clock, fs, pit, task};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use home::{Action, Home, DESKTOP_DIR};
use kfs::FileType;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The manager's task slot.
static MANAGER: AtomicUsize = AtomicUsize::new(task::BOOT);
static SWITCHER: SpinLock<Switcher> = SpinLock::new(Switcher::new());

const BAR: u8 = attr(Color::Black, Color::LightGray);
const BAR_ACTIVE: u8 = attr(Color::White, Color::Blue);
const BAR_CLOCK: u8 = attr(Color::Black, Color::Cyan);
const OVERLAY: u8 = attr(Color::Black, Color::LightGray);
const OVERLAY_SELECTED: u8 = attr(Color::White, Color::Blue);
/// How long leaving the desktop waits for tasks to close before killing them.
const CLOSE_WAIT_MS: u64 = 2000;

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

pub fn is_manager(slot: usize) -> bool {
    is_active() && MANAGER.load(Ordering::Relaxed) == slot
}

/// The order of tasks for Alt+Tab, and the switcher's state. Pure logic, so the self-tests can
/// drive it.
pub struct Switcher {
    /// Task slots, most recently shown first; the home screen (the manager) is one of them.
    pub mru: Vec<usize>,
    /// While the switcher is open: the highlighted entry of `mru`.
    pub open: Option<usize>,
    /// The switcher appeared, moved or went away: it needs drawing.
    pub changed: bool,
    /// Bring this task to the front.
    pub switch_to: Option<usize>,
}

impl Switcher {
    pub const fn new() -> Switcher {
        Switcher { mru: Vec::new(), open: None, changed: false, switch_to: None }
    }

    /// Handles a key if it is the switcher's. `from_keyboard`: PS/2, where Alt can be held and
    /// the switch happens when it is let go; serial `ESC TAB` switches at once.
    pub fn key(&mut self, key: Key, from_keyboard: bool, home: usize) -> bool {
        let len = self.mru.len();
        if let Some(at) = self.open {
            match key {
                Key::Alt(b'\t') | Key::Char(b'\t') | Key::Right | Key::Down => self.open = Some((at + 1) % len.max(1)),
                Key::Left | Key::Up => self.open = Some((at + len.max(1) - 1) % len.max(1)),
                Key::Enter => self.commit(),
                Key::Escape => self.open = None,
                _ => {}
            }
            self.changed = true;
            return true;
        }
        match key {
            Key::Alt(b'\t') if len > 1 => {
                if from_keyboard {
                    self.open = Some(1);
                    self.changed = true;
                } else {
                    self.switch_to = Some(self.mru[1]);
                }
                true
            }
            Key::Alt(b'\t') => true,
            Key::Alt(b'h' | b'H') => {
                self.switch_to = Some(home);
                true
            }
            _ => false,
        }
    }

    /// Alt was let go: switch to the highlighted task.
    pub fn alt_up(&mut self) {
        if self.open.is_some() {
            self.commit();
            self.changed = true;
        }
    }

    fn commit(&mut self) {
        if let Some(at) = self.open.take() {
            self.switch_to = self.mru.get(at).copied();
        }
    }

    /// `slot` was brought to the front.
    pub fn shown(&mut self, slot: usize) {
        self.mru.retain(|&s| s != slot);
        self.mru.insert(0, slot);
    }

    pub fn forget(&mut self, slot: usize) {
        self.mru.retain(|&s| s != slot);
        if self.open.is_some_and(|at| at >= self.mru.len()) {
            self.open = None;
            self.changed = true;
        }
    }
}

/// Called by `input::pump` for every key: takes the desktop's hotkeys.
pub fn intercept(key: Key, from_keyboard: bool) -> bool {
    is_active() && SWITCHER.lock().key(key, from_keyboard, MANAGER.load(Ordering::Relaxed))
}

/// Called by `input::pump` when Alt is let go on the PS/2 keyboard.
pub fn alt_released() {
    if is_active() {
        SWITCHER.lock().alt_up();
    }
}

/// Asks the desktop to bring task `slot` to the front.
pub fn request_switch(slot: usize) {
    SWITCHER.lock().switch_to = Some(slot);
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(path)
}

/// Opens a new terminal task, starting in `cwd`, and brings it to the front.
pub fn open_terminal(cwd: &str) -> Result<usize, CmdError> {
    let cwd = String::from(cwd);
    let slot = task::spawn("Terminal", move || shell::terminal(&cwd))?;
    request_switch(slot);
    Ok(slot)
}

/// Opens a file in a new task and brings it to the front: programs and scripts run (with
/// `args`, in `cwd`), other files open in the editor (the hex editor for binary types). With
/// `edit`, runnable files are edited too.
pub fn open(path: &str, args: &[&str], cwd: &str, edit: bool) -> Result<usize, CmdError> {
    let info = fs::with(|fs| fs.stat(path)).map_err(|e| CmdError::Message(alloc::format!("{path}: {e}")))?;
    if info.is_dir {
        return Err(CmdError::Message(alloc::format!("{path} is a folder")));
    }
    let kind = FileType::from_name(path);
    let name = file_name(path);
    let path = String::from(path);
    let slot = if kind.is_runnable() && !edit {
        let args: Vec<String> = args.iter().map(|&a| String::from(a)).collect();
        let cwd = String::from(cwd);
        task::spawn(name, move || run_program(&path, &args, &cwd))?
    } else {
        task::spawn(name, move || edit_file(&path, kind.is_text()))?
    };
    request_switch(slot);
    Ok(slot)
}

fn run_program(path: &str, args: &[String], cwd: &str) {
    let mut shell = Shell::with_cwd(cwd);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(error) = shell.run_file(path, &args, &mut Stdout) {
        shell::report(error);
    }
    wait_before_closing();
}

fn edit_file(path: &str, text: bool) {
    let result = if text {
        crate::editor::Editor::open(path).map(|e| e.run())
    } else {
        crate::hexedit::HexEditor::open(path).map(|e| e.run())
    };
    if let Err(error) = result {
        crate::println!("error: {path}: {error}");
        wait_before_closing();
    }
}

/// Keeps a finished program's output on the screen until a key is pressed.
fn wait_before_closing() {
    let attr = console::attr();
    console::set_color(Color::Black, Color::LightGray);
    crate::print!("\n[finished: press any key to close]");
    console::set_attr(attr);
    let _ = input::read_key();
}

/// Runs the desktop until the user leaves it (Ctrl-Q on the home screen). The home screen
/// starts at `/desktop`.
pub fn run() -> Result<(), CmdError> {
    if is_active() {
        return Err("the desktop is already running".into());
    }
    let me = task::current();
    match fs::with(|fs| fs.mkdir(DESKTOP_DIR)) {
        Ok(()) | Err(fs::FsError::Fs(kfs::Error::AlreadyExists)) => {}
        Err(e) => return Err(CmdError::Message(alloc::format!("{DESKTOP_DIR}: {e}"))),
    }
    let saved = console::snapshot(me);
    {
        let mut switcher = SWITCHER.lock();
        *switcher = Switcher::new();
        switcher.mru.push(me);
    }
    MANAGER.store(me, Ordering::Relaxed);
    task::set_foreground(me);
    task::rename(me, "desktop");
    input::clear_queue(me);
    ACTIVE.store(true, Ordering::Release);
    console::set_desktop(true);
    console::set_cursor_visible(false);
    console::clear();

    let mut manager = Manager { me, home: Home::new(), bar_due: 0, quit: false };
    manager.home.load(DESKTOP_DIR);
    while !manager.quit {
        manager.step();
        task::wait_input();
    }
    manager.close_all();

    ACTIVE.store(false, Ordering::Release);
    task::set_foreground(me);
    task::rename(me, "shell");
    input::clear_queue(me);
    console::set_desktop(false);
    console::set_cursor_visible(true);
    match saved {
        Some(screen) => console::restore(me, &screen),
        None => console::clear(),
    }
    Ok(())
}

struct Manager {
    me: usize,
    home: Home,
    /// When the taskbar is next redrawn (the clock), in ticks.
    bar_due: u64,
    quit: bool,
}

impl Manager {
    fn step(&mut self) {
        // Reading keys also pumps all input: hotkeys first, the rest to the task in front.
        while let Some(key) = input::try_read_key() {
            if task::foreground() == self.me {
                self.home_key(key);
            }
        }
        self.follow_tasks();
        self.switcher();
        // Free ended tasks only now, after switching away from one that was in front.
        task::reap();
        if self.home.dirty {
            self.home.render();
        }
        if pit::ticks() >= self.bar_due {
            self.draw_bar();
        }
    }

    fn home_key(&mut self, key: Key) {
        let result = match self.home.handle(key) {
            Action::None => Ok(()),
            Action::Enter(path) => {
                self.home.load(&path);
                Ok(())
            }
            Action::Open(path) => open(&path, &[], &self.home.cwd.clone(), false).map(drop),
            Action::Edit(path) => open(&path, &[], &self.home.cwd.clone(), true).map(drop),
            Action::NewTerminal => open_terminal(&self.home.cwd.clone()).map(drop),
            Action::Quit => {
                let open = self.others().len();
                if open == 0 {
                    self.quit = true;
                } else {
                    self.home.ask_quit(open);
                }
                Ok(())
            }
            Action::QuitNow => {
                self.quit = true;
                Ok(())
            }
        };
        if let Err(CmdError::Message(message)) = result {
            self.home.message = message;
            self.home.dirty = true;
        }
    }

    /// The tasks other than the manager.
    fn others(&self) -> Vec<task::Info> {
        task::list().into_iter().filter(|t| t.slot != self.me).collect()
    }

    /// Notices tasks that started or ended.
    fn follow_tasks(&mut self) {
        let alive: Vec<usize> = task::list().iter().filter(|t| t.state != task::State::Dead).map(|t| t.slot).collect();
        let foreground = task::foreground();
        let mut changed = false;
        {
            let mut switcher = SWITCHER.lock();
            let gone: Vec<usize> = switcher.mru.iter().copied().filter(|s| !alive.contains(s)).collect();
            for slot in gone {
                switcher.forget(slot);
                changed = true;
            }
            for &slot in &alive {
                if !switcher.mru.contains(&slot) {
                    // New tasks go right after the one in front.
                    let at = 1.min(switcher.mru.len());
                    switcher.mru.insert(at, slot);
                    changed = true;
                }
            }
            if !alive.contains(&foreground) && switcher.switch_to.is_none() {
                switcher.switch_to = switcher.mru.first().copied();
            }
        }
        if changed {
            self.bar_due = 0;
        }
    }

    /// Draws or removes the Alt+Tab switcher, and brings a task to the front when asked.
    fn switcher(&mut self) {
        let (changed, open, mru, switch_to) = {
            let mut s = SWITCHER.lock();
            let changed = core::mem::take(&mut s.changed);
            (changed, s.open, s.mru.clone(), s.switch_to.take())
        };
        if let Some(slot) = switch_to {
            self.bring_to_front(slot);
        } else if changed {
            match open {
                Some(at) => self.draw_switcher(&mru, at),
                None => console::repaint(),
            }
        }
    }

    fn bring_to_front(&mut self, slot: usize) {
        let slot = if slot == self.me || task::is_alive(slot) { slot } else { self.me };
        SWITCHER.lock().shown(slot);
        task::set_foreground(slot);
        console::show(slot);
        self.bar_due = 0;
        if slot == self.me {
            // Keys typed for a task that went away should not land on the home screen.
            input::clear_queue(self.me);
        }
    }

    fn label(&self, slot: usize) -> String {
        if slot == self.me {
            String::from("Home")
        } else {
            alloc::format!("{} {}", task::id_of(slot), task::name_of(slot))
        }
    }

    fn draw_switcher(&self, mru: &[usize], at: usize) {
        const BOX_WIDTH: usize = 40;
        let inner = BOX_WIDTH - 2;
        let left = (WIDTH - BOX_WIDTH) / 2;
        let top = (console::BAR_ROW.saturating_sub(mru.len() + 4)) / 2;
        let line = |text: &str, width: usize| -> Vec<u8> {
            let mut bytes: Vec<u8> = text.bytes().take(width).collect();
            bytes.resize(width, b' ');
            bytes
        };
        let mut border = alloc::vec![0xC4u8; inner];
        let title = b" Switch to ";
        border[(inner - title.len()) / 2..][..title.len()].copy_from_slice(title);
        console::overlay(top, left, &[(&[0xDA], OVERLAY), (&border, OVERLAY), (&[0xBF], OVERLAY)]);
        for (i, &slot) in mru.iter().enumerate() {
            let text = line(&alloc::format!(" {} {}", if i == at { '>' } else { ' ' }, self.label(slot)), inner);
            let attr = if i == at { OVERLAY_SELECTED } else { OVERLAY };
            console::overlay(top + 1 + i, left, &[(&[0xB3], OVERLAY), (&text, attr), (&[0xB3], OVERLAY)]);
        }
        let hint = line(" Tab: next   let go of Alt: switch", inner);
        let row = top + 1 + mru.len();
        console::overlay(row, left, &[(&[0xB3], OVERLAY), (&hint, OVERLAY), (&[0xB3], OVERLAY)]);
        let bottom = alloc::vec![0xC4u8; inner];
        console::overlay(row + 1, left, &[(&[0xC0], OVERLAY), (&bottom, OVERLAY), (&[0xD9], OVERLAY)]);
    }

    /// The taskbar: Home and every task (the one in front highlighted), and the clock.
    fn draw_bar(&mut self) {
        // Redraw twice a second, so the minute changes on time.
        self.bar_due = pit::ticks() + pit::TICK_HZ / 2;
        let now = clock::now();
        let time = alloc::format!(" {} {:02} {} {}  {:02}:{:02} ", now.weekday_name(), now.day, now.month_name(), now.year, now.hour, now.minute);
        let foreground = task::foreground();
        let mut items: Vec<(String, u8)> = Vec::new();
        let mut slots = alloc::vec![self.me];
        slots.extend(self.others().iter().filter(|t| t.state != task::State::Dead).map(|t| t.slot));
        let room = WIDTH - time.len();
        for slot in slots {
            let mut label = self.label(slot);
            label.truncate(16);
            let attr = if slot == foreground { BAR_ACTIVE } else { BAR };
            items.push((alloc::format!(" {label} "), attr));
        }
        let mut used = 0;
        let mut spans: Vec<(&[u8], u8)> = Vec::new();
        for (i, (text, attr)) in items.iter().enumerate() {
            let more = items.len() - i;
            if used + text.len() + 4 > room && more > 1 {
                spans.push((b" ...", BAR));
                used += 4;
                break;
            }
            spans.push((text.as_bytes(), *attr));
            used += text.len();
        }
        let gap = alloc::vec![b' '; room.saturating_sub(used)];
        spans.push((&gap, BAR));
        spans.push((time.as_bytes(), BAR_CLOCK));
        console::draw_bar(&spans, BAR);
    }

    /// Asks every task to close, waits a moment, and kills those that did not.
    fn close_all(&mut self) {
        self.bring_to_front(self.me);
        for t in self.others() {
            task::close(t.slot);
        }
        let start = pit::uptime_ms();
        while pit::uptime_ms() - start < CLOSE_WAIT_MS {
            task::reap();
            if self.others().is_empty() {
                break;
            }
            pit::sleep_ms(10);
        }
        for t in self.others() {
            task::kill(t.slot);
        }
        task::reap();
    }
}
