//! The system's looks: the colors of the shell, the editors and the desktop, the RGB values of the
//! 16 colors (the palette), and a few options (the taskbar clock, the cursor).
//!
//! Saved in `/system/theme.cfg` by the `theme` command and read at boot. The file is text, one
//! setting per line, so it can also be changed in the editor. Settings it does not mention keep
//! their classic values.

use crate::display::{self, CursorShape, DEFAULT_PALETTE};
use crate::fs;
use crate::sync::SpinLock;
use crate::vga::{self, Color};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use Color::*;

pub const CONFIG_PATH: &str = "/system/theme.cfg";

/// Everything that has colors of its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    Shell,
    Editor,
    EditorStatus,
    EditorDim,
    Desktop,
    DesktopHeader,
    DesktopHint,
    DesktopMessage,
    DesktopSelected,
    DesktopFrame,
    IconTerminal,
    IconFiles,
    IconUp,
    IconDir,
    IconText,
    IconAsm,
    IconVex,
    IconScript,
    IconSys,
    IconBoot,
    IconRaw,
    IconImage,
    IconPaint,
    Taskbar,
    TaskbarActive,
    TaskbarClock,
    Switcher,
    SwitcherSelected,
}

/// A setting's name, whether it has a background color of its own (a "pair") or uses the one of
/// `base`, and what it colors.
pub struct ItemInfo {
    pub item: Item,
    pub name: &'static str,
    /// The item whose background a foreground-only item is drawn on.
    pub base: Option<Item>,
    pub about: &'static str,
}

const fn pair(item: Item, name: &'static str, about: &'static str) -> ItemInfo {
    ItemInfo { item, name, base: None, about }
}

const fn on(item: Item, name: &'static str, base: Item, about: &'static str) -> ItemInfo {
    ItemInfo { item, name, base: Some(base), about }
}

/// In the order of `Item`.
pub const ITEMS: &[ItemInfo] = &[
    pair(Item::Shell, "shell", "terminals and the shell"),
    pair(Item::Editor, "editor", "text of the text and hex editors"),
    pair(Item::EditorStatus, "editor.status", "the editors' status bar"),
    on(Item::EditorDim, "editor.dim", Item::Editor, "the ~ past the end"),
    pair(Item::Desktop, "desktop", "the home screen; icon names"),
    pair(Item::DesktopHeader, "desktop.header", "the top row"),
    pair(Item::DesktopHint, "desktop.hint", "the row of key reminders"),
    pair(Item::DesktopMessage, "desktop.message", "messages and questions"),
    pair(Item::DesktopSelected, "desktop.selected", "the chosen icon's name"),
    on(Item::DesktopFrame, "desktop.frame", Item::Desktop, "the chosen icon's box"),
    on(Item::IconTerminal, "icon.terminal", Item::Desktop, "Terminal"),
    on(Item::IconFiles, "icon.files", Item::Desktop, "Files"),
    on(Item::IconUp, "icon.up", Item::Desktop, ".. (the folder above)"),
    on(Item::IconDir, "icon.dir", Item::Desktop, "folders"),
    on(Item::IconText, "icon.text", Item::Desktop, "text files"),
    on(Item::IconAsm, "icon.asm", Item::Desktop, ".asm files"),
    on(Item::IconVex, "icon.vex", Item::Desktop, ".vex programs"),
    on(Item::IconScript, "icon.script", Item::Desktop, ".v scripts"),
    on(Item::IconSys, "icon.sys", Item::Desktop, ".sys files"),
    on(Item::IconBoot, "icon.boot", Item::Desktop, ".boot files"),
    on(Item::IconRaw, "icon.raw", Item::Desktop, ".raw files"),
    on(Item::IconImage, "icon.image", Item::Desktop, "picture files (.bmp)"),
    on(Item::IconPaint, "icon.paint", Item::Desktop, "Paint"),
    pair(Item::Taskbar, "taskbar", "the taskbar"),
    pair(Item::TaskbarActive, "taskbar.active", "the task in front"),
    pair(Item::TaskbarClock, "taskbar.clock", "the clock"),
    pair(Item::Switcher, "switcher", "the Alt+Tab list"),
    pair(Item::SwitcherSelected, "switcher.selected", "its highlighted entry"),
];

const COUNT: usize = 28;
const _: () = assert!(ITEMS.len() == COUNT);

/// The options that are not colors: name, the values it takes, and what it does.
pub const OPTIONS: &[(&str, &str, &str)] = &[
    ("clock", "24h 12h", "how the taskbar shows the time"),
    ("clock.seconds", "off on", "the taskbar clock shows seconds"),
    ("clock.date", "on off", "the taskbar shows the date"),
    ("cursor", "underline block", "the shape of the cursor"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    /// The color byte of every item; foreground-only items use the low 4 bits.
    colors: [u8; COUNT],
    pub palette: [[u8; 3]; 16],
    pub clock_12h: bool,
    pub clock_seconds: bool,
    pub clock_date: bool,
    pub cursor: CursorShape,
}

impl Theme {
    /// The looks the system always had: a blue desktop, light gray text on black.
    pub const CLASSIC: Theme = Theme {
        colors: [
            vga::attr(LightGray, Black),
            vga::attr(LightGray, Black),
            vga::attr(Black, LightGray),
            DarkGray as u8,
            vga::attr(White, Blue),
            vga::attr(Black, Cyan),
            vga::attr(LightCyan, Blue),
            vga::attr(Yellow, Blue),
            vga::attr(Blue, White),
            Yellow as u8,
            LightGreen as u8,
            LightCyan as u8,
            LightGray as u8,
            Yellow as u8,
            White as u8,
            LightCyan as u8,
            LightGreen as u8,
            Pink as u8,
            LightRed as u8,
            LightRed as u8,
            Brown as u8,
            Pink as u8,
            LightGreen as u8,
            vga::attr(Black, LightGray),
            vga::attr(White, Blue),
            vga::attr(Black, Cyan),
            vga::attr(Black, LightGray),
            vga::attr(White, Blue),
        ],
        palette: DEFAULT_PALETTE,
        clock_12h: false,
        clock_seconds: false,
        clock_date: true,
        cursor: CursorShape::Underline,
    };

    /// The full color byte of `item` (foreground-only items get their base's background).
    pub fn attr(&self, item: Item) -> u8 {
        let info = &ITEMS[item as usize];
        let own = self.colors[item as usize];
        match info.base {
            Some(base) => (self.colors[base as usize] & 0xF0) | (own & 0x0F),
            None => own,
        }
    }

    fn find(name: &str) -> Option<&'static ItemInfo> {
        ITEMS.iter().find(|i| i.name.eq_ignore_ascii_case(name))
    }

    /// Changes one setting: `name` and its value as words, e.g. `desktop` `white on blue`,
    /// `icon.dir` `yellow`, `palette.blue` `#1e3a8a`, `clock` `12h`.
    pub fn set(&mut self, name: &str, value: &[&str]) -> Result<(), String> {
        let color = |word: &str| {
            Color::from_name(word).ok_or_else(|| format!("unknown color: {word} (see 'color -l')"))
        };
        if let Some(info) = Self::find(name) {
            let at = info.item as usize;
            let words: Vec<&str> = value.iter().copied().filter(|w| !w.eq_ignore_ascii_case("on")).collect();
            match (info.base, &words[..]) {
                (Some(_), [fg]) => self.colors[at] = color(fg)? as u8,
                (None, [fg]) => self.colors[at] = (self.colors[at] & 0xF0) | color(fg)? as u8,
                (None, [fg, bg]) => self.colors[at] = vga::attr(color(fg)?, color(bg)?),
                (Some(_), _) => return Err(format!("{}: give one color (its background is {}'s)", info.name, ITEMS[info.base.unwrap() as usize].name)),
                (None, _) => return Err(format!("{}: give a color, or two: <text> on <background>", info.name)),
            }
            return Ok(());
        }
        let lower = name.to_ascii_lowercase();
        if let Some(color_name) = lower.strip_prefix("palette.") {
            let index = color(color_name)? as usize;
            let [word] = value else { return Err(format!("{name}: give one color value, e.g. #1e3a8a")) };
            self.palette[index] = if word.eq_ignore_ascii_case("default") {
                DEFAULT_PALETTE[index]
            } else {
                parse_rgb(word).ok_or_else(|| format!("{name}: not a color value: {word} (use #rrggbb)"))?
            };
            return Ok(());
        }
        let switch = |word: &str| match word.to_ascii_lowercase().as_str() {
            "on" | "yes" => Ok(true),
            "off" | "no" => Ok(false),
            _ => Err(format!("{name}: say on or off")),
        };
        match (lower.as_str(), value) {
            ("clock", [word]) => {
                self.clock_12h = match word.to_ascii_lowercase().as_str() {
                    "12h" | "12" => true,
                    "24h" | "24" => false,
                    _ => return Err(String::from("clock: say 24h or 12h")),
                }
            }
            ("clock.seconds", [word]) => self.clock_seconds = switch(word)?,
            ("clock.date", [word]) => self.clock_date = switch(word)?,
            ("cursor", [word]) => {
                self.cursor = match word.to_ascii_lowercase().as_str() {
                    "underline" => CursorShape::Underline,
                    "block" => CursorShape::Block,
                    _ => return Err(String::from("cursor: say underline or block")),
                }
            }
            ("clock" | "clock.seconds" | "clock.date" | "cursor", _) => return Err(format!("{name}: give one value")),
            _ => return Err(format!("unknown setting: {name} (see 'theme')")),
        }
        Ok(())
    }

    /// The value of a setting as `set` takes it.
    pub fn value(&self, name: &str) -> Option<String> {
        if let Some(info) = Self::find(name) {
            let byte = self.colors[info.item as usize];
            return Some(match info.base {
                Some(_) => String::from(Color::from_index(byte).name()),
                None => format!("{} on {}", Color::from_index(byte).name(), Color::from_index(byte >> 4).name()),
            });
        }
        let lower = name.to_ascii_lowercase();
        if let Some(color_name) = lower.strip_prefix("palette.") {
            let [r, g, b] = self.palette[Color::from_name(color_name)? as usize];
            return Some(format!("#{r:02x}{g:02x}{b:02x}"));
        }
        let on_off = |b: bool| String::from(if b { "on" } else { "off" });
        Some(match lower.as_str() {
            "clock" => String::from(if self.clock_12h { "12h" } else { "24h" }),
            "clock.seconds" => on_off(self.clock_seconds),
            "clock.date" => on_off(self.clock_date),
            "cursor" => String::from(if self.cursor == CursorShape::Block { "block" } else { "underline" }),
            _ => return None,
        })
    }

    /// Every setting's name, in the order the file lists them.
    pub fn names() -> Vec<String> {
        let mut names: Vec<String> = ITEMS.iter().map(|i| String::from(i.name)).collect();
        names.extend(Color::ALL.iter().map(|(n, _)| format!("palette.{n}")));
        names.extend(OPTIONS.iter().map(|(n, ..)| String::from(*n)));
        names
    }

    /// The contents of `/system/theme.cfg`.
    pub fn to_config(&self) -> String {
        let mut text = String::from(
            "# The system's looks, written by 'theme' (see /docs/commands/screen.txt).\n\
             # It is read at start-up; you can edit it too. One setting per line:\n\
             #   <item> <color> [on <background>]   colors (see 'color -l')\n\
             #   palette.<color> #rrggbb            what a color looks like\n",
        );
        for name in Self::names() {
            let value = self.value(&name).unwrap_or_default();
            text.push_str(&format!("{name:<20}{value}\n"));
        }
        text
    }

    /// Reads a theme file. Lines it cannot use are skipped and reported.
    pub fn parse(text: &str) -> (Theme, Vec<String>) {
        let mut theme = Theme::CLASSIC;
        let mut errors = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let words: Vec<&str> = line.split_whitespace().collect();
            let Some((name, value)) = words.split_first() else { continue };
            if name.starts_with('#') {
                continue;
            }
            if let Err(e) = theme.set(name, value) {
                errors.push(format!("line {}: {e}", number + 1));
            }
        }
        (theme, errors)
    }
}

/// `#rrggbb` (or `rrggbb`) as red, green and blue.
pub fn parse_rgb(text: &str) -> Option<[u8; 3]> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let part = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([part(0)?, part(2)?, part(4)?])
}

/// Ready-made looks: their names, and the settings they change from the classic ones.
pub const PRESETS: &[(&str, &[(&str, &str)])] = &[
    ("classic", &[]),
    (
        "dark",
        &[
            ("editor.status", "white on darkgray"),
            ("desktop", "lightgray on black"),
            ("desktop.header", "white on darkgray"),
            ("desktop.hint", "cyan on black"),
            ("desktop.message", "yellow on black"),
            ("desktop.selected", "black on lightcyan"),
            ("desktop.frame", "lightcyan"),
            ("icon.text", "lightgray"),
            ("taskbar", "lightgray on darkgray"),
            ("taskbar.active", "black on lightcyan"),
            ("taskbar.clock", "white on darkgray"),
            ("switcher", "lightgray on darkgray"),
            ("switcher.selected", "black on lightcyan"),
            ("palette.darkgray", "#3a3a3a"),
        ],
    ),
    (
        "light",
        &[
            ("shell", "black on white"),
            ("editor", "black on white"),
            ("editor.status", "white on blue"),
            ("editor.dim", "lightgray"),
            ("desktop", "black on white"),
            ("desktop.header", "white on blue"),
            ("desktop.hint", "blue on white"),
            ("desktop.message", "red on white"),
            ("desktop.selected", "white on blue"),
            ("desktop.frame", "blue"),
            ("icon.terminal", "green"),
            ("icon.files", "blue"),
            ("icon.up", "darkgray"),
            ("icon.dir", "brown"),
            ("icon.text", "black"),
            ("icon.asm", "cyan"),
            ("icon.vex", "green"),
            ("icon.script", "magenta"),
            ("icon.sys", "red"),
            ("icon.boot", "red"),
            ("icon.raw", "brown"),
            ("icon.image", "magenta"),
            ("icon.paint", "green"),
            ("taskbar", "black on lightgray"),
            ("taskbar.active", "white on blue"),
            ("taskbar.clock", "white on darkgray"),
            ("palette.white", "#f4f4f0"),
        ],
    ),
    (
        "amber",
        &[
            ("shell", "yellow on black"),
            ("editor", "yellow on black"),
            ("editor.status", "black on yellow"),
            ("editor.dim", "brown"),
            ("desktop", "yellow on black"),
            ("desktop.header", "black on yellow"),
            ("desktop.hint", "brown on black"),
            ("desktop.message", "white on black"),
            ("desktop.selected", "black on yellow"),
            ("desktop.frame", "white"),
            ("icon.terminal", "yellow"),
            ("icon.files", "yellow"),
            ("icon.up", "brown"),
            ("icon.dir", "white"),
            ("icon.text", "yellow"),
            ("icon.asm", "yellow"),
            ("icon.vex", "white"),
            ("icon.script", "yellow"),
            ("icon.sys", "white"),
            ("icon.boot", "white"),
            ("icon.raw", "brown"),
            ("icon.image", "yellow"),
            ("icon.paint", "yellow"),
            ("taskbar", "black on brown"),
            ("taskbar.active", "black on yellow"),
            ("taskbar.clock", "black on yellow"),
            ("switcher", "yellow on brown"),
            ("switcher.selected", "black on yellow"),
            ("palette.yellow", "#ffb000"),
            ("palette.brown", "#8a5a00"),
            ("palette.white", "#ffd890"),
        ],
    ),
];

pub fn preset(name: &str) -> Option<Theme> {
    let (_, changes) = PRESETS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name))?;
    let mut theme = Theme::CLASSIC;
    for (setting, value) in changes.iter() {
        let words: Vec<&str> = value.split_whitespace().collect();
        theme.set(setting, &words).expect("bad preset");
    }
    Some(theme)
}

static THEME: SpinLock<Theme> = SpinLock::new(Theme::CLASSIC);

pub fn current() -> Theme {
    *THEME.lock()
}

/// The full color byte of `item` in the current theme.
pub fn attr(item: Item) -> u8 {
    THEME.lock().attr(item)
}

/// Makes `theme` the current one: the palette and cursor change at once, terminals showing the
/// old shell colors take the new ones, and the desktop and the editors draw themselves again.
pub fn apply(theme: Theme) {
    let old = core::mem::replace(&mut *THEME.lock(), theme);
    crate::console::retheme(old.attr(Item::Shell), theme.attr(Item::Shell), theme.palette, theme.cursor);
    display::touch();
}

/// Reads `CONFIG_PATH` and applies it. Returns the problems found in the file, if any.
pub fn load() -> Vec<String> {
    let Ok(data) = fs::with(|fs| fs.read(CONFIG_PATH)) else {
        apply(current());
        return Vec::new();
    };
    let (theme, errors) = Theme::parse(&String::from_utf8_lossy(&data));
    apply(theme);
    errors
}

pub fn save() -> fs::Result<()> {
    let text = current().to_config();
    fs::with(|fs| fs.write(CONFIG_PATH, text.as_bytes()))
}
