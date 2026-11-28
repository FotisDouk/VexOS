//! The desktop: a home screen of icons, a taskbar with a clock, and switching between tasks.
//!
//! `desktop` (a shell command) turns the task that runs it into the desktop's manager. The
//! manager owns the screen: it shows the home screen (its own screen) or the screen of the
//! task in front, draws the taskbar on the last row, and is the only task that reads the
//! keyboard and serial input (`input::pump`), handing keys to the task in front. That way
//! Alt+Tab works even while the task in front is busy.
//!
//! Every terminal, editor and program opened from the desktop is a task of its own (see
//! `task.rs`). Ctrl-Q on the home screen leaves the desktop, closing them all. The colors come
//! from the theme (`theme.rs`), and everything is drawn again when the display mode or the
//! theme changes.

pub mod home;
pub mod layout;

use crate::console::{self, Stdout};
use crate::input::{self, Key};
use crate::shell::{self, CmdError, Shell};
use crate::sync::SpinLock;
use crate::theme::{self, Item};
use crate::{clock, display, fs, mouse, pit, task};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use home::{Action, Home, DESKTOP_DIR};
use kfs::FileType;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The manager's task slot.
static MANAGER: AtomicUsize = AtomicUsize::new(task::BOOT);
static SWITCHER: SpinLock<Switcher> = SpinLock::new(Switcher::new());

/// How long leaving the desktop waits for tasks to close before killing them.
const CLOSE_WAIT_MS: u64 = 2000;

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

pub fn is_manager(slot: usize) -> bool {
    is_active() && MANAGER.load(Ordering::Relaxed) == slot
}

/// The manager's task slot while the desktop runs.
pub fn manager() -> Option<usize> {
    is_active().then(|| MANAGER.load(Ordering::Relaxed))
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

/// The columns `start..end` of one taskbar entry (its text with the two padding spaces) and the
/// task it stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BarZone {
    pub start: usize,
    pub end: usize,
    pub slot: usize,
}

/// Where the taskbar puts its entries: what `draw_bar` draws and what a click hits.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BarLayout {
    /// The entries that fit, left to right.
    pub zones: Vec<BarZone>,
    /// The column of the ` ...` that stands for the entries that did not fit (no zone).
    pub overflow: Option<usize>,
    /// Columns used by the entries and the overflow; the clock goes at `room` or here, whichever
    /// is further right.
    pub used: usize,
}

/// Lays out the taskbar's entries, each `(task slot, text width)`, in `room` columns (the width
/// minus the clock). An entry that would leave no room for ` ...` is replaced by it, unless it is
/// the last one. Pure, so the self-tests can drive it.
pub fn bar_layout(entries: &[(usize, usize)], room: usize) -> BarLayout {
    const OVERFLOW_WIDTH: usize = 4; // " ..."
    let mut layout = BarLayout { zones: Vec::new(), overflow: None, used: 0 };
    for (i, &(slot, width)) in entries.iter().enumerate() {
        let more = entries.len() - i;
        if layout.used + width + OVERFLOW_WIDTH > room && more > 1 {
            layout.overflow = Some(layout.used);
            layout.used += OVERFLOW_WIDTH;
            break;
        }
        layout.zones.push(BarZone { start: layout.used, end: layout.used + width, slot });
        layout.used += width;
    }
    layout
}

/// The task whose taskbar entry covers column `col`. The overflow, the gap and the clock have
/// none. Pure.
pub fn bar_slot_at(zones: &[BarZone], col: usize) -> Option<usize> {
    zones.iter().find(|z| col >= z.start && col < z.end).map(|z| z.slot)
}

/// What a left click on the taskbar does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarAction {
    /// The click hit nothing, or the task it hit is already in front.
    Nothing,
    /// Bring this task to the front.
    Switch(usize),
    /// Go to the home screen at `/desktop`: the manager shows that folder (a sub-folder that was
    /// open is left) and comes to the front.
    Home,
}

/// What a left click at column `col` of the taskbar does, with task `foreground` in front, the
/// manager in slot `home` and the home screen showing folder `folder`. Home always ends on
/// `/desktop`, like Esc does; clicking it on `/desktop` itself with the home screen in front does
/// nothing. Pure.
pub fn bar_action(zones: &[BarZone], col: usize, foreground: usize, home: usize, folder: &str) -> BarAction {
    match bar_slot_at(zones, col) {
        Some(slot) if slot == home && folder != DESKTOP_DIR => BarAction::Home,
        Some(slot) if slot != foreground => BarAction::Switch(slot),
        _ => BarAction::Nothing,
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

/// The taskbar's clock as the theme asks for it, e.g. " Wed 25 Nov 2026  14:30 " or
/// " 2:30:05 PM ".
pub fn clock_text(look: &theme::Theme, now: crate::rtc::DateTime) -> String {
    let mut text = String::from(" ");
    if look.clock_date {
        text.push_str(&alloc::format!("{} {:02} {} {}  ", now.weekday_name(), now.day, now.month_name(), now.year));
    }
    if look.clock_12h {
        let hour = match now.hour % 12 {
            0 => 12,
            h => h,
        };
        text.push_str(&alloc::format!("{hour}:{:02}", now.minute));
    } else {
        text.push_str(&alloc::format!("{:02}:{:02}", now.hour, now.minute));
    }
    if look.clock_seconds {
        text.push_str(&alloc::format!(":{:02}", now.second));
    }
    if look.clock_12h {
        text.push_str(if now.hour < 12 { " AM" } else { " PM" });
    }
    text.push(' ');
    text
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

/// Opens paint in a new task and brings it to the front: on `file` (a picture), or a blank
/// canvas. Names typed in paint are relative to `cwd`.
pub fn open_paint(file: Option<&str>, cwd: &str) -> Result<usize, CmdError> {
    let file = file.map(String::from);
    let cwd = String::from(cwd);
    let slot = task::spawn("paint", move || paint_task(file.as_deref(), &cwd))?;
    request_switch(slot);
    Ok(slot)
}

fn paint_task(file: Option<&str>, cwd: &str) {
    match crate::paint::Paint::open(cwd, file, None) {
        Ok(paint) => paint.run(),
        Err(error) => {
            crate::println!("error: paint: {error}");
            wait_before_closing();
        }
    }
}

/// Opens a file in a new task and brings it to the front: programs and scripts run (with
/// `args`, in `cwd`), pictures open in the viewer, other files open in the editor (the hex editor
/// for binary types). With `edit`, runnable files and pictures are edited too.
pub fn open(path: &str, args: &[&str], cwd: &str, edit: bool) -> Result<usize, CmdError> {
    let info = fs::with(|fs| fs.stat(path)).map_err(|e| CmdError::Message(alloc::format!("{path}: {e}")))?;
    if info.is_dir {
        return Err(CmdError::Message(alloc::format!("{path} is a folder")));
    }
    let kind = FileType::from_name(path);
    let name = file_name(path);
    let path = String::from(path);
    let slot = if kind == FileType::Image && !edit {
        // Real pixels in a graphics mode, characters in text mode.
        let pixels = display::is_graphics();
        task::spawn(name, move || view_image(&path, pixels))?
    } else if kind.is_runnable() && !edit {
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

fn view_image(path: &str, pixels: bool) {
    match crate::viewer::Viewer::open(path, pixels, true) {
        Ok(viewer) => viewer.run(),
        Err(error) => {
            crate::println!("error: {path}: {error}");
            wait_before_closing();
        }
    }
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
    console::set_attr(theme::attr(Item::Taskbar));
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

    let mut manager = Manager {
        me,
        home: Home::new(),
        bar_due: 0,
        bar_zones: Vec::new(),
        press: None,
        quit: false,
        generation: u64::MAX,
    };
    manager.follow_display();
    manager.home.load(DESKTOP_DIR);
    while !manager.quit {
        manager.step();
        task::wait_input();
    }
    manager.close_all();

    ACTIVE.store(false, Ordering::Release);
    task::set_foreground(me);
    // The pointer belongs to the desktop: it goes with it.
    crate::mouse::refresh();
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
    /// The taskbar entries as last drawn, for clicks.
    bar_zones: Vec<BarZone>,
    /// Where the mouse button that is down went (it gets the rest of the press).
    press: Option<Press>,
    quit: bool,
    /// `display::generation` when everything was last drawn.
    generation: u64,
}

/// What a mouse press started on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Press {
    Bar,
    Home,
}

impl Manager {
    /// After a change of display mode or theme: fits the home screen to the new size and draws
    /// everything again.
    fn follow_display(&mut self) {
        let generation = display::generation();
        if generation == self.generation {
            return;
        }
        self.generation = generation;
        self.home.set_screen(console::cols(), console::rows());
        self.bar_due = 0;
    }

    fn step(&mut self) {
        // Reading keys also pumps all input: hotkeys first, the rest to the task in front.
        while let Some(key) = input::try_read_key() {
            match key {
                Key::Mouse(event) => self.mouse_event(event),
                _ if task::foreground() == self.me => self.home_key(key),
                _ => {}
            }
        }
        self.follow_display();
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

    /// A mouse event for the manager: the taskbar's, or the home screen's when it is in front.
    /// A press goes to what it started on, and so does everything until the buttons are up, so
    /// a drag that leaves the grid still ends on the home screen.
    fn mouse_event(&mut self, event: mouse::Event) {
        let (row, col) = event.cell(display::cell_px());
        let target = match self.press {
            Some(press) => press,
            None if row == console::bar_row() => Press::Bar,
            None => Press::Home,
        };
        if matches!(event.action, mouse::Action::Down(_)) && self.press.is_none() {
            self.press = Some(target);
        }
        if event.buttons == 0 {
            self.press = None;
        }
        match target {
            Press::Bar => {
                if event.action == mouse::Action::Down(mouse::Button::Left) {
                    self.click_bar(col);
                }
            }
            Press::Home => {
                // Only the home screen in front uses clicks (a press can only start here then).
                let starts = matches!(event.action, mouse::Action::Down(_));
                if task::foreground() == self.me || !starts {
                    let action = self.home.mouse(event, display::cell_px(), pit::ticks());
                    self.home_action(action);
                }
            }
        }
    }

    /// A left click on the taskbar: Home goes to the home screen at `/desktop` (also when it is
    /// already in front showing a folder), a task's entry brings that task to the front (the
    /// same path as Alt+Tab). The overflow, the gap and the clock do nothing.
    fn click_bar(&mut self, col: usize) {
        if SWITCHER.lock().open.is_some() {
            return; // the Alt+Tab list is open: Alt decides
        }
        match bar_action(&self.bar_zones, col, task::foreground(), self.me, &self.home.cwd) {
            BarAction::Nothing => {}
            BarAction::Switch(slot) => request_switch(slot),
            BarAction::Home => {
                self.home.load(DESKTOP_DIR);
                if task::foreground() != self.me {
                    request_switch(self.me);
                }
            }
        }
    }

    fn home_key(&mut self, key: Key) {
        let action = self.home.handle(key);
        self.home_action(action);
    }

    /// Does what the home screen asked for.
    fn home_action(&mut self, action: Action) {
        let result = match action {
            Action::None => Ok(()),
            Action::Enter(path) => {
                self.home.load(&path);
                Ok(())
            }
            Action::Open(path) => open(&path, &[], &self.home.cwd.clone(), false).map(drop),
            Action::Edit(path) => open(&path, &[], &self.home.cwd.clone(), true).map(drop),
            Action::NewTerminal => open_terminal(&self.home.cwd.clone()).map(drop),
            Action::Paint(file) => open_paint(file.as_deref(), &self.home.cwd.clone()).map(drop),
            Action::SaveLayout(places) => layout::save(&self.home.cwd, &places)
                .map_err(|e| CmdError::Message(alloc::format!("could not save the icon places: {e}"))),
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
        let overlay = theme::attr(Item::Switcher);
        let overlay_selected = theme::attr(Item::SwitcherSelected);
        let inner = BOX_WIDTH - 2;
        let left = console::cols().saturating_sub(BOX_WIDTH) / 2;
        let top = (console::bar_row().saturating_sub(mru.len() + 4)) / 2;
        let line = |text: &str, width: usize| -> Vec<u8> {
            let mut bytes: Vec<u8> = text.bytes().take(width).collect();
            bytes.resize(width, b' ');
            bytes
        };
        let mut border = alloc::vec![0xC4u8; inner];
        let title = b" Switch to ";
        border[(inner - title.len()) / 2..][..title.len()].copy_from_slice(title);
        console::overlay(top, left, &[(&[0xDA], overlay), (&border, overlay), (&[0xBF], overlay)]);
        for (i, &slot) in mru.iter().enumerate() {
            let text = line(&alloc::format!(" {} {}", if i == at { '>' } else { ' ' }, self.label(slot)), inner);
            let attr = if i == at { overlay_selected } else { overlay };
            console::overlay(top + 1 + i, left, &[(&[0xB3], overlay), (&text, attr), (&[0xB3], overlay)]);
        }
        let hint = line(" Tab: next   let go of Alt: switch", inner);
        let row = top + 1 + mru.len();
        console::overlay(row, left, &[(&[0xB3], overlay), (&hint, overlay), (&[0xB3], overlay)]);
        let bottom = alloc::vec![0xC4u8; inner];
        console::overlay(row + 1, left, &[(&[0xC0], overlay), (&bottom, overlay), (&[0xD9], overlay)]);
    }

    /// The taskbar: Home and every task (the one in front highlighted), and the clock.
    fn draw_bar(&mut self) {
        // Redraw twice a second, so the minute (or second) changes on time.
        self.bar_due = pit::ticks() + pit::TICK_HZ / 2;
        let look = theme::current();
        let (bar, bar_active, bar_clock) =
            (look.attr(Item::Taskbar), look.attr(Item::TaskbarActive), look.attr(Item::TaskbarClock));
        let time = clock_text(&look, clock::now());
        let foreground = task::foreground();
        let mut items: Vec<(String, u8)> = Vec::new();
        let mut slots = alloc::vec![self.me];
        slots.extend(self.others().iter().filter(|t| t.state != task::State::Dead).map(|t| t.slot));
        let room = console::cols().saturating_sub(time.len());
        for &slot in &slots {
            let mut label = self.label(slot);
            label.truncate(16);
            let attr = if slot == foreground { bar_active } else { bar };
            items.push((alloc::format!(" {label} "), attr));
        }
        // The same layout answers clicks (`bar_zones`), so the two never drift apart.
        let entries: Vec<(usize, usize)> = slots.iter().zip(&items).map(|(&slot, (text, _))| (slot, text.len())).collect();
        let layout = bar_layout(&entries, room);
        let mut spans: Vec<(&[u8], u8)> = Vec::new();
        for (i, (text, attr)) in items.iter().enumerate().take(layout.zones.len()) {
            debug_assert_eq!(layout.zones[i].end - layout.zones[i].start, text.len());
            spans.push((text.as_bytes(), *attr));
        }
        if layout.overflow.is_some() {
            spans.push((b" ...", bar));
        }
        let gap = alloc::vec![b' '; room.saturating_sub(layout.used)];
        self.bar_zones = layout.zones;
        spans.push((&gap, bar));
        spans.push((time.as_bytes(), bar_clock));
        console::draw_bar(&spans, bar);
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
