//! The home screen: a grid of icons for the files in a folder, starting with `/desktop`.
//!
//! Each icon sits in a cell of a grid as wide as the screen allows (16 text columns and 5 rows
//! per cell). At first the icons fill the grid in order; `M` moves one, and then the places of
//! the folder's icons are saved (`layout.rs`), so they stay where they were put. `A` puts them
//! back in order.
//!
//! `Home::handle` turns keys into actions without touching the disk or the screen, so the
//! self-tests can drive it; the desktop loads folders (`load`) and draws it (`render`).
//! `Home::mouse` does the same for mouse events: a click chooses an icon, a double-click opens
//! it, and pulling an icon 4 pixels or more picks it up like `M` does (`icon_at` and
//! `grid_cell_at` are the pure hit tests).

use super::layout::{self, Place};
use crate::console;
use crate::fs;
use crate::input::Key;
use crate::mouse::{self, Button, DoubleClick};
use crate::theme::{self, Item, Theme};
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use kfs::{FileType, Info};

/// The folder the home screen shows first.
pub const DESKTOP_DIR: &str = "/desktop";

/// Screen columns and rows of one grid cell.
const CELL_WIDTH: usize = 16;
const CELL_HEIGHT: usize = 5;
const HEADER_ROW: usize = 0;
const GRID_TOP: usize = 2;
/// Icons can be moved down to this grid row (counted from 0).
pub const MAX_GRID_ROW: usize = 99;

// Code page 437 box lines.
const TOP_LEFT: u8 = 0xDA;
const TOP_RIGHT: u8 = 0xBF;
const BOTTOM_LEFT: u8 = 0xC0;
const BOTTOM_RIGHT: u8 = 0xD9;
const HORIZONTAL: u8 = 0xC4;
const VERTICAL: u8 = 0xB3;
const DOUBLE_HORIZONTAL: u8 = 0xCD;
const DOUBLE_VERTICAL: u8 = 0xBA;
const DOUBLE_TOP_LEFT: u8 = 0xC9;
const DOUBLE_TOP_RIGHT: u8 = 0xBB;
const DOUBLE_BOTTOM_LEFT: u8 = 0xC8;
const DOUBLE_BOTTOM_RIGHT: u8 = 0xBC;

const HINT_WIDE: &[u8] =
    b" Enter open  E edit  P paint  T terminal  M move  A arrange  R reload  Bksp up  Esc desktop  Alt+Tab switch  ^Q leave";
const HINT: &[u8] = b" Enter open  E edit  P paint  T term  M move  A arrange  Bksp up  ^Q leave";
const HINT_MOVING: &[u8] = b" Arrows: move the icon   Enter: put it down here   Esc: put it back";
/// Shown after a click on an icon, until the next key or click on an icon.
const CLICK_MESSAGE: &str = "Double-click opens it, drag moves it";
/// The pointer must move this far (area pixels, straight line) from where the button went down
/// before an icon is picked up.
const DRAG_PIXELS: i32 = 4;

/// The left button held down on an icon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Drag {
    icon: usize,
    /// Where the button went down, in area pixels.
    from: (u16, u16),
    /// The icon was picked up (`moving` is set).
    picked: bool,
    /// Esc (or a key) ended it: the rest of the press does nothing.
    over: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Opens a new terminal.
    Terminal,
    /// Shows the whole disk (`/`).
    Files,
    /// Opens paint on a blank canvas.
    Paint,
    /// The parent folder.
    Up,
    File(FileType),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Icon {
    pub name: String,
    pub kind: Kind,
}

impl Icon {
    fn new(name: &str, kind: Kind) -> Icon {
        Icon { name: String::from(name), kind }
    }

    /// The label inside the icon's box, and the theme item that colors it.
    fn tag(&self) -> (&'static str, Item) {
        match self.kind {
            Kind::Terminal => ("TERM", Item::IconTerminal),
            Kind::Files => ("DISK", Item::IconFiles),
            Kind::Paint => ("PAINT", Item::IconPaint),
            Kind::Up => ("UP", Item::IconUp),
            Kind::File(t) => match t {
                FileType::Dir => ("DIR", Item::IconDir),
                FileType::Text => ("TXT", Item::IconText),
                FileType::Asm => ("ASM", Item::IconAsm),
                FileType::Exe => ("VEX", Item::IconVex),
                FileType::Script => ("V", Item::IconScript),
                FileType::Kernel => ("SYS", Item::IconSys),
                FileType::Boot => ("BOOT", Item::IconBoot),
                FileType::Raw => ("RAW", Item::IconRaw),
                FileType::Image => ("IMG", Item::IconImage),
            },
        }
    }

    /// How the layout file names this icon: `"name"`, or `:terminal`, `:files`, `:paint`, `:up`.
    pub fn id(&self) -> String {
        match self.kind {
            Kind::Terminal => String::from(":terminal"),
            Kind::Files => String::from(":files"),
            Kind::Paint => String::from(":paint"),
            Kind::Up => String::from(":up"),
            Kind::File(_) => format!("\"{}\"", self.name),
        }
    }
}

/// What a key on the home screen asks the desktop to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    None,
    /// Show this folder.
    Enter(String),
    /// Open a file: run it if it is runnable, else edit it.
    Open(String),
    /// Open a file in the editor, even if it is runnable.
    Edit(String),
    NewTerminal,
    /// Open paint: on this picture, or on a blank canvas.
    Paint(Option<String>),
    /// Save these places for the folder shown (none: back to the automatic order).
    SaveLayout(Vec<Place>),
    /// Leave the desktop (the desktop asks first if programs are open).
    Quit,
    /// Leave the desktop, closing open programs: the answer to the question.
    QuitNow,
}

pub struct Home {
    pub cwd: String,
    icons: Vec<Icon>,
    /// The grid cell (column, row) of each icon.
    pos: Vec<(usize, usize)>,
    /// The places saved for this folder.
    saved: Vec<Place>,
    selected: usize,
    /// First grid row on the screen.
    top: usize,
    /// The screen's columns and rows, and the grid columns and rows that fit on it.
    width: usize,
    rows: usize,
    cols: usize,
    grid_rows: usize,
    /// While an icon is being moved: every icon's place before the move (Esc puts them back).
    moving: Option<Vec<(usize, usize)>>,
    /// The left button is down on an icon (a click that may become a drag).
    drag: Option<Drag>,
    /// Tells a double-click from two clicks.
    clicks: DoubleClick,
    pub message: String,
    /// Asking whether to close this many programs and leave.
    confirm_quit: Option<usize>,
    /// Needs drawing.
    pub dirty: bool,
    /// What each row shows, so only changed rows are drawn (this matters on serial).
    shown: Vec<Option<Vec<(Vec<u8>, u8)>>>,
}

/// The icons for a folder's entries: built-in ones first, then folders, then files, by name.
pub fn icons_for(cwd: &str, mut entries: Vec<Info>) -> Vec<Icon> {
    let mut icons = Vec::new();
    if cwd == DESKTOP_DIR {
        icons.push(Icon::new("Terminal", Kind::Terminal));
        icons.push(Icon::new("Files", Kind::Files));
        icons.push(Icon::new("Paint", Kind::Paint));
    } else if cwd != "/" {
        icons.push(Icon::new("..", Kind::Up));
    }
    entries.sort_by(|x, y| y.is_dir.cmp(&x.is_dir).then_with(|| x.name.cmp(&y.name)));
    icons.extend(entries.iter().map(|e| Icon { name: e.name.clone(), kind: Kind::File(e.file_type()) }));
    icons
}

fn parent(path: &str) -> String {
    kfs::resolve(path, "..")
}

impl Home {
    /// A home screen for an 80x24 screen; the desktop calls `set_screen` with the real size.
    pub fn new() -> Home {
        let mut home = Home {
            cwd: String::from(DESKTOP_DIR),
            icons: Vec::new(),
            pos: Vec::new(),
            saved: Vec::new(),
            selected: 0,
            top: 0,
            width: 0,
            rows: 0,
            cols: 1,
            grid_rows: 1,
            moving: None,
            drag: None,
            clicks: DoubleClick::new(),
            message: String::new(),
            confirm_quit: None,
            dirty: true,
            shown: Vec::new(),
        };
        home.set_screen(80, 24);
        home
    }

    /// Fits the grid to a screen of `width` columns and `rows` rows: a header and a blank row
    /// above it, the message and key reminder rows below.
    pub fn set_screen(&mut self, width: usize, rows: usize) {
        self.width = width.max(CELL_WIDTH);
        self.rows = rows.max(GRID_TOP + CELL_HEIGHT + 2);
        self.cols = (self.width / CELL_WIDTH).max(1);
        self.grid_rows = ((self.rows - GRID_TOP - 2) / CELL_HEIGHT).max(1);
        self.shown = vec![None; self.rows];
        self.arrange();
        self.dirty = true;
    }

    /// Draws one row unless it already shows exactly this.
    fn draw(&mut self, row: usize, spans: Vec<(Vec<u8>, u8)>, pad: u8) {
        if row >= self.shown.len() || self.shown[row].as_ref() == Some(&spans) {
            return;
        }
        let parts: Vec<(&[u8], u8)> = spans.iter().map(|(t, a)| (t.as_slice(), *a)).collect();
        console::draw_spans(row, &parts, pad);
        self.shown[row] = Some(spans);
    }

    /// Shows `icons` as the contents of `cwd` (keeps the selection when it is the same folder).
    pub fn set_folder(&mut self, cwd: &str, icons: Vec<Icon>) {
        if self.cwd != cwd {
            self.selected = 0;
            self.top = 0;
            self.cwd = String::from(cwd);
            self.saved.clear();
        }
        self.icons = icons;
        self.moving = None;
        self.drag = None;
        self.clicks.reset();
        self.selected = self.selected.min(self.icons.len().saturating_sub(1));
        self.arrange();
        self.dirty = true;
    }

    /// The saved places of the folder's icons.
    pub fn set_places(&mut self, saved: Vec<Place>) {
        self.saved = saved;
        self.arrange();
        self.dirty = true;
    }

    /// Reads folder `path` and its icon places from the disk. On an error the current folder
    /// stays.
    pub fn load(&mut self, path: &str) {
        match fs::with(|fs| fs.read_dir(path)) {
            Ok(entries) => {
                let icons = icons_for(path, entries);
                self.set_folder(path, icons);
                self.set_places(layout::load(path));
            }
            Err(e) => self.message = format!("{path}: {e}"),
        }
        self.dirty = true;
    }

    /// Puts every icon in a cell: the saved place if it is free and fits on the screen, else
    /// the first free cell in reading order.
    fn arrange(&mut self) {
        let mut taken = BTreeSet::new();
        let mut pos: Vec<Option<(usize, usize)>> = vec![None; self.icons.len()];
        for (i, icon) in self.icons.iter().enumerate() {
            let id = icon.id();
            if let Some(place) = self.saved.iter().find(|p| p.id == id) {
                let cell = (place.col, place.row);
                if place.col < self.cols && place.row <= MAX_GRID_ROW && taken.insert(cell) {
                    pos[i] = Some(cell);
                }
            }
        }
        let mut next = 0;
        for p in pos.iter_mut().filter(|p| p.is_none()) {
            loop {
                let cell = (next % self.cols, next / self.cols);
                next += 1;
                if taken.insert(cell) {
                    *p = Some(cell);
                    break;
                }
            }
        }
        self.pos = pos.into_iter().flatten().collect();
    }

    pub fn selected(&self) -> Option<&Icon> {
        self.icons.get(self.selected)
    }

    pub fn icons(&self) -> &[Icon] {
        &self.icons
    }

    /// The grid cell of the icon called `name` (for the self-tests).
    pub fn place_of(&self, name: &str) -> Option<(usize, usize)> {
        let i = self.icons.iter().position(|icon| icon.name == name)?;
        self.pos.get(i).copied()
    }

    /// Every icon's place, as the layout file saves them.
    pub fn places(&self) -> Vec<Place> {
        self.icons.iter().zip(&self.pos).map(|(icon, &(col, row))| Place { id: icon.id(), col, row }).collect()
    }

    /// The icon in grid cell (`col`, `row`).
    fn icon_in_cell(&self, col: usize, row: usize) -> Option<usize> {
        self.pos.iter().position(|&p| p == (col, row))
    }

    /// The grid cell (column, row) under the text cell (`row`, `col`), if that is on the grid as
    /// it is shown: the header, the blank row under it, the message and hint rows and the
    /// columns right of the last grid column are not. Pure.
    pub fn grid_cell_at(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        let below_header = row.checked_sub(GRID_TOP)?;
        let (grid_col, grid_row) = (col / CELL_WIDTH, below_header / CELL_HEIGHT);
        (grid_col < self.cols && grid_row < self.grid_rows).then_some((grid_col, self.top + grid_row))
    }

    /// The icon (index into `icons`) whose grid cell, as `render` lays it out, holds the text
    /// cell (`row`, `col`): the box, the name and the blank space around them. `None` for an
    /// empty grid cell and for the header, hint and message rows. Pure.
    pub fn icon_at(&self, row: usize, col: usize) -> Option<usize> {
        let (grid_col, grid_row) = self.grid_cell_at(row, col)?;
        self.icon_in_cell(grid_col, grid_row)
    }

    /// The icons in reading order: by row, then by column.
    fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.icons.len()).collect();
        order.sort_by_key(|&i| (self.pos[i].1, self.pos[i].0));
        order
    }

    /// The previous (`-1`) or next (`1`) icon in reading order.
    fn step(&mut self, delta: isize) {
        let order = self.order();
        let Some(at) = order.iter().position(|&i| i == self.selected) else { return };
        if let Some(&next) = at.checked_add_signed(delta).and_then(|to| order.get(to)) {
            self.selected = next;
        }
    }

    /// The icon in the nearest row below (or above) that has icons, in the nearest column.
    fn vertical(&self, down: bool) -> Option<usize> {
        let &(col, row) = self.pos.get(self.selected)?;
        let rows = self.pos.iter().map(|p| p.1).filter(|&r| if down { r > row } else { r < row });
        let target = if down { rows.min() } else { rows.max() }?;
        (0..self.pos.len())
            .filter(|&i| self.pos[i].1 == target)
            .min_by_key(|&i| (self.pos[i].0.abs_diff(col), self.pos[i].0))
    }

    fn step_rows(&mut self, down: bool, count: usize) {
        for _ in 0..count {
            match self.vertical(down) {
                Some(i) => self.selected = i,
                None => break,
            }
        }
    }

    /// Asks "close N programs and leave?"; the next key answers.
    pub fn ask_quit(&mut self, open: usize) {
        self.confirm_quit = Some(open);
        self.message = format!(
            "Leave the desktop? {open} open program{} will be closed. (y/n)",
            if open == 1 { "" } else { "s" }
        );
        self.dirty = true;
    }

    fn path_of(&self, icon: &Icon) -> String {
        kfs::resolve(&self.cwd, &icon.name)
    }

    /// Applies one key.
    pub fn handle(&mut self, key: Key) -> Action {
        if matches!(key, Key::Mouse(_)) {
            return Action::None; // mouse events go to `mouse`
        }
        self.dirty = true;
        if let Some(drag) = self.drag {
            // A key while the button is down: Esc ends the press, putting everything back; a
            // picked-up icon takes no other key; a plain press just stops being a drag.
            if matches!(key, Key::Escape | Key::Close) {
                self.cancel_drag();
                return Action::None;
            }
            if drag.picked {
                return Action::None;
            }
            self.drag = Some(Drag { over: true, ..drag });
        }
        if self.confirm_quit.take().is_some() {
            self.message.clear();
            return match key {
                Key::Char(b'y' | b'Y') => Action::QuitNow,
                _ => Action::None,
            };
        }
        if self.moving.is_some() {
            return self.handle_move(key);
        }
        self.message.clear();
        match key {
            Key::Left => self.step(-1),
            Key::Right => self.step(1),
            Key::Up => self.step_rows(false, 1),
            Key::Down => self.step_rows(true, 1),
            Key::PageUp => self.step_rows(false, self.grid_rows),
            Key::PageDown => self.step_rows(true, self.grid_rows),
            Key::Home => self.selected = self.order().first().copied().unwrap_or(0),
            Key::End => self.selected = self.order().last().copied().unwrap_or(0),
            Key::Enter => return self.activate(),
            Key::Char(b'e' | b'E') => {
                return match self.selected() {
                    Some(icon) if matches!(icon.kind, Kind::File(t) if t != FileType::Dir) => {
                        Action::Edit(self.path_of(icon))
                    }
                    _ => Action::None,
                }
            }
            Key::Char(b'm' | b'M') => {
                if let Some(icon) = self.selected() {
                    self.message = format!("Moving {}", icon.name);
                    self.moving = Some(self.pos.clone());
                }
            }
            Key::Char(b'a' | b'A') => {
                self.saved.clear();
                self.arrange();
                self.message = String::from("Icons put back in order");
                return Action::SaveLayout(Vec::new());
            }
            Key::Char(b't' | b'T') => return Action::NewTerminal,
            Key::Char(b'p' | b'P') => return self.paint_key(),
            Key::Char(b'r' | b'R') => return Action::Enter(self.cwd.clone()),
            Key::Backspace if self.cwd == "/" => return Action::Enter(String::from(DESKTOP_DIR)),
            Key::Backspace => return Action::Enter(parent(&self.cwd)),
            Key::Escape => return Action::Enter(String::from(DESKTOP_DIR)),
            Key::Ctrl(b'q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    /// Keys while an icon is being moved.
    fn handle_move(&mut self, key: Key) -> Action {
        match key {
            Key::Left => self.shift(-1, 0),
            Key::Right => self.shift(1, 0),
            Key::Up => self.shift(0, -1),
            Key::Down => self.shift(0, 1),
            Key::Enter | Key::Char(b'm' | b'M') => {
                self.moving = None;
                self.message.clear();
                self.saved = self.places();
                return Action::SaveLayout(self.saved.clone());
            }
            Key::Escape | Key::Close => {
                if let Some(before) = self.moving.take() {
                    self.pos = before;
                }
                self.message.clear();
            }
            _ => {}
        }
        Action::None
    }

    /// Applies one mouse event, at tick `now`; `cell_px` is `display::cell_px()`. Only the left
    /// button does anything:
    /// - a press on an icon chooses it and shows `CLICK_MESSAGE`; a second press on it within
    ///   400 ms opens it (like Enter);
    /// - moving `DRAG_PIXELS` or more with the button down picks the icon up (like `M`); it
    ///   follows the pointer cell by cell, and letting go puts it down and saves the places;
    ///   letting go outside the grid, or Esc, puts everything back. A drag never scrolls;
    /// - a press on empty space, the header or the hint rows does nothing.
    pub fn mouse(&mut self, event: mouse::Event, cell_px: (usize, usize), now: u64) -> Action {
        if self.confirm_quit.is_some() {
            return Action::None;
        }
        let (row, col) = event.cell(cell_px);
        match event.action {
            mouse::Action::Down(Button::Left) => self.press(event, row, col, now),
            mouse::Action::Move => {
                self.drag_to(event, row, col);
                Action::None
            }
            mouse::Action::Up(Button::Left) => self.release(row, col),
            // The right and middle buttons do nothing, also during a drag.
            _ => Action::None,
        }
    }

    fn press(&mut self, event: mouse::Event, row: usize, col: usize, now: u64) -> Action {
        // A press that never ended (the screen was away meanwhile) is over.
        self.cancel_drag();
        self.drag = None;
        if self.moving.is_some() {
            return Action::None; // M is active: its keys answer it
        }
        self.follow_selection();
        let Some(icon) = self.icon_at(row, col) else {
            self.clicks.reset();
            return Action::None;
        };
        self.selected = icon;
        self.dirty = true;
        if self.clicks.press(icon, now) {
            self.message.clear();
            return self.activate();
        }
        self.message = String::from(CLICK_MESSAGE);
        self.drag = Some(Drag { icon, from: (event.x, event.y), picked: false, over: false });
        Action::None
    }

    /// The pointer moved with a button down: picks the icon up once it has moved far enough, and
    /// then puts it in the cell under the pointer.
    fn drag_to(&mut self, event: mouse::Event, row: usize, col: usize) {
        let Some(mut drag) = self.drag.filter(|d| !d.over) else { return };
        if !drag.picked {
            let (dx, dy) = (event.x as i32 - drag.from.0 as i32, event.y as i32 - drag.from.1 as i32);
            if dx * dx + dy * dy < DRAG_PIXELS * DRAG_PIXELS {
                return;
            }
            drag.picked = true;
            self.drag = Some(drag);
            self.moving = Some(self.pos.clone());
            self.message = format!("Moving {}", self.icons[drag.icon].name);
        }
        self.place_dragged(drag.icon, row, col);
        self.dirty = true;
    }

    /// Shows the picked-up icon in the cell under the pointer (an icon there is shown at the
    /// dragged one's origin: the swap that letting go makes), or back at its origin when the
    /// pointer is off the grid. Always starts from the places before the pick-up.
    fn place_dragged(&mut self, icon: usize, row: usize, col: usize) {
        let Some(mut pos) = self.moving.clone() else { return };
        if let Some(cell) = self.grid_cell_at(row, col).filter(|&(_, r)| r <= MAX_GRID_ROW) {
            let origin = pos[icon];
            if let Some(other) = pos.iter().position(|&p| p == cell).filter(|&o| o != icon) {
                pos[other] = origin;
            }
            pos[icon] = cell;
        }
        self.pos = pos;
    }

    /// The left button went up: puts a picked-up icon down where it is shown and saves the
    /// places, unless the pointer is off the grid (then it is back where it was).
    fn release(&mut self, row: usize, col: usize) -> Action {
        let Some(drag) = self.drag.take() else { return Action::None };
        if drag.over || !drag.picked {
            return Action::None;
        }
        self.place_dragged(drag.icon, row, col);
        self.moving = None;
        self.message.clear();
        self.dirty = true;
        if self.grid_cell_at(row, col).is_none() {
            return Action::None;
        }
        self.saved = self.places();
        Action::SaveLayout(self.saved.clone())
    }

    /// Esc during a press: a picked-up icon goes back, and the rest of the press does nothing.
    fn cancel_drag(&mut self) {
        let Some(drag) = self.drag.as_mut() else { return };
        if drag.picked {
            if let Some(before) = self.moving.take() {
                self.pos = before;
            }
            self.message.clear();
        }
        drag.picked = false;
        drag.over = true;
        self.dirty = true;
    }

    /// Scrolls so the chosen icon is on the screen.
    fn follow_selection(&mut self) {
        if let Some(&(_, row)) = self.pos.get(self.selected) {
            if row < self.top {
                self.top = row;
            } else if row >= self.top + self.grid_rows {
                self.top = row + 1 - self.grid_rows;
            }
        }
    }

    /// Moves the selected icon one cell; an icon already there takes its old place.
    fn shift(&mut self, dx: isize, dy: isize) {
        let Some(&(col, row)) = self.pos.get(self.selected) else { return };
        let (Some(c), Some(r)) = (col.checked_add_signed(dx), row.checked_add_signed(dy)) else { return };
        if c >= self.cols || r > MAX_GRID_ROW {
            return;
        }
        if let Some(other) = self.icon_in_cell(c, r) {
            self.pos[other] = (col, row);
        }
        self.pos[self.selected] = (c, r);
    }

    /// `P`: paint opens the chosen picture, or a blank canvas (with a note, unless the chosen icon
    /// is Paint itself).
    fn paint_key(&mut self) -> Action {
        match self.selected() {
            Some(icon) if icon.kind == Kind::File(FileType::Image) => Action::Paint(Some(self.path_of(icon))),
            Some(icon) if icon.kind == Kind::Paint => Action::Paint(None),
            _ => {
                self.message = String::from("Not an image: opening a blank canvas");
                Action::Paint(None)
            }
        }
    }

    fn activate(&self) -> Action {
        let Some(icon) = self.selected() else { return Action::None };
        match icon.kind {
            Kind::Terminal => Action::NewTerminal,
            Kind::Files => Action::Enter(String::from("/")),
            Kind::Paint => Action::Paint(None),
            Kind::Up => Action::Enter(parent(&self.cwd)),
            Kind::File(FileType::Dir) => Action::Enter(self.path_of(icon)),
            Kind::File(_) => Action::Open(self.path_of(icon)),
        }
    }

    /// Draws the home screen on the running task's screen (every row but the taskbar).
    pub fn render(&mut self) {
        self.dirty = false;
        let theme = theme::current();
        let background = theme.attr(Item::Desktop);
        self.follow_selection();

        let width = self.width;
        let title = format!(" DESKTOP  {}", self.cwd);
        let count = format!("{} item{} ", self.icons.len(), if self.icons.len() == 1 { "" } else { "s" });
        // The path on the left, the item count on the right.
        let mut header = title.into_bytes();
        header.resize(width.saturating_sub(count.len()).max(header.len()), b' ');
        header.extend(count.bytes());
        header.truncate(width);
        let header_attr = theme.attr(Item::DesktopHeader);
        self.draw(HEADER_ROW, vec![(header, header_attr)], header_attr);
        self.draw(HEADER_ROW + 1, Vec::new(), background);

        // Which icon is in each cell on the screen.
        let (cols, grid_rows, top) = (self.cols, self.grid_rows, self.top);
        let mut on_screen: Vec<Option<usize>> = vec![None; cols * grid_rows];
        for (i, &(col, row)) in self.pos.iter().enumerate() {
            if row >= top && row < top + grid_rows {
                on_screen[(row - top) * cols + col] = Some(i);
            }
        }
        for grid_row in 0..grid_rows {
            for line in 0..CELL_HEIGHT {
                let cells: Vec<(Vec<u8>, u8)> =
                    (0..cols).map(|col| self.icon_line(&theme, on_screen[grid_row * cols + col], line)).collect();
                self.draw(GRID_TOP + grid_row * CELL_HEIGHT + line, cells, background);
            }
        }
        let message_row = self.rows - 2;
        for row in GRID_TOP + grid_rows * CELL_HEIGHT..message_row {
            self.draw(row, Vec::new(), background);
        }

        let above = self.pos.iter().any(|p| p.1 < top);
        let below = self.pos.iter().any(|p| p.1 >= top + grid_rows);
        let more = match (above, below) {
            (true, true) => "  (more above and below: PgUp/PgDn)",
            (false, true) => "  (more below: PgDn)",
            (true, false) => "  (more above: PgUp)",
            (false, false) => "",
        };
        let message = format!(" {}{more}", self.message);
        let message_attr = theme.attr(Item::DesktopMessage);
        self.draw(message_row, vec![(message.into_bytes(), message_attr)], message_attr);
        let hint = match self.moving {
            Some(_) => HINT_MOVING,
            None if HINT_WIDE.len() <= width => HINT_WIDE,
            None => HINT,
        };
        let hint_attr = theme.attr(Item::DesktopHint);
        self.draw(message_row + 1, vec![(hint.to_vec(), hint_attr)], hint_attr);
    }

    /// One screen line (0-4) of the grid cell holding icon `index`: `CELL_WIDTH` cells.
    fn icon_line(&self, theme: &Theme, index: Option<usize>, line: usize) -> (Vec<u8>, u8) {
        let background = theme.attr(Item::Desktop);
        let blank = (vec![b' '; CELL_WIDTH], background);
        let Some(icon) = index.and_then(|i| self.icons.get(i)) else { return blank };
        let selected = index == Some(self.selected);
        let (tag, item) = icon.tag();
        let box_attr = match (selected, self.moving.is_some()) {
            (true, true) => theme.attr(Item::DesktopSelected),
            (true, false) => theme.attr(Item::DesktopFrame),
            (false, _) => theme.attr(item),
        };
        let (tl, tr, bl, br, h, v) = if selected {
            (DOUBLE_TOP_LEFT, DOUBLE_TOP_RIGHT, DOUBLE_BOTTOM_LEFT, DOUBLE_BOTTOM_RIGHT, DOUBLE_HORIZONTAL, DOUBLE_VERTICAL)
        } else {
            (TOP_LEFT, TOP_RIGHT, BOTTOM_LEFT, BOTTOM_RIGHT, HORIZONTAL, VERTICAL)
        };
        // The box is 10 wide, in the middle of the 16-wide cell.
        let inner = 8;
        let mut text = vec![b' '; 3];
        match line {
            0 => {
                text.push(tl);
                text.extend(core::iter::repeat_n(h, inner));
                text.push(tr);
            }
            1 => {
                text.push(v);
                let left = (inner - tag.len()) / 2;
                text.extend(core::iter::repeat_n(b' ', left));
                text.extend(tag.bytes());
                text.extend(core::iter::repeat_n(b' ', inner - left - tag.len()));
                text.push(v);
            }
            2 => {
                text.push(bl);
                text.extend(core::iter::repeat_n(h, inner));
                text.push(br);
            }
            3 => {
                let mut name: Vec<u8> = icon.name.bytes().collect();
                if name.len() > CELL_WIDTH - 2 {
                    name.truncate(CELL_WIDTH - 3);
                    name.push(b'~');
                }
                let left = (CELL_WIDTH - name.len()) / 2;
                let mut text = vec![b' '; left];
                text.extend(name);
                text.resize(CELL_WIDTH, b' ');
                return (text, if selected { theme.attr(Item::DesktopSelected) } else { background });
            }
            _ => return blank,
        }
        text.resize(CELL_WIDTH, b' ');
        (text, box_attr)
    }
}
