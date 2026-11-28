//! Paint's side panel and its strings, as pure functions: the one layout table that both the
//! drawing and the mouse hit tests read (so a row that is dropped on a short screen is never
//! clickable, and a click never lands on a row that is not drawn), and the hint texts.
//!
//! Panel columns are counted from the separator (column 0); the 12 cells to its right hold the
//! text. See `CLAUDE/design/image-update-layouts.md`, sections 1.3 to 1.5 and 2.3.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The panel's width in text columns (the separator and 13 more).
pub const PANEL_W: usize = 14;
/// The smallest screen paint runs on.
pub const MIN_COLS: usize = 40;
pub const MIN_ROWS: usize = 12;

/// Shown instead of paint on a screen below the minimum.
pub const TOO_SMALL: &str = "Screen too small for paint (needs 40x12). Ctrl-Q quits.";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Pencil,
    Line,
    Rect,
    Ellipse,
    Bucket,
    Picker,
    Eraser,
    /// Drags the view (mouse); with the keys W A S D pan.
    Hand,
}

impl Tool {
    pub fn letter(self) -> u8 {
        match self {
            Tool::Pencil => b'P',
            Tool::Line => b'L',
            Tool::Rect => b'R',
            Tool::Ellipse => b'O',
            Tool::Bucket => b'B',
            Tool::Picker => b'I',
            Tool::Eraser => b'E',
            Tool::Hand => b'H',
        }
    }

    pub fn from_letter(c: u8) -> Option<Tool> {
        match c.to_ascii_uppercase() {
            b'P' => Some(Tool::Pencil),
            b'L' => Some(Tool::Line),
            b'R' => Some(Tool::Rect),
            b'O' => Some(Tool::Ellipse),
            b'B' => Some(Tool::Bucket),
            b'I' => Some(Tool::Picker),
            b'E' => Some(Tool::Eraser),
            b'H' => Some(Tool::Hand),
            _ => None,
        }
    }

    /// Two-corner tools: line, rectangle, ellipse.
    pub fn is_shape(self) -> bool {
        matches!(self, Tool::Line | Tool::Rect | Tool::Ellipse)
    }

    /// Tools that draw while the pen is down.
    pub fn is_pen(self) -> bool {
        matches!(self, Tool::Pencil | Tool::Eraser)
    }
}

/// One line of the panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    /// The file name, `*` if changed.
    Name,
    /// `[P][L][R][O]`
    Tools1,
    /// `[B][I][E][H]`
    Tools2,
    Fill,
    Size,
    /// `Pen up/DOWN` or `Corner -/set`.
    Pen,
    Blank,
    Fg,
    Bg,
    /// Swatch row `n` (0 to 7): colours `n` and `n + 8`.
    Palette(u8),
    Custom,
    Xy,
    /// The canvas size, `WxH`.
    Dims,
    /// `Zoom x4` or `Zoom 1/4`.
    Zoom,
    /// The canvas pixel at the top left of the view, `View X,Y`.
    View,
    Step,
}

/// Rows of the full panel.
pub const FULL_ROWS: usize = 25;

/// What a panel row holds, top to bottom, for a screen with `n` work rows (all rows but the bar).
/// Rows are dropped in the order of the design file when `n` is short: the three blanks, then
/// step, view, canvas size, pen state, x:y, zoom, the name (all the information), then the custom
/// row and the palette rows from the bottom. The mouse targets (tools, fill, size, fg/bg,
/// swatches) outlast every information line.
pub fn panel_rows(n: usize) -> Vec<Row> {
    let mut rows = Vec::with_capacity(FULL_ROWS);
    rows.extend([Row::Name, Row::Tools1, Row::Tools2, Row::Fill, Row::Size, Row::Pen, Row::Blank, Row::Fg, Row::Bg, Row::Blank]);
    rows.extend((0..8).map(Row::Palette));
    rows.extend([Row::Custom, Row::Blank, Row::Xy, Row::Dims, Row::Zoom, Row::View, Row::Step]);
    // Positions in the full table, in the order they are dropped.
    const DROPS: [usize; 20] = [19, 9, 6, 24, 23, 21, 5, 20, 22, 0, 18, 17, 16, 15, 14, 13, 12, 11, 10, 4];
    let mut gone = [false; FULL_ROWS];
    for &at in DROPS.iter().take(FULL_ROWS.saturating_sub(n)) {
        gone[at] = true;
    }
    let mut at = 0;
    rows.retain(|_| {
        at += 1;
        !gone[at - 1]
    });
    rows
}

/// What a click on the panel does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Tool(Tool),
    Fill,
    Size,
    /// The foreground and background rows: swap them.
    Swap,
    /// A palette swatch (0 to 15) or the custom one (16).
    Swatch(u8),
}

/// The target at panel row `row`, panel column `col` (0 is the separator), for a table from
/// `panel_rows`.
pub fn hit(rows: &[Row], row: usize, col: usize) -> Option<Target> {
    let kind = *rows.get(row)?;
    let tool = |first: usize, tools: &[Tool]| -> Option<Target> {
        let i = col.checked_sub(first)? / 3;
        (col >= first && col < first + 3 * tools.len()).then(|| Target::Tool(tools[i]))
    };
    match kind {
        Row::Tools1 => tool(1, &[Tool::Pencil, Tool::Line, Tool::Rect, Tool::Ellipse]),
        Row::Tools2 => tool(1, &[Tool::Bucket, Tool::Picker, Tool::Eraser, Tool::Hand]),
        Row::Fill => (1..=12).contains(&col).then_some(Target::Fill),
        Row::Size => (1..=12).contains(&col).then_some(Target::Size),
        Row::Fg | Row::Bg => (1..=12).contains(&col).then_some(Target::Swap),
        Row::Palette(r) => match col {
            1..=4 => Some(Target::Swatch(r)),
            5..=8 => Some(Target::Swatch(r + 8)),
            _ => None,
        },
        Row::Custom => (1..=10).contains(&col).then_some(Target::Swatch(16)),
        _ => None,
    }
}

/// Panel columns (from the separator) of the colour boxes a row draws, each 2 cells wide, with the
/// swatch slot they show (`None`: the foreground colour for `Fg`, background for `Bg`).
pub fn swatch_cells(kind: Row) -> Vec<(usize, Option<u8>)> {
    match kind {
        Row::Fg | Row::Bg => alloc::vec![(4, None)],
        Row::Palette(r) => alloc::vec![(2, Some(r)), (6, Some(r + 8))],
        Row::Custom => alloc::vec![(2, Some(16))],
        _ => Vec::new(),
    }
}

/// The idle hint for a bar `width` columns wide. After the mouse was used, a mouse hint takes
/// the place of the longer variants.
pub fn idle_hint(width: usize, mouse: bool) -> &'static str {
    if mouse && width >= HINT_MOUSE_WIDE.len() {
        HINT_MOUSE_WIDE
    } else if mouse && width >= HINT_MOUSE_PAN.len() {
        HINT_MOUSE_PAN
    } else if mouse && width >= HINT_MOUSE.len() {
        HINT_MOUSE
    } else if width >= HINT_WIDEST.len() {
        HINT_WIDEST
    } else if width >= HINT_KEYS.len() {
        HINT_KEYS
    } else if width >= HINT_SHORT.len() {
        HINT_SHORT
    } else {
        HINT_MIN
    }
}

pub const HINT_WIDEST: &str = " Space act  Tab step  PgUp/PgDn zoom  z fit  Z x1  WASD pan  P L R O B I E H tool  F fill  ^S save  ^Q quit";
pub const HINT_KEYS: &str = " Space act  Tab step  P L R O B I E H tool  F fill  ^S save  ^Q quit";
pub const HINT_SHORT: &str = " Space act  P L R O B I E H  ^S save  ^Q quit";
pub const HINT_MIN: &str = " Space act  ^S save  ^Q quit";
pub const HINT_MOUSE: &str = " Left fg  Right bg  Space act  ^Z undo  ^S save  ^Q quit";
pub const HINT_MOUSE_PAN: &str = " Left fg  Right bg  Middle pan  Space act  ^Z undo  ^S save  ^Q quit";
pub const HINT_MOUSE_WIDE: &str = " Left fg  Right bg  Middle pan  Space act  PgUp/PgDn zoom  WASD pan  ^Z undo  ^S save  ^Q quit";

pub const HINT_PEN_DOWN: &str = " PEN DOWN: arrows draw  Space or Esc lifts  ^Q quit";
pub const HINT_CORNER: &str = " Corner set: move, Space draws, Esc cancels";
pub const HINT_DRAG: &str = " Drag: release draws  Esc cancels";
/// The message of Space or Enter with the Hand tool.
pub const HINT_HAND: &str = " Hand tool: drag the canvas with the mouse to pan, or use W A S D";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptKind {
    SaveAs,
    Overwrite,
    NewSize,
    Colour,
    Load,
    DiscardNew,
    DiscardLoad,
}

/// What a text prompt shows before what is typed (the cursor follows it).
pub fn prompt_label(kind: PromptKind) -> &'static str {
    match kind {
        PromptKind::SaveAs => " Save as: ",
        PromptKind::NewSize => " New size WxH: ",
        PromptKind::Colour => " Colour RRGGBB: ",
        PromptKind::Load => " Load file: ",
        _ => "",
    }
}

/// The bar text of a prompt: the label, what is typed so far and the keys; the keys go first when
/// the bar is narrow.
pub fn prompt_text(kind: PromptKind, typed: &str, name: &str, width: usize) -> String {
    let (label, keys) = match kind {
        PromptKind::SaveAs => (" Save as: ", "  Enter ok  Esc no"),
        PromptKind::NewSize => (" New size WxH: ", "  Enter ok  Esc no"),
        PromptKind::Colour => (" Colour RRGGBB: ", "  Enter ok  Esc no"),
        PromptKind::Load => (" Load file: ", "  Enter ok  Esc no"),
        PromptKind::Overwrite => return cut(format!(" {name} exists. Overwrite? (y/n)"), width),
        PromptKind::DiscardNew => return cut(String::from(" Unsaved changes. Discard them and start a new canvas? (y/n)"), width),
        PromptKind::DiscardLoad => return cut(String::from(" Unsaved changes. Discard them and load another file? (y/n)"), width),
    };
    let long = format!("{label}{typed}{keys}");
    if long.len() <= width {
        return long;
    }
    let short = format!("{label}{typed}");
    if short.len() <= width {
        return short;
    }
    // Too long: show the end of what is typed after the label.
    let room = width.saturating_sub(label.len());
    let tail = &typed[typed.len().saturating_sub(room)..];
    cut(format!("{label}{tail}"), width)
}

fn cut(mut s: String, width: usize) -> String {
    s.truncate(width);
    s
}

/// The x:y line: two spaces between the numbers, one if the 13 text cells have no room for two.
pub fn xy_text(x: i32, y: i32) -> String {
    let long = format!("x:{x}  y:{y}");
    if long.len() < PANEL_W {
        long
    } else {
        format!("x:{x} y:{y}")
    }
}

/// The view line: `View X,Y`, or `V X,Y` if that is longer than the 13 text cells.
pub fn view_text(x: i32, y: i32) -> String {
    let long = format!("View {x},{y}");
    if long.len() < PANEL_W {
        long
    } else {
        format!("V {x},{y}")
    }
}

/// A file name for the panel: at most 12 characters, the last `~` if it was cut.
pub fn short_name(name: &str) -> String {
    if name.chars().count() <= 12 {
        String::from(name)
    } else {
        let mut cut: String = name.chars().take(11).collect();
        cut.push('~');
        cut
    }
}
