//! The two picture viewers: `view` (the character viewer) and `gview` (the real-pixel viewer).
//! One module for both: they share the keys, the stepping through a folder's pictures, the
//! status bar and the character renderer.
//!
//! * `view` draws with cells: the half block 0xDF, the upper pixel as the foreground and the lower
//!   as the background (two picture pixels per cell), in the nearest of the theme's 16 colors,
//!   dithered. It looks the same in text mode and in every graphics mode, and serial gets it too.
//! * `gview` draws true-color pixels into the text rows above the status bar (the pixel surface,
//!   `console::draw_pixels`). In text mode it shows the character view instead, with a note.
//!   Serial only gets the status bar. If the display mode changes while it is open it switches
//!   between the two views by itself.
//!
//! The screen is the usual full-screen one: all rows but the last show the picture, the last is
//! the status bar (`editor.status`). Keys: Esc, Q or Ctrl-Q leave; P/N (PgUp/PgDn, and Left/Right
//! while the picture is fitted) show the previous/next `.bmp` of the folder; F toggles fit and
//! actual size; in `gview` `+`/`-` zoom and the arrows pan a picture larger than the area.
//!
//! Everything that can be decided without the screen is a plain function (`place`, `cell_row`,
//! `neighbour`, `bar_text`, `zoom_step`), and `Viewer::handle(Key) -> bool` is driven by the
//! self-tests like the editors' `handle`.

use crate::console::{self, Style};
use crate::input::{self, Key};
use crate::theme::{self, Item};
use crate::{display, fs, picture, pit, vga};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use bmp::Image;
use kfs::FileType;

/// The zoom steps of `gview`, in percent.
pub const ZOOMS: [u32; 6] = [25, 50, 100, 200, 400, 800];

/// How long `gview` shows its text-mode note, unless a key comes first.
const NOTICE_MS: u64 = 5000;

/// What `gview` says in text mode instead of the status bar.
pub const NOTICE: &str = " text mode: use 'display 1024x768' for the real picture";

/// What a picture is scaled to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scale {
    /// The whole picture fits the area (shrunk; in `gview` also enlarged by whole factors).
    Fit,
    /// A percentage of the picture's size (the character view only has 100: actual size).
    Percent(u32),
}

/// Where a scaled picture sits in the drawing area.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    /// The scaled picture's size.
    pub w: usize,
    pub h: usize,
    /// Where it starts inside the area: centered, or 0 if it is larger than the area.
    pub x: usize,
    pub y: usize,
    /// The first visible column and row of the scaled picture (0 if it fits); never past the
    /// end.
    pub pan_x: usize,
    pub pan_y: usize,
}

impl Placement {
    /// The part of the scaled picture that is shown: at most the area's size.
    pub fn visible(&self, area: (usize, usize)) -> (usize, usize) {
        (self.w.min(area.0), self.h.min(area.1))
    }
}

/// The size a picture of `size` gets in `area` (character cells wide by picture pixels high for
/// the character view; pixels for `gview`). `enlarge`: a fitted small picture grows by whole
/// factors (`gview`).
pub fn scaled_size(size: (u32, u32), area: (usize, usize), scale: Scale, enlarge: bool) -> (usize, usize) {
    match scale {
        Scale::Fit => {
            let (w, h) = bmp::fit(size.0, size.1, area.0 as u32, area.1 as u32, enlarge);
            (w as usize, h as usize)
        }
        Scale::Percent(p) => {
            let side = |n: u32| ((n as u64 * p as u64 / 100).max(1)).min(usize::MAX as u64 / 2) as usize;
            (side(size.0), side(size.1))
        }
    }
}

/// Fits a picture to the area: its scaled size, centered if smaller than the area, panned (and
/// the pan kept inside the picture) if larger.
pub fn place(size: (u32, u32), area: (usize, usize), scale: Scale, enlarge: bool, pan: (usize, usize)) -> Placement {
    let (w, h) = scaled_size(size, area, scale, enlarge);
    let axis = |len: usize, room: usize, pan: usize| {
        if len >= room {
            (0, pan.min(len - room))
        } else {
            ((room - len) / 2, 0)
        }
    };
    let (x, pan_x) = axis(w, area.0, pan.0);
    let (y, pan_y) = axis(h, area.1, pan.1);
    Placement { w, h, x, y, pan_x, pan_y }
}

/// The next zoom step above (`zoom_in`) or below `percent`; the end of the list stays.
pub fn zoom_step(percent: u32, zoom_in: bool) -> u32 {
    if zoom_in {
        ZOOMS.iter().copied().find(|&z| z > percent).unwrap_or(ZOOMS[ZOOMS.len() - 1])
    } else {
        ZOOMS.iter().rev().copied().find(|&z| z < percent).unwrap_or(ZOOMS[0])
    }
}

/// The drawing area of a screen with `picture_rows` text rows above the status row and `cols`
/// columns: with `cell` (the cell size in pixels, graphics modes) real pixels, else cells wide by
/// two picture pixel rows per text row. `picture_rows` is the task screen's rows minus the status
/// row, so under the desktop (one row less than the display) it is the display's rows minus 2.
pub fn picture_area(picture_rows: usize, cols: usize, cell: Option<(usize, usize)>) -> (usize, usize) {
    match cell {
        Some((w, h)) => (cols * w, picture_rows * h),
        None => (cols, 2 * picture_rows),
    }
}

/// The percent the character view says a shrunk picture is shown at: `shown` of `full` pixels
/// wide, rounded to the nearest percent, at least 1 and (since it is shrunk) at most 99.
pub fn shrunk_percent(shown: usize, full: u32) -> u32 {
    let full = full.max(1) as u64;
    ((shown as u64 * 100 + full / 2) / full).clamp(1, 99) as u32
}

/// How the character view turns picture colors into cells.
pub struct CellStyle {
    /// The 16 colors as the screen really shows them (the theme's palette).
    pub palette: [[u8; 3]; 16],
    /// The color index of the blank around the picture (the `editor` background).
    pub bg: u8,
    /// The color byte of a blank cell.
    pub blank: u8,
    pub dither: bool,
}

impl CellStyle {
    /// The current theme's `editor` colors and palette.
    pub fn current(dither: bool) -> CellStyle {
        let blank = theme::attr(Item::Editor);
        CellStyle { palette: theme::current().palette, bg: blank >> 4 & 15, blank, dither }
    }
}

/// One row of cells of the character view: cell row `r` shows picture pixel rows `2r` (upper
/// half) and `2r + 1` (lower half) of the area, which is `area.0` cells by `area.1` pixel rows.
/// The result has `area.0` cells. Pixels outside the picture are the blank color (so an odd last
/// pixel row gets it in its lower half); a cell with the same color in both halves is a space.
/// `scratch` is working memory, kept between calls so no buffer is made per row.
pub fn cell_row(img: &Image, p: &Placement, area: (usize, usize), r: usize, st: &CellStyle, scratch: &mut Vec<u32>, out: &mut Vec<u16>) {
    out.clear();
    out.resize(area.0, vga::cell(b' ', st.blank));
    let (vis_w, vis_h) = p.visible(area);
    let rows = [2 * r, 2 * r + 1];
    let shown = |y: usize| y >= p.y && y < p.y + vis_h;
    if vis_w == 0 || (!shown(rows[0]) && !shown(rows[1])) {
        return;
    }
    scratch.clear();
    scratch.resize(2 * vis_w, 0);
    for (half, &y) in rows.iter().enumerate() {
        if shown(y) {
            let source_row = (y - p.y + p.pan_y) as u32;
            bmp::scaled_row(img, p.w as u32, p.h as u32, source_row, p.pan_x as u32, &mut scratch[half * vis_w..(half + 1) * vis_w]);
        }
    }
    for i in 0..vis_w {
        let color = |half: usize| {
            if shown(rows[half]) {
                let (x, y) = ((p.pan_x + i) as u32, (rows[half] - p.y + p.pan_y) as u32);
                bmp::quantize(&st.palette, scratch[half * vis_w + i], x, y, st.dither)
            } else {
                st.bg
            }
        };
        if let Some(cell) = out.get_mut(p.x + i) {
            *cell = bmp::half_block(color(0), color(1));
        }
    }
}

/// The sorted `.bmp` names of a folder listing: byte order, case-sensitive.
pub fn sorted_images(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut images: Vec<String> = names.into_iter().filter(|n| FileType::from_name(n) == FileType::Image).collect();
    images.sort();
    images
}

/// The index in the sorted `names` of the picture after (`forward`) or before `current`, with
/// wrapping. `current` need not be in the list: the neighbour is then the first name above it, or
/// the last one below it. None if the list is empty.
pub fn neighbour(names: &[String], current: &str, forward: bool) -> Option<usize> {
    if names.is_empty() {
        return None;
    }
    if forward {
        Some(names.iter().position(|n| n.as_str() > current).unwrap_or(0))
    } else {
        Some(names.iter().rposition(|n| n.as_str() < current).unwrap_or(names.len() - 1))
    }
}

/// A file name for a status bar: at most 12 characters, the last one `~` if it was cut.
pub fn short_name(name: &str) -> String {
    if name.chars().count() <= 12 {
        String::from(name)
    } else {
        let mut cut: String = name.chars().take(11).collect();
        cut.push('~');
        cut
    }
}

/// What the status bar says.
pub struct BarInfo<'a> {
    pub name: &'a str,
    pub size: (u32, u32),
    /// Its place among the folder's pictures: (number from 1, how many).
    pub pos: Option<(usize, usize)>,
    /// The character view shows a shrunk picture at this percent.
    pub shrunk: Option<u32>,
    pub fitted: bool,
    /// `gview` with real pixels: the zoom percent shown when not fitted.
    pub zoom: u32,
    pub pixel: bool,
    /// `gview` in text mode.
    pub fallback: bool,
}

/// The status bar texts of a picture, longest first (see the design file, section 2).
pub fn bar_candidates(i: &BarInfo) -> Vec<String> {
    let n = short_name(i.name);
    let size = format!("{}x{}", i.size.0, i.size.1);
    let pos = i.pos.map(|(a, b)| format!("{a}/{b}"));
    let join = |fields: &[&str], tail: &str| -> String {
        let mut s = String::from(" ");
        for f in fields {
            s.push_str(f);
            s.push_str("  ");
        }
        s.push_str(tail);
        s
    };
    let pos_s = pos.as_deref();
    let mut out = Vec::new();
    if i.pixel && i.fitted {
        let fields: Vec<&str> = [Some(n.as_str()), Some(size.as_str()), pos_s].into_iter().flatten().collect();
        out.push(join(&fields, "<-/-> image  F actual  +/- zoom  Q quit"));
        out.push(join(&fields, "F actual  +/- zoom  Q quit"));
    } else if i.pixel {
        let zoom = format!("{}% +/-", i.zoom);
        let fields: Vec<&str> = [Some(n.as_str()), pos_s, Some(zoom.as_str())].into_iter().flatten().collect();
        out.push(join(&fields, "arrows pan  P/N image  F fit  Q quit"));
        out.push(join(&fields, "F fit  Q quit"));
    } else {
        let shrunk = i.shrunk.map(|p| format!("shrunk to {p}%"));
        let text_mode = i.fallback.then_some("text mode");
        let (long, mid) = if i.fitted {
            ("<-/-> image  F actual  Q quit", "F actual  Q quit")
        } else {
            ("arrows pan  P/N image  F fit  Q quit", "F fit  Q quit")
        };
        // The shrunk note is the first thing to go when the bar is narrow.
        for with_shrunk in [true, false] {
            if !with_shrunk && shrunk.is_none() {
                break;
            }
            let fields: Vec<&str> = [
                Some(n.as_str()),
                Some(size.as_str()),
                shrunk.as_deref().filter(|_| with_shrunk),
                pos_s,
                text_mode,
            ]
            .into_iter()
            .flatten()
            .collect();
            out.push(join(&fields, long));
            out.push(join(&fields, mid));
        }
    }
    let toggle = if i.fitted { "F actual" } else { "F fit" };
    let fields: Vec<&str> = [Some(n.as_str()), pos_s].into_iter().flatten().collect();
    out.push(join(&fields, &format!("{toggle}  Q quit")));
    out.push(String::from(" Q quit"));
    out
}

/// The longest status bar text that fits in `width` columns.
pub fn bar_text(info: &BarInfo, width: usize) -> String {
    let mut candidates = bar_candidates(info);
    match candidates.iter().position(|c| c.len() <= width) {
        Some(at) => candidates.swap_remove(at),
        None => {
            let mut last = candidates.pop().unwrap_or_default();
            last.truncate(width);
            last
        }
    }
}

/// The status bar of a picture that could not be shown (a bad file reached with P/N).
pub fn bad_bar_text(name: &str, why: &str, width: usize) -> String {
    let full = format!(" {}: {why}. Q quits.", short_name(name));
    if full.len() <= width {
        full
    } else {
        let mut short = String::from(" Q quit");
        short.truncate(width);
        short
    }
}

/// Splits an absolute path into its folder and the file name.
fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some(("", name)) => (String::from("/"), String::from(name)),
        Some((dir, name)) => (String::from(dir), String::from(name)),
        None => (String::from("/"), String::from(path)),
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The sorted picture names in a folder.
fn list_images(dir: &str) -> Vec<String> {
    match fs::with(|fs| fs.read_dir(dir)) {
        Ok(entries) => sorted_images(entries.into_iter().filter(|e| !e.is_dir).map(|e| e.name)),
        Err(_) => Vec::new(),
    }
}

/// The part of the screen that has to be drawn again.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Redraw {
    Nothing,
    Bar,
    All,
}

pub struct Viewer {
    dir: String,
    name: String,
    /// None: the file could not be shown (reached with P/N); `why` says so.
    image: Option<Image>,
    why: String,
    /// The folder's pictures, sorted.
    names: Vec<String>,
    /// Started as `gview`: real pixels in graphics modes.
    gview: bool,
    dither: bool,
    /// Drawing real pixels now (`gview` in a graphics mode).
    pixel: bool,
    scale: Scale,
    pan: (usize, usize),
    /// The drawing area: cells wide by picture pixel rows (2 per cell row) for the character
    /// view, pixels for real pixels.
    area: (usize, usize),
    /// When the text-mode note stops (`pit::uptime_ms`), or 0.
    notice_until: u64,
    /// The cells of the picture rows are as `draw_row` left them (pixel view): only the pixels
    /// have to be drawn again.
    cells_valid: bool,
    /// `display::generation` when the area was last measured.
    generation: u64,
    redraw: Redraw,
}

impl Viewer {
    /// Reads the picture at `path` (absolute) and looks at its folder. The screen is not touched,
    /// so an error can be reported like any command's.
    pub fn open(path: &str, gview: bool, dither: bool) -> Result<Viewer, picture::Error> {
        let image = picture::load(path)?;
        let (dir, name) = split_path(path);
        let names = list_images(&dir);
        Ok(Viewer::with_image(&dir, &name, image, names, gview, dither))
    }

    /// A viewer of a picture already in memory (for the self-tests, and `open`).
    pub fn with_image(dir: &str, name: &str, image: Image, names: Vec<String>, gview: bool, dither: bool) -> Viewer {
        Viewer {
            dir: String::from(dir),
            name: String::from(name),
            image: Some(image),
            why: String::new(),
            names,
            gview,
            dither,
            // Taken as true until the first `refit`, so `gview` in text mode shows its note.
            pixel: true,
            scale: Scale::Fit,
            pan: (0, 0),
            area: (0, 0),
            notice_until: 0,
            cells_valid: false,
            generation: u64::MAX,
            redraw: Redraw::All,
        }
    }

    pub fn is_pixel(&self) -> bool {
        self.pixel
    }

    pub fn scale(&self) -> Scale {
        self.scale
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        self.image.as_ref().map(|i| (i.width, i.height))
    }

    /// Takes the drawing area and whether it is real pixels. Used by `refit`, and by the
    /// self-tests, which have no screen to ask.
    pub fn set_view(&mut self, area: (usize, usize), pixel: bool) {
        self.area = area;
        self.pixel = pixel;
        if !pixel {
            // The character view only has fit and actual size.
            if let Scale::Percent(p) = self.scale {
                if p != 100 {
                    self.scale = Scale::Fit;
                }
            }
        }
        self.pan = self.placement().map_or((0, 0), |p| (p.pan_x, p.pan_y));
        self.cells_valid = false;
        self.redraw = Redraw::All;
    }

    /// How the current picture sits in the area.
    pub fn placement(&self) -> Option<Placement> {
        let img = self.image.as_ref()?;
        Some(place((img.width, img.height), self.area, self.scale, self.pixel, self.pan))
    }

    /// The area and whether it is real pixels, as the screen has them now: the rows of this
    /// task's own screen (all but the status row; under the desktop that screen is one row
    /// shorter than the display) in both views.
    fn measure(&self) -> ((usize, usize), bool) {
        let picture_rows = console::rows().saturating_sub(1);
        let cell = (self.gview && display::is_graphics()).then(display::cell_px);
        (picture_area(picture_rows, console::cols(), cell), cell.is_some())
    }

    /// After a change of display mode or theme: measures the screen again.
    fn refit(&mut self) {
        self.generation = display::generation();
        let was_pixel = self.pixel;
        let (area, pixel) = self.measure();
        self.set_view(area, pixel);
        if self.gview && !pixel && was_pixel {
            self.notice_until = pit::uptime_ms() + NOTICE_MS;
        } else if pixel {
            self.notice_until = 0;
        }
    }

    /// Applies one key. Returns false to quit.
    pub fn handle(&mut self, key: Key) -> bool {
        if key == Key::Close {
            return false;
        }
        if self.notice_until != 0 {
            self.notice_until = 0;
            self.redraw = self.redraw.max(Redraw::Bar);
        }
        let fitted = self.scale == Scale::Fit || self.image.is_none();
        match key {
            Key::Escape | Key::Ctrl(b'q') | Key::Char(b'q' | b'Q') => return false,
            Key::Char(b'f' | b'F') => self.toggle_fit(),
            Key::Char(b'p' | b'P') | Key::PageUp => self.step(false),
            Key::Char(b'n' | b'N') | Key::PageDown => self.step(true),
            Key::Left if fitted => self.step(false),
            Key::Right if fitted => self.step(true),
            Key::Left => self.pan_by(-1, 0),
            Key::Right => self.pan_by(1, 0),
            Key::Up => self.pan_by(0, -1),
            Key::Down => self.pan_by(0, 1),
            Key::Char(b'+' | b'=') if self.pixel => self.zoom(true),
            Key::Char(b'-' | b'_') if self.pixel => self.zoom(false),
            // Mouse events never come (the viewers do not ask for them); anything else is not
            // a viewer key.
            _ => {}
        }
        true
    }

    /// Changes the scale and keeps the middle of what is shown in the middle.
    fn set_scale(&mut self, scale: Scale) {
        let Some(old) = self.placement() else { return };
        let Some(img) = self.image.as_ref() else { return };
        let new = place((img.width, img.height), self.area, scale, self.pixel, (0, 0));
        let pan = |old_pan: usize, old_len: usize, new_len: usize, room: usize| -> usize {
            let centre = (old_pan + old_len.min(room) / 2) as u64;
            let moved = if old_len == 0 { 0 } else { (centre * new_len as u64 / old_len as u64) as usize };
            moved.saturating_sub(new_len.min(room) / 2).min(new_len.saturating_sub(room))
        };
        self.pan = (pan(old.pan_x, old.w, new.w, self.area.0), pan(old.pan_y, old.h, new.h, self.area.1));
        self.scale = scale;
        self.redraw = Redraw::All;
    }

    fn toggle_fit(&mut self) {
        match self.scale {
            Scale::Fit => self.set_scale(Scale::Percent(100)),
            Scale::Percent(_) => self.set_scale(Scale::Fit),
        }
    }

    fn zoom(&mut self, zoom_in: bool) {
        let (Some(p), Some(img)) = (self.placement(), self.image.as_ref()) else { return };
        let now = match self.scale {
            Scale::Percent(p) => p,
            Scale::Fit => (p.w as u64 * 100 / img.width.max(1) as u64) as u32,
        };
        let next = zoom_step(now, zoom_in);
        if Scale::Percent(next) != self.scale {
            self.set_scale(Scale::Percent(next));
        }
    }

    /// Moves the picture under the area: 1/8 of the area per key.
    fn pan_by(&mut self, dx: isize, dy: isize) {
        let Some(p) = self.placement() else { return };
        let step_x = (self.area.0 / 8).max(1) as isize;
        // Whole cells in the character view.
        let step_y = (if self.pixel { (self.area.1 / 8).max(1) } else { (self.area.1 / 8).max(2) & !1 }) as isize;
        let max_x = p.w.saturating_sub(self.area.0) as isize;
        let max_y = p.h.saturating_sub(self.area.1) as isize;
        let pan_x = (p.pan_x as isize + dx * step_x).clamp(0, max_x) as usize;
        let pan_y = (p.pan_y as isize + dy * step_y).clamp(0, max_y) as usize;
        if (pan_x, pan_y) != (p.pan_x, p.pan_y) {
            self.pan = (pan_x, pan_y);
            self.redraw = Redraw::All;
        }
    }

    /// Shows the previous or next picture of the folder.
    fn step(&mut self, forward: bool) {
        self.names = list_images(&self.dir);
        let Some(at) = neighbour(&self.names, &self.name, forward) else { return };
        let name = self.names[at].clone();
        // The old picture goes first, so two pictures never need memory at once.
        self.image = None;
        match picture::load(&join_path(&self.dir, &name)) {
            Ok(image) => {
                self.image = Some(image);
                self.why.clear();
            }
            Err(picture::Error::Bmp(bmp::Error::NotBmp)) => self.why = String::from("not a BMP picture this viewer can show"),
            Err(e) => self.why = format!("{e}"),
        }
        self.name = name;
        // The new picture starts centered.
        self.pan = (0, 0);
        let area = self.area;
        self.pan = self.placement().map_or((0, 0), |p| (p.w.saturating_sub(area.0) / 2, p.h.saturating_sub(area.1) / 2));
        self.cells_valid = false;
        self.redraw = Redraw::All;
    }

    /// The status bar text for the current state.
    pub fn bar(&self, width: usize) -> String {
        if self.notice_until != 0 {
            let mut note = String::from(NOTICE);
            note.truncate(width);
            return note;
        }
        let Some(img) = self.image.as_ref() else { return bad_bar_text(&self.name, &self.why, width) };
        let placement = self.placement();
        let shrunk = match placement {
            Some(p) if !self.pixel && self.scale == Scale::Fit && (p.w < img.width as usize || p.h < img.height as usize) => {
                Some(shrunk_percent(p.w, img.width))
            }
            _ => None,
        };
        let info = BarInfo {
            name: &self.name,
            size: (img.width, img.height),
            pos: self.names.iter().position(|n| *n == self.name).map(|i| (i + 1, self.names.len())),
            shrunk,
            fitted: self.scale == Scale::Fit,
            zoom: if let Scale::Percent(p) = self.scale { p } else { 100 },
            pixel: self.pixel,
            fallback: self.gview && !self.pixel,
        };
        bar_text(&info, width)
    }

    // --- Drawing ---

    /// Runs until the user quits, then clears the screen (like the editors).
    pub fn run(mut self) {
        let saved = console::attr();
        console::set_attr(theme::attr(Item::Editor));
        console::clear();
        console::set_cursor_visible(false);
        loop {
            if display::generation() != self.generation {
                self.refit();
            }
            // Recorded before drawing: a screen shown meanwhile is noticed at the next wait.
            let seen = (console::shows(), display::generation());
            self.paint();
            let until = self.notice_until;
            // Showing any screen draws its cells over our pixels, so the pixels are drawn again;
            // a new mode or theme also moves the area; the note ends by itself.
            let key = input::read_key_until(|| {
                console::shows() != seen.0 || display::generation() != seen.1 || (until != 0 && pit::uptime_ms() >= until)
            });
            match key {
                Some(key) => {
                    if !self.handle(key) {
                        break;
                    }
                }
                None => {
                    if until != 0 && pit::uptime_ms() >= until {
                        self.notice_until = 0;
                        self.redraw = self.redraw.max(Redraw::Bar);
                    }
                    if console::shows() != seen.0 {
                        self.cells_valid = false;
                        // The character view's cells are in its screen: showing it again is
                        // enough. Pixels were covered by the other screen's cells.
                        if self.pixel {
                            self.redraw = Redraw::All;
                        }
                    }
                    // A new generation is handled at the top of the loop.
                }
            }
        }
        console::set_attr(saved);
        console::clear();
        console::set_cursor_visible(true);
    }

    fn paint(&mut self) {
        let what = core::mem::replace(&mut self.redraw, Redraw::Nothing);
        if what == Redraw::Nothing {
            return;
        }
        let rows = console::rows();
        if rows == 0 {
            return;
        }
        let bar_row = rows - 1;
        if what == Redraw::All {
            if self.pixel {
                // Cells first (the picture rows blank in the editor colors), then the pixels.
                if !self.cells_valid {
                    for row in 0..bar_row {
                        console::draw_row(row, b"", Style::Normal);
                    }
                    self.cells_valid = true;
                }
            } else {
                self.paint_cells(bar_row);
            }
        }
        let bar = self.bar(console::cols());
        console::draw_row(bar_row, bar.as_bytes(), Style::Inverse);
        if what == Redraw::All && self.pixel {
            self.paint_pixels();
        }
    }

    /// The character view: every picture row.
    fn paint_cells(&self, bar_row: usize) {
        let style = CellStyle::current(self.dither);
        let placement = self.placement();
        let (mut scratch, mut cells) = (Vec::new(), Vec::new());
        for row in 0..bar_row {
            match (&self.image, &placement) {
                (Some(img), Some(p)) => cell_row(img, p, self.area, row, &style, &mut scratch, &mut cells),
                _ => {
                    cells.clear();
                    cells.resize(self.area.0, vga::cell(b' ', style.blank));
                }
            }
            console::draw_cells(row, &cells, style.blank);
        }
    }

    /// `gview`: true-color pixels into the rows above the bar, the letterbox in the `editor`
    /// background color. Stops if the screen is not the one shown (it draws again when shown).
    fn paint_pixels(&self) {
        // The area the picture was placed in (`measure`), from the top left of the text area.
        let (x, y, w, h) = (0, 0, self.area.0, self.area.1);
        let [r, g, b] = theme::current().palette[(theme::attr(Item::Editor) >> 4 & 15) as usize];
        let letterbox = (r as u32) << 16 | (g as u32) << 8 | b as u32;
        let (Some(img), Some(p)) = (&self.image, self.placement()) else {
            console::fill_pixels(x, y, w, h, letterbox);
            return;
        };
        let (vis_w, vis_h) = p.visible((w, h));
        if vis_w == 0 || vis_h == 0 {
            console::fill_pixels(x, y, w, h, letterbox);
            return;
        }
        let mut line = vec![letterbox; w];
        for row in 0..h {
            if row >= p.y && row < p.y + vis_h {
                // The letterbox at the sides is already in `line`: only the picture changes.
                bmp::scaled_row(img, p.w as u32, p.h as u32, (row - p.y + p.pan_y) as u32, p.pan_x as u32, &mut line[p.x..p.x + vis_w]);
                if !console::draw_pixels(x, y + row, &line) {
                    return;
                }
            } else if !console::fill_pixels(x, y + row, w, 1, letterbox) {
                return;
            }
        }
    }
}
