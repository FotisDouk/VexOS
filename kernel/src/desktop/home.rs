//! The home screen: a grid of icons for the files in a folder, starting with `/desktop`.
//!
//! `Home::handle` turns keys into actions without touching the disk or the screen, so the
//! self-tests can drive it; the desktop loads folders (`load`) and draws it (`render`).

use crate::console;
use crate::fs;
use crate::input::Key;
use crate::vga::{attr, Color, WIDTH};
use alloc::string::String;
use alloc::vec::Vec;
use kfs::{FileType, Info};

/// The folder the home screen shows first.
pub const DESKTOP_DIR: &str = "/desktop";

/// Icons per row, and the width of each.
const COLS: usize = 5;
const CELL_WIDTH: usize = WIDTH / COLS;
/// Rows of icons on the screen, and the screen rows each one takes.
const GRID_ROWS: usize = 4;
const CELL_HEIGHT: usize = 5;
const HEADER_ROW: usize = 0;
const GRID_TOP: usize = 2;
const MESSAGE_ROW: usize = 22;
const HINT_ROW: usize = 23;

const BACKGROUND: u8 = attr(Color::White, Color::Blue);
const HEADER: u8 = attr(Color::Black, Color::Cyan);
const HINT: u8 = attr(Color::LightCyan, Color::Blue);
const MESSAGE: u8 = attr(Color::Yellow, Color::Blue);
const SELECTED_NAME: u8 = attr(Color::Blue, Color::White);

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Opens a new terminal.
    Terminal,
    /// Shows the whole disk (`/`).
    Files,
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

    /// The label inside the icon's box, and its color.
    fn tag(&self) -> (&'static str, Color) {
        match self.kind {
            Kind::Terminal => ("TERM", Color::LightGreen),
            Kind::Files => ("DISK", Color::LightCyan),
            Kind::Up => ("UP", Color::LightGray),
            Kind::File(t) => match t {
                FileType::Dir => ("DIR", Color::Yellow),
                FileType::Text => ("TXT", Color::White),
                FileType::Asm => ("ASM", Color::LightCyan),
                FileType::Exe => ("VEX", Color::LightGreen),
                FileType::Script => ("V", Color::Pink),
                FileType::Kernel => ("SYS", Color::LightRed),
                FileType::Boot => ("BOOT", Color::LightRed),
                FileType::Raw => ("RAW", Color::Brown),
            },
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
    /// Leave the desktop (the desktop asks first if programs are open).
    Quit,
    /// Leave the desktop, closing open programs: the answer to the question.
    QuitNow,
}

pub struct Home {
    pub cwd: String,
    icons: Vec<Icon>,
    selected: usize,
    /// First icon row on the screen.
    top: usize,
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
    pub fn new() -> Home {
        Home {
            cwd: String::from(DESKTOP_DIR),
            icons: Vec::new(),
            selected: 0,
            top: 0,
            message: String::new(),
            confirm_quit: None,
            dirty: true,
            shown: alloc::vec![None; HINT_ROW + 1],
        }
    }

    /// Draws one row unless it already shows exactly this.
    fn draw(&mut self, row: usize, spans: Vec<(Vec<u8>, u8)>) {
        if self.shown[row].as_ref() == Some(&spans) {
            return;
        }
        let parts: Vec<(&[u8], u8)> = spans.iter().map(|(t, a)| (t.as_slice(), *a)).collect();
        console::draw_spans(row, &parts, BACKGROUND);
        self.shown[row] = Some(spans);
    }

    /// Shows `icons` as the contents of `cwd` (keeps the selection when it is the same folder).
    pub fn set_folder(&mut self, cwd: &str, icons: Vec<Icon>) {
        if self.cwd != cwd {
            self.selected = 0;
            self.top = 0;
            self.cwd = String::from(cwd);
        }
        self.icons = icons;
        self.selected = self.selected.min(self.icons.len().saturating_sub(1));
        self.dirty = true;
    }

    /// Reads folder `path` from the disk. On an error the current folder stays.
    pub fn load(&mut self, path: &str) {
        match fs::with(|fs| fs.read_dir(path)) {
            Ok(entries) => {
                let icons = icons_for(path, entries);
                self.set_folder(path, icons);
            }
            Err(e) => self.message = alloc::format!("{path}: {e}"),
        }
        self.dirty = true;
    }

    pub fn selected(&self) -> Option<&Icon> {
        self.icons.get(self.selected)
    }

    pub fn icons(&self) -> &[Icon] {
        &self.icons
    }

    /// Asks "close N programs and leave?"; the next key answers.
    pub fn ask_quit(&mut self, open: usize) {
        self.confirm_quit = Some(open);
        self.message = alloc::format!(
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
        self.dirty = true;
        if self.confirm_quit.take().is_some() {
            self.message.clear();
            return match key {
                Key::Char(b'y' | b'Y') => Action::QuitNow,
                _ => Action::None,
            };
        }
        self.message.clear();
        let count = self.icons.len();
        let last = count.saturating_sub(1);
        let page = COLS * GRID_ROWS;
        match key {
            Key::Left => self.selected = self.selected.saturating_sub(1),
            Key::Right => self.selected = (self.selected + 1).min(last),
            Key::Up => self.selected = self.selected.checked_sub(COLS).unwrap_or(self.selected),
            Key::Down if self.selected + COLS <= last => self.selected += COLS,
            // On the row above the last, Down goes to the last icon.
            Key::Down if self.selected / COLS < last / COLS => self.selected = last,
            Key::Home => self.selected = 0,
            Key::End => self.selected = last,
            Key::PageUp => self.selected = self.selected.saturating_sub(page),
            Key::PageDown => self.selected = (self.selected + page).min(last),
            Key::Enter => return self.activate(),
            Key::Char(b'e' | b'E') => {
                return match self.selected() {
                    Some(icon) if matches!(icon.kind, Kind::File(t) if t != FileType::Dir) => {
                        Action::Edit(self.path_of(icon))
                    }
                    _ => Action::None,
                }
            }
            Key::Char(b't' | b'T') => return Action::NewTerminal,
            Key::Char(b'r' | b'R') => return Action::Enter(self.cwd.clone()),
            Key::Backspace if self.cwd == "/" => return Action::Enter(String::from(DESKTOP_DIR)),
            Key::Backspace => return Action::Enter(parent(&self.cwd)),
            Key::Escape => return Action::Enter(String::from(DESKTOP_DIR)),
            Key::Ctrl(b'q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    fn activate(&self) -> Action {
        let Some(icon) = self.selected() else { return Action::None };
        match icon.kind {
            Kind::Terminal => Action::NewTerminal,
            Kind::Files => Action::Enter(String::from("/")),
            Kind::Up => Action::Enter(parent(&self.cwd)),
            Kind::File(FileType::Dir) => Action::Enter(self.path_of(icon)),
            Kind::File(_) => Action::Open(self.path_of(icon)),
        }
    }

    /// Draws the home screen on the running task's screen (rows 0-23).
    pub fn render(&mut self) {
        self.dirty = false;
        // Scroll so the selection is on the screen.
        let row = self.selected / COLS;
        if row < self.top {
            self.top = row;
        } else if row >= self.top + GRID_ROWS {
            self.top = row + 1 - GRID_ROWS;
        }

        let title = alloc::format!(" DESKTOP  {}", self.cwd);
        let count = alloc::format!("{} item{} ", self.icons.len(), if self.icons.len() == 1 { "" } else { "s" });
        // The path on the left, the item count on the right.
        let mut header = title.into_bytes();
        header.resize(WIDTH.saturating_sub(count.len()).max(header.len()), b' ');
        header.extend(count.bytes());
        header.truncate(WIDTH);
        self.draw(HEADER_ROW, alloc::vec![(header, HEADER)]);
        self.draw(HEADER_ROW + 1, Vec::new());

        for grid_row in 0..GRID_ROWS {
            for line in 0..CELL_HEIGHT {
                let cells: Vec<(Vec<u8>, u8)> =
                    (0..COLS).map(|col| self.icon_line((self.top + grid_row) * COLS + col, line)).collect();
                self.draw(GRID_TOP + grid_row * CELL_HEIGHT + line, cells);
            }
        }

        let more = if (self.top + GRID_ROWS) * COLS < self.icons.len() { "  (more below: PgDn)" } else { "" };
        let message = alloc::format!(" {}{more}", self.message);
        self.draw(MESSAGE_ROW, alloc::vec![(message.into_bytes(), MESSAGE)]);
        let hint = b" Enter open  E edit  T terminal  Bksp up  Esc desktop  Alt+Tab switch  ^Q leave";
        self.draw(HINT_ROW, alloc::vec![(hint.to_vec(), HINT)]);
    }

    /// One screen line (0-4) of the icon at `index`: 16 cells.
    fn icon_line(&self, index: usize, line: usize) -> (Vec<u8>, u8) {
        let blank = (alloc::vec![b' '; CELL_WIDTH], BACKGROUND);
        let Some(icon) = self.icons.get(index) else { return blank };
        let selected = index == self.selected;
        let (tag, color) = icon.tag();
        let box_attr = attr(if selected { Color::Yellow } else { color }, Color::Blue);
        let (tl, tr, bl, br, h, v) = if selected {
            (DOUBLE_TOP_LEFT, DOUBLE_TOP_RIGHT, DOUBLE_BOTTOM_LEFT, DOUBLE_BOTTOM_RIGHT, DOUBLE_HORIZONTAL, DOUBLE_VERTICAL)
        } else {
            (TOP_LEFT, TOP_RIGHT, BOTTOM_LEFT, BOTTOM_RIGHT, HORIZONTAL, VERTICAL)
        };
        // The box is 10 wide, in the middle of the 16-wide cell.
        let inner = 8;
        let mut text = alloc::vec![b' '; 3];
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
                let mut text = alloc::vec![b' '; left];
                text.extend(name);
                text.resize(CELL_WIDTH, b' ');
                return (text, if selected { SELECTED_NAME } else { BACKGROUND });
            }
            _ => return blank,
        }
        text.resize(CELL_WIDTH, b' ');
        (text, box_attr)
    }
}
