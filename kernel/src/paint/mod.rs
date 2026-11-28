//! `paint`: the picture editor. It works with the keyboard and, once a mouse is there, with the
//! mouse too; the keyboard keeps every function.
//!
//! * Text mode (and serial): the canvas is drawn as half blocks (a screen unit is one half-block
//!   column by one pixel row), in the nearest of the theme's 16 colors, no dither. The file keeps
//!   the exact colors.
//! * Graphics modes: true color through the pixel surface (`console::draw_pixels`); a screen unit
//!   is a screen pixel.
//! * Zoom (`ZOOMS`, one table for both modes; text mode stops at x8): `up/down` screen units per
//!   canvas pixel, from 1/64 to x16. Magnified, a pixel is a block of units; reduced, every unit
//!   shows one pixel (the top left of its block of `down` pixels: point sampling, so thin lines
//!   can hide). Every mapping (drawing, the pointer, dirty rectangles) goes through the helpers
//!   below `ZOOMS`.
//!
//! The screen is the canvas area on the left, a 14-column text panel on the right and a hint bar
//! in the last row. The 16 swatches are the fixed classic VGA colors (a drawing must not change
//! with the theme) plus one custom slot.
//!
//! The logic is split from the drawing so the self-tests can drive it without a screen:
//! `Paint::handle(Key) -> bool` takes keys and mouse events (`Key::Mouse`), `set_screen` tells it
//! the screen's size, and `layout.rs` holds the pure panel table that drawing and mouse hit tests
//! share.
//!
//! * `mod.rs`: the state, the geometry (canvas to screen and back), opening files.
//! * `edit.rs`: tools, undo and redo, saving and loading.
//! * `input.rs`: keys, prompts and the mouse.
//! * `render.rs`: the screen and the run loop.
//! * `selftest.rs`: the boot self-tests (run from `selftest::run`).

mod edit;
mod input;
pub mod layout;
mod render;
pub mod selftest;

pub use edit::parse_size;
pub use layout::{PromptKind, Row, Target, Tool};

use crate::mouse::{Button, DoubleClick};
use crate::{fs, picture};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bmp::{Image, Rect};
use layout::{MIN_COLS, MIN_ROWS, PANEL_W};

/// The size of a new canvas.
pub const DEFAULT_SIZE: (u32, u32) = (64, 40);

/// The error for a size that is not `WxH` or has a side of 0 (without the command's name).
pub const SIZE_ERROR: &str = "size must be WxH, like 64x40 (up to 4096x4096)";

/// The 16 fixed colors: the classic VGA palette, whatever the theme.
pub const VGA: [u32; 16] = [
    0x000000, 0x0000AA, 0x00AA00, 0x00AAAA, 0xAA0000, 0xAA00AA, 0xAA5500, 0xAAAAAA, 0x555555, 0x5555FF, 0x55FF55, 0x55FFFF,
    0xFF5555, 0xFF55FF, 0xFFFF55, 0xFFFFFF,
];

/// The custom colour's slot (after the 16 fixed ones).
pub const CUSTOM: u8 = 16;
/// The zoom levels, `(up, down)`: `up/down` screen units per canvas pixel. Both modes use this
/// table; text mode stops at `ZOOM_TEXT_MAX`.
pub const ZOOMS: [(i32, i32); 19] = [
    (1, 64),
    (1, 48),
    (1, 32),
    (1, 24),
    (1, 16),
    (1, 12),
    (1, 8),
    (1, 6),
    (1, 4),
    (1, 3),
    (1, 2),
    (1, 1),
    (2, 1),
    (3, 1),
    (4, 1),
    (6, 1),
    (8, 1),
    (12, 1),
    (16, 1),
];
/// The index of x1 in `ZOOMS`, and of x8: the largest level of text mode, and the largest "fit"
/// of graphics modes.
pub const ZOOM_ONE: usize = 11;
pub const ZOOM_TEXT_MAX: usize = 16;
pub const MAX_BRUSH: u32 = 8;
/// Undo: at most this many steps, within `undo_budget()` bytes.
const UNDO_STEPS: usize = 16;

/// The bytes the undo history may hold: an eighth of the heap, between 8 and 64 MiB.
pub fn undo_budget() -> usize {
    (crate::heap::stats().size / 8).clamp(8 << 20, 64 << 20)
}

// --- The geometry of a zoom level, as pure functions ---
//
// Area unit `a`, counted from the canvas's origin, shows the canvas pixel `view + a * down / up`
// (integer division). The canvas pixel `view + k` is drawn at the units `unit_of(k)` up to
// `extent(k + 1)`.

/// `a / b` rounded down (`b` > 0), also for negative `a`.
pub fn floor_div(a: i32, b: i32) -> i32 {
    a.div_euclid(b)
}

/// `a / b` rounded up (`b` > 0).
pub fn ceil_div(a: i32, b: i32) -> i32 {
    -(-a).div_euclid(b)
}

/// How many pixels past the view's first one area unit `a` (from the canvas origin) shows.
pub fn sample_off(a: i32, z: (i32, i32)) -> i32 {
    floor_div(a * z.1, z.0)
}

/// The area unit (from the canvas origin) where the pixel `k` past the view's first one starts.
pub fn unit_of(k: i32, z: (i32, i32)) -> i32 {
    floor_div(k * z.0, z.1)
}

/// The area units `n` canvas pixels cover (the end of the pixel `n - 1`).
pub fn extent(n: i32, z: (i32, i32)) -> i32 {
    ceil_div(n * z.0, z.1)
}

/// The canvas pixels shown along an axis of `len` pixels in `area` units: whole pixels when
/// magnified, at least 1.
pub fn visible_len(len: i32, area: i32, z: (i32, i32)) -> i32 {
    len.min((area * z.1 / z.0).max(1))
}

/// Where the canvas starts along an axis: centred if all of it is visible, else 0.
pub fn origin_1d(len: i32, area: i32, z: (i32, i32)) -> i32 {
    if visible_len(len, area, z) == len {
        (area - extent(len, z)).max(0) / 2
    } else {
        0
    }
}

/// How a zoom level reads: `x4` or `1/4`.
pub fn zoom_label(z: (i32, i32)) -> String {
    if z.1 == 1 {
        format!("x{}", z.0)
    } else {
        format!("{}/{}", z.0, z.1)
    }
}
/// Pixels of slack a stroke's undo snapshot grows by, so it is not rebuilt at every move.
const SNAP_MARGIN: i32 = 16;

/// A pencil or eraser stroke in progress: the pen is down.
struct Stroke {
    /// The last point drawn; the next move draws a line from here.
    last: (i32, i32),
    color: u32,
    /// The mouse button that holds it (None: the keyboard's pen).
    button: Option<Button>,
}

/// A rectangle snapshot of the canvas: the pixels of `rect` as they were.
struct Snap {
    rect: Rect,
    data: Vec<u32>,
}

/// A line, rectangle or ellipse in progress. Its preview is drawn into the canvas, over a
/// snapshot (`snap`) of what it covers; cancelling pastes the snapshot back, committing turns it
/// into the undo step.
struct Shape {
    start: (i32, i32),
    color: u32,
    button: Option<Button>,
    snap: Option<Snap>,
}

/// The undo snapshot of a stroke in progress, growing with it. `dirty` is what the stroke
/// changed; the step stored at the end is cropped to it.
struct Pending {
    rect: Rect,
    data: Vec<u32>,
    dirty: Rect,
    /// False if memory ran out: the stroke is drawn but cannot be undone.
    recording: bool,
}

/// One undo or redo step: the pixels `data` belong in `rect` (undoing swaps them with what is
/// there, so the same step redoes).
struct Step {
    rect: Rect,
    data: Vec<u32>,
}

struct Prompt {
    kind: PromptKind,
    text: String,
    /// The text is a suggestion: the first character typed replaces it.
    fresh: bool,
    /// The path to write after "replace? (y/n)".
    path: String,
    /// The colour prompt sets the background (`C`) instead of the foreground (`c`).
    bg: bool,
}

impl Prompt {
    fn new(kind: PromptKind, text: String, fresh: bool) -> Prompt {
        Prompt { kind, text, fresh, path: String::new(), bg: false }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DragKind {
    /// A press on the canvas: a stroke, a shape drag or a click.
    Canvas,
    /// The Hand tool, or the middle button: the view follows the pointer.
    Pan,
    /// A press on the panel: its click was handled, the rest is ignored.
    Panel,
    /// A press somewhere that does nothing.
    Ignore,
}

/// A mouse button held down since a press.
struct Drag {
    button: Button,
    kind: DragKind,
    /// For a pan: the pointer (area units) and the view at the press.
    anchor: ((i32, i32), (i32, i32)),
}

impl Drag {
    fn new(button: Button, kind: DragKind) -> Drag {
        Drag { button, kind, anchor: ((0, 0), (0, 0)) }
    }
}

/// The theme colors paint needs, read again when the theme changes.
#[derive(Clone, Copy)]
pub struct Looks {
    pub normal: u8,
    pub dim: u8,
    pub status: u8,
    pub palette: [[u8; 3]; 16],
}

impl Looks {
    pub const CLASSIC: Looks = Looks { normal: 0x07, dim: 0x08, status: 0x70, palette: crate::display::DEFAULT_PALETTE };
}

pub struct Paint {
    img: Image,
    /// Where it is saved (absolute); None: untitled.
    path: Option<String>,
    /// The folder names typed in prompts are relative to.
    cwd: String,
    modified: bool,
    /// The canvas cursor.
    cx: i32,
    cy: i32,
    /// Arrow keys move by this much (1 or 8).
    step: i32,
    tool: Tool,
    /// The tool the picker returns to.
    prev_tool: Tool,
    fill: bool,
    brush: u32,
    /// Foreground and background: a swatch 0..=16.
    fg: u8,
    bg: u8,
    custom: u32,
    stroke: Option<Stroke>,
    shape: Option<Shape>,
    pending: Option<Pending>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    prompt: Option<Prompt>,
    quit_armed: bool,
    message: String,
    /// A mouse event came in this session: the hint line tells how to use it.
    mouse_seen: bool,
    drag: Option<Drag>,
    dbl: DoubleClick,

    // The screen.
    cols: usize,
    rows: usize,
    graphics: bool,
    /// The size of a text cell in area pixels (8x16 in text mode).
    cell: (usize, usize),
    /// The zoom level: an index into `ZOOMS`.
    zoom: usize,
    /// The user chose the zoom (`z`, fit, takes it back): a new display mode keeps it.
    zoom_set: bool,
    /// The canvas pixel at the top left of the visible part.
    view: (i32, i32),
    looks: Looks,
    /// The cursor is drawn (it blinks).
    blink: bool,
    /// Canvas rectangle changed since the pixels were drawn (graphics modes).
    dirty: Rect,
    /// Every pixel has to be drawn again.
    px_full: bool,
    /// The cells of every row as last drawn, so only changed rows are drawn.
    shown: Vec<Vec<u16>>,
    /// The hardware cursor is shown (while a prompt is open).
    cursor_on: bool,
}

impl Paint {
    /// A paint session on `img`, saved as `path` (None: untitled), with `cwd` for typed names.
    /// It starts as on an 80x25 text screen; `set_screen` fits it to the real one.
    pub fn new(img: Image, path: Option<String>, cwd: &str) -> Paint {
        let mut paint = Paint {
            img,
            path,
            cwd: String::from(cwd),
            modified: false,
            cx: 0,
            cy: 0,
            step: 1,
            tool: Tool::Pencil,
            prev_tool: Tool::Pencil,
            fill: false,
            brush: 1,
            fg: 0,
            bg: 15,
            custom: 0x808080,
            stroke: None,
            shape: None,
            pending: None,
            undo: Vec::new(),
            redo: Vec::new(),
            prompt: None,
            quit_armed: false,
            message: String::new(),
            mouse_seen: false,
            drag: None,
            dbl: DoubleClick::new(),
            cols: 80,
            rows: 25,
            graphics: false,
            cell: (8, 16),
            zoom: ZOOM_ONE,
            zoom_set: false,
            view: (0, 0),
            looks: Looks::CLASSIC,
            blink: true,
            dirty: Rect::EMPTY,
            px_full: true,
            shown: Vec::new(),
            cursor_on: false,
        };
        paint.set_screen(80, 25, false, (8, 16));
        paint
    }

    /// Opens what the `paint` command and the desktop ask for: `file` (relative to `cwd`) if it
    /// exists, else a blank canvas that will be saved under that name; `size` is the new canvas's
    /// size (64x40 if None) and is an error with an existing file. Errors are one line, without
    /// the command's name.
    pub fn open(cwd: &str, file: Option<&str>, size: Option<(u32, u32)>) -> Result<Paint, String> {
        let blank = |(w, h): (u32, u32)| {
            if w == 0 || h == 0 {
                return Err(String::from(SIZE_ERROR)); // not a size at all, not "too large"
            }
            picture::blank(w, h, 0xFFFFFF).map_err(|e| format!("{e}"))
        };
        let Some(file) = file else {
            return Ok(Paint::new(blank(size.unwrap_or(DEFAULT_SIZE))?, None, cwd));
        };
        let path = kfs::resolve(cwd, file);
        match fs::with(|fs| fs.stat(&path)) {
            Ok(_) => {
                if size.is_some() {
                    return Err(format!("{file} exists: -s only sets the size of a new picture"));
                }
                let img = picture::load(&path).map_err(|e| format!("{file}: {e}"))?;
                Ok(Paint::new(img, Some(path), cwd))
            }
            Err(fs::FsError::Fs(kfs::Error::NotFound)) => {
                let img = blank(size.unwrap_or(DEFAULT_SIZE))?;
                Ok(Paint::new(img, Some(edit::with_extension(&path)), cwd))
            }
            Err(e) => Err(format!("{file}: {e}")),
        }
    }

    // --- What the self-tests look at ---

    pub fn image(&self) -> &Image {
        &self.img
    }

    pub fn cursor(&self) -> (i32, i32) {
        (self.cx, self.cy)
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn fill_on(&self) -> bool {
        self.fill
    }

    pub fn brush(&self) -> u32 {
        self.brush
    }

    /// The foreground and background swatches (0 to 16) and the custom colour.
    pub fn colors(&self) -> (u8, u8, u32) {
        (self.fg, self.bg, self.custom)
    }

    pub fn is_modified(&self) -> bool {
        self.modified
    }

    pub fn pen_is_down(&self) -> bool {
        self.stroke.is_some()
    }

    pub fn shape_in_progress(&self) -> bool {
        self.shape.is_some()
    }

    pub fn undo_depth(&self) -> (usize, usize) {
        (self.undo.len(), self.redo.len())
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn prompt_kind(&self) -> Option<PromptKind> {
        self.prompt.as_ref().map(|p| p.kind)
    }

    #[cfg_attr(not(feature = "qemu-test"), allow(dead_code))]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn is_graphics(&self) -> bool {
        self.graphics
    }

    /// The zoom level as `(up, down)`: `up/down` screen units per canvas pixel.
    pub fn zoom(&self) -> (i32, i32) {
        ZOOMS[self.zoom]
    }

    /// The zoom level's index in `ZOOMS`.
    pub fn zoom_index(&self) -> usize {
        self.zoom
    }

    /// The canvas pixel at the top left of the view.
    pub fn view(&self) -> (i32, i32) {
        self.view
    }

    pub fn step_size(&self) -> i32 {
        self.step
    }

    /// The colour of swatch `slot` (0 to 16).
    pub fn color_of(&self, slot: u8) -> u32 {
        match VGA.get(slot as usize) {
            Some(&rgb) => rgb,
            None => self.custom,
        }
    }

    /// The file name shown in the panel.
    pub fn name(&self) -> &str {
        match &self.path {
            Some(path) => path.rsplit('/').next().unwrap_or(path),
            None => "untitled.bmp",
        }
    }

    // --- The screen and the canvas on it ---

    /// A screen of `cols` x `rows` text cells (`cell` pixels each, `graphics`: true color pixels
    /// are drawn). Everything has to be drawn again. The zoom is the fit unless the user chose
    /// one; that is kept (text mode clamps it to x8).
    pub fn set_screen(&mut self, cols: usize, rows: usize, graphics: bool, cell: (usize, usize)) {
        self.cols = cols;
        self.rows = rows;
        self.graphics = graphics;
        self.cell = (cell.0.max(1), cell.1.max(1));
        if !self.zoom_set {
            self.zoom = self.default_zoom();
        }
        self.zoom = self.zoom.min(self.max_zoom());
        self.clamp_view();
        self.ensure_visible();
        self.shown.clear();
        self.px_full = true;
    }

    /// True on a screen below the 40x12 minimum.
    pub fn too_small(&self) -> bool {
        self.cols < MIN_COLS || self.rows < MIN_ROWS
    }

    /// Rows above the bar.
    pub fn work_rows(&self) -> usize {
        self.rows.saturating_sub(1)
    }

    /// The first text column of the panel.
    pub fn panel_col(&self) -> usize {
        self.cols.saturating_sub(PANEL_W)
    }

    /// The panel's rows for this screen.
    pub fn table(&self) -> Vec<Row> {
        layout::panel_rows(self.work_rows())
    }

    /// The canvas area in "area units": half-block columns by pixel rows in text mode (two per
    /// text row), screen pixels in graphics modes.
    pub fn area(&self) -> (i32, i32) {
        let cells = self.panel_col();
        if self.graphics {
            ((cells * self.cell.0) as i32, (self.work_rows() * self.cell.1) as i32)
        } else {
            (cells as i32, 2 * self.work_rows() as i32)
        }
    }

    /// The zoom as `(up, down)`.
    fn z(&self) -> (i32, i32) {
        ZOOMS[self.zoom]
    }

    /// The largest zoom level of the screen: x16, or x8 in text mode.
    fn max_zoom(&self) -> usize {
        if self.graphics {
            ZOOMS.len() - 1
        } else {
            ZOOM_TEXT_MAX
        }
    }

    fn canvas_size(&self) -> (i32, i32) {
        (self.img.width as i32, self.img.height as i32)
    }

    /// How much of the canvas is visible: whole canvas pixels when magnified.
    pub fn visible(&self) -> (i32, i32) {
        self.visible_at(self.zoom)
    }

    fn visible_at(&self, level: usize) -> (i32, i32) {
        let (aw, ah) = self.area();
        let (w, h) = self.canvas_size();
        let z = ZOOMS[level];
        (visible_len(w, aw, z), visible_len(h, ah, z))
    }

    /// The area units the visible part of the canvas covers.
    pub fn canvas_extent(&self) -> (i32, i32) {
        let (vw, vh) = self.visible();
        let z = self.z();
        (extent(vw, z), extent(vh, z))
    }

    /// Where the canvas's top left corner is in the area (centred if all of it is visible; in
    /// text mode on a whole text row).
    pub fn canvas_origin(&self) -> (i32, i32) {
        let (aw, ah) = self.area();
        let (w, h) = self.canvas_size();
        let z = self.z();
        let oy = origin_1d(h, ah, z);
        (origin_1d(w, aw, z), if self.graphics { oy } else { oy & !1 })
    }

    /// The text cells (row0, col0, row1, col1, exclusive ends) the canvas covers in text mode.
    pub fn canvas_cells(&self) -> (usize, usize, usize, usize) {
        let (ox, oy) = self.canvas_origin();
        let (ew, eh) = self.canvas_extent();
        let (c0, r0) = (ox as usize, oy as usize / 2);
        (r0, c0, r0 + (eh as usize).div_ceil(2), c0 + ew as usize)
    }

    /// The zoom a new picture or a new display starts at: the largest level that shows the whole
    /// canvas, at most x8 in graphics modes and x1 in text mode; 1/64 if even that does not.
    fn default_zoom(&self) -> usize {
        let cap = if self.graphics { ZOOM_TEXT_MAX } else { ZOOM_ONE };
        (0..=cap).rev().find(|&level| self.visible_at(level) == self.canvas_size()).unwrap_or(0)
    }

    fn clamp_view(&mut self) {
        let (w, h) = self.canvas_size();
        let (vw, vh) = self.visible();
        self.view.0 = self.view.0.clamp(0, (w - vw).max(0));
        self.view.1 = self.view.1.clamp(0, (h - vh).max(0));
    }

    /// Sets the view (kept inside the canvas); true if it moved.
    fn set_view(&mut self, view: (i32, i32)) -> bool {
        let before = self.view;
        self.view = view;
        self.clamp_view();
        if self.view != before {
            self.px_full = true;
        }
        self.view != before
    }

    /// Scrolls so the cursor is in view, with a margin; true if the view moved.
    fn ensure_visible(&mut self) -> bool {
        let (w, h) = self.canvas_size();
        let (vw, vh) = self.visible();
        let z = self.z();
        // The margin is about two screen units, in canvas pixels.
        let unit = (z.1 / z.0).max(1);
        let before = self.view;
        let follow = |view: i32, pos: i32, vis: i32, len: i32| -> i32 {
            if vis >= len {
                return 0;
            }
            let margin = (2 * unit).min(vis / 4);
            let mut view = view;
            if pos < view + margin {
                view = pos - margin;
            } else if pos >= view + vis - margin {
                view = pos - vis + 1 + margin;
            }
            view.clamp(0, len - vis)
        };
        self.view = (follow(self.view.0, self.cx, vw, w), follow(self.view.1, self.cy, vh, h));
        if self.view != before {
            self.px_full = true;
        }
        self.view != before
    }

    /// The area position of a pointer position from a mouse event: (column, pixel row) in text
    /// mode (the upper half of a cell is the even pixel row), pixels in graphics.
    fn area_pos(&self, x: usize, y: usize) -> (i32, i32) {
        if self.graphics {
            (x as i32, y as i32)
        } else {
            let (cw, ch) = self.cell;
            ((x / cw) as i32, (2 * (y / ch) + (y % ch) * 2 / ch) as i32)
        }
    }

    /// The canvas pixel under the pointer at (`x`, `y`) (area pixels, as in mouse events); None
    /// outside the visible canvas, unless `clamp`: then the nearest edge pixel.
    pub fn locate(&self, x: usize, y: usize, clamp: bool) -> Option<(i32, i32)> {
        let (ax, ay) = self.area_pos(x, y);
        let (ox, oy) = self.canvas_origin();
        let (ew, eh) = self.canvas_extent();
        let (w, h) = self.canvas_size();
        let z = self.z();
        let (mut rx, mut ry) = (ax - ox, ay - oy);
        if rx < 0 || ry < 0 || rx >= ew || ry >= eh {
            if !clamp {
                return None;
            }
            rx = rx.clamp(0, ew - 1);
            ry = ry.clamp(0, eh - 1);
        }
        Some(((self.view.0 + sample_off(rx, z)).min(w - 1), (self.view.1 + sample_off(ry, z)).min(h - 1)))
    }

    /// What a click at text cell (`row`, `col`) hits on the panel.
    pub fn panel_hit(&self, row: usize, col: usize) -> Option<Target> {
        if col < self.panel_col() || self.prompt.is_some() {
            return None;
        }
        layout::hit(&self.table(), row, col - self.panel_col())
    }

    /// The cursor's footprint (the brush), clipped to the canvas.
    fn footprint(&self) -> Rect {
        bmp::brush_rect(self.cx, self.cy, self.brush).clip_to(self.img.width, self.img.height)
    }

    /// Marks a canvas rectangle as needing to be drawn again.
    fn add_dirty(&mut self, r: Rect) {
        self.dirty = self.dirty.union(r);
    }

    /// Whether the inverted cursor is drawn now: in its "on" blink phase, and not while a mouse
    /// button draws with the pencil or eraser (the pointer marks the place, and the inverted
    /// pixel under it would hide the colour just drawn).
    pub fn cursor_lit(&self) -> bool {
        self.blink && !self.stroke.as_ref().is_some_and(|s| s.button.is_some())
    }

    /// The cursor's footprint in area units: (x0, y0, x1, y1), exclusive ends, at least one unit
    /// each way. This is what the blinking cursor inverts.
    pub fn cursor_box(&self) -> (i32, i32, i32, i32) {
        let f = self.footprint();
        let (ox, oy) = self.canvas_origin();
        let z = self.z();
        let x0 = ox + unit_of(f.x - self.view.0, z);
        let y0 = oy + unit_of(f.y - self.view.1, z);
        let x1 = (ox + extent(f.x + f.w - self.view.0, z)).max(x0 + 1);
        let y1 = (oy + extent(f.y + f.h - self.view.1, z)).max(y0 + 1);
        (x0, y0, x1, y1)
    }

    /// The cursor's rectangle with a ring (the ring frames a small cursor): one canvas pixel
    /// wider on each side when magnified, `down` pixels reduced; that is one screen unit or
    /// more either way.
    fn cursor_area(&self) -> Rect {
        let f = self.footprint();
        let z = self.z();
        let pad = (z.1 / z.0).max(1);
        Rect::new(f.x - pad, f.y - pad, f.w + 2 * pad, f.h + 2 * pad).clip_to(self.img.width, self.img.height)
    }
}
