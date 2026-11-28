//! Paint's screen and its run loop.
//!
//! Every row of the screen is made of cells (canvas, panel, bar) and compared with what it showed
//! last, so only the changed part of a changed row is written (`console::draw_cells_at`); that
//! keeps serial quiet and the screen fast. B-008: writing the whole row of the x:y line at every
//! mouse move rewrote the blank cells under the canvas there, which erased its pixels (black)
//! until they were drawn again, and sent the whole row to serial each time (the lag).
//!
//! * Text mode: the canvas is half-block cells, so it is part of the row's cells and a redraw is
//!   a recomposed row.
//! * Graphics modes: the canvas rows hold blank cells and the picture is true-color pixels drawn
//!   after the cells (`console::draw_pixels`). Writing a cell over pixels erases them, so every
//!   row whose cells were rewritten has its pixels drawn again, and canvas changes redraw only
//!   the changed rectangle.

use super::layout::{self, PANEL_W};
use super::*;
use crate::console;
use crate::input;
use crate::theme::{self, Item};
use crate::{display, mouse, pit, vga};
use alloc::vec;

/// The cursor blinks at this interval.
const BLINK_MS: u64 = 500;
/// A reduced view averages each block over at most this many pixels a side.
const AVERAGE_GRID: i32 = 8;

/// The columns `[first, end)` where two rows of cells differ; None if they are the same. A row
/// never drawn (or of another width) differs everywhere.
pub fn changed_span(old: &[u16], new: &[u16]) -> Option<(usize, usize)> {
    if old.len() != new.len() {
        return (!new.is_empty()).then_some((0, new.len()));
    }
    let first = old.iter().zip(new).position(|(a, b)| a != b)?;
    let last = old.iter().zip(new).rposition(|(a, b)| a != b)?;
    Some((first, last + 1))
}

fn cell(ch: u8, attr: u8) -> u16 {
    vga::cell(ch, attr)
}

fn put(cells: &mut [u16], col: usize, text: &str, attr: u8) {
    for (i, b) in text.bytes().enumerate() {
        if let Some(c) = cells.get_mut(col + i) {
            *c = cell(b, attr);
        }
    }
}

fn rgb_of(c: [u8; 3]) -> u32 {
    (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32
}

/// The colour a cursor pixel is drawn in: the inverse, or black or white where the inverse would
/// hardly differ (middle grays).
fn invert(px: u32) -> u32 {
    let near = |shift: u32| (2 * (px >> shift & 255) as i32 - 255).abs() < 64;
    if near(16) && near(8) && near(0) {
        let lum = 3 * (px >> 16 & 255) + 6 * (px >> 8 & 255) + (px & 255);
        if lum > 1270 {
            0x000000
        } else {
            0xFFFFFF
        }
    } else {
        !px & 0xFF_FFFF
    }
}

impl Paint {
    /// Reads the screen's size and the theme again (after a change of display mode or theme).
    fn refit(&mut self) {
        self.looks = Looks {
            normal: theme::attr(Item::Editor),
            dim: theme::attr(Item::EditorDim),
            status: theme::attr(Item::EditorStatus),
            palette: theme::current().palette,
        };
        self.set_screen(console::cols(), console::rows(), display::is_graphics(), display::cell_px());
    }

    /// Runs until the user quits (Ctrl-Q) or the task is closed, then clears the screen like the
    /// editors. The mouse and its pointer belong to paint meanwhile.
    pub fn run(mut self) {
        let grab = mouse::grab(mouse::Use::Pointer);
        let saved = console::attr();
        console::set_attr(theme::attr(Item::Editor));
        console::clear();
        console::set_cursor_visible(false);
        let mut generation = u64::MAX;
        let mut blink_due = pit::uptime_ms() + BLINK_MS;
        'run: loop {
            if display::generation() != generation {
                generation = display::generation();
                self.refit();
            }
            // Recorded before drawing: a screen shown meanwhile is noticed at the next wait.
            let seen = (console::shows(), generation);
            self.draw(&grab);
            let key = input::read_key_until(|| {
                console::shows() != seen.0 || display::generation() != seen.1 || pit::uptime_ms() >= blink_due
            });
            match key {
                Some(key) => {
                    if !self.handle(key) {
                        break;
                    }
                    // Everything that came meanwhile is handled before the next drawing.
                    while let Some(key) = input::try_read_key() {
                        if !self.handle(key) {
                            break 'run;
                        }
                    }
                    blink_due = pit::uptime_ms() + BLINK_MS;
                }
                None => {
                    if console::shows() != seen.0 {
                        // Another screen's cells covered the pixels.
                        self.px_full = true;
                    }
                    if pit::uptime_ms() >= blink_due {
                        blink_due = pit::uptime_ms() + BLINK_MS;
                        if self.stroke.is_none() {
                            self.blink = !self.blink;
                            let area = self.cursor_area();
                            self.add_dirty(area);
                        }
                    }
                }
            }
        }
        console::set_attr(saved);
        console::clear();
        console::set_cursor_visible(true);
        drop(grab);
    }

    // --- Composing the cells ---

    /// What is drawn now: the geometry every cell of a text row needs.
    fn frame(&self) -> Frame {
        Frame { origin: self.canvas_origin(), extent: self.canvas_extent(), z: self.z(), view: self.view, cursor: self.cursor_box() }
    }

    /// The palette index a half block shows at area unit (`ax`, `ay`): the canvas pixel there in
    /// the nearest theme color, the blank color outside the canvas, and a contrasting one for the
    /// blinking cursor (every unit of its footprint's area rectangle).
    fn half(&self, ax: i32, ay: i32, f: &Frame) -> u8 {
        let (rx, ry) = (ax - f.origin.0, ay - f.origin.1);
        let inside = rx >= 0 && ry >= 0 && rx < f.extent.0 && ry < f.extent.1;
        let px = if inside { self.shown(f.view.0 + sample_off(rx, f.z), f.view.1 + sample_off(ry, f.z), f.z.1) } else { None };
        let Some(px) = px else { return (self.looks.normal >> 4) & 15 };
        let idx = bmp::nearest(&self.looks.palette, px);
        let (x0, y0, x1, y1) = f.cursor;
        if !(self.cursor_lit() && ax >= x0 && ax < x1 && ay >= y0 && ay < y1) {
            return idx;
        }
        let inverted = bmp::nearest(&self.looks.palette, invert(px));
        if inverted != idx {
            return inverted;
        }
        let [r, g, b] = self.looks.palette[idx as usize];
        if 3 * r as u32 + 6 * g as u32 + b as u32 > 1270 {
            0
        } else {
            15
        }
    }

    /// The text-mode canvas cell at column `col` of text row `r`: two area units.
    fn canvas_cell(&self, col: usize, r: usize, f: &Frame) -> u16 {
        let (ax, ay) = (col as i32, 2 * r as i32);
        let inside = |ax: i32, ay: i32| {
            let (rx, ry) = (ax - f.origin.0, ay - f.origin.1);
            rx >= 0 && ry >= 0 && rx < f.extent.0 && ry < f.extent.1
        };
        if !inside(ax, ay) && !inside(ax, ay + 1) {
            return cell(b' ', self.looks.normal);
        }
        bmp::half_block(self.half(ax, ay, f), self.half(ax, ay + 1, f))
    }

    /// A panel row: 14 cells, the separator first.
    fn panel_cells(&self, kind: Row, out: &mut [u16]) {
        let l = self.looks;
        // While a prompt is open nothing in the panel reacts: its targets are drawn dim.
        let live = if self.prompt.is_some() { l.dim } else { l.normal };
        out[0] = cell(0xB3, l.dim);
        let swatch = |slot: u8| -> u8 { bmp::nearest(&l.palette, self.color_of(slot)) << 4 };
        let box2 = |out: &mut [u16], col: usize, attr: u8| {
            put(out, col, "  ", attr);
        };
        match kind {
            Row::Name => {
                let mut name = layout::short_name(self.name());
                if self.modified {
                    name.push('*');
                }
                put(out, 1, &name, l.normal);
            }
            Row::Tools1 | Row::Tools2 => {
                let tools: &[Tool] = if kind == Row::Tools1 {
                    &[Tool::Pencil, Tool::Line, Tool::Rect, Tool::Ellipse]
                } else {
                    &[Tool::Bucket, Tool::Picker, Tool::Eraser, Tool::Hand]
                };
                for (i, &tool) in tools.iter().enumerate() {
                    let letter = tool.letter() as char;
                    if tool == self.tool {
                        put(out, 1 + 3 * i, &format!(">{letter}<"), l.status);
                    } else {
                        put(out, 1 + 3 * i, &format!("[{letter}]"), live);
                    }
                }
            }
            Row::Fill => {
                if self.fill {
                    put(out, 1, "Fill on", l.status);
                } else {
                    put(out, 1, "Fill off", live);
                }
            }
            Row::Size => put(out, 1, &format!("Size {}", self.brush), live),
            Row::Pen => {
                if self.tool.is_pen() {
                    match self.stroke {
                        Some(_) => put(out, 1, "Pen DOWN", l.status),
                        None => put(out, 1, "Pen up", l.normal),
                    }
                } else if self.tool.is_shape() {
                    match self.shape {
                        Some(_) => put(out, 1, "Corner set", l.status),
                        None => put(out, 1, "Corner -", l.normal),
                    }
                }
            }
            Row::Fg | Row::Bg => {
                let (label, slot) = if kind == Row::Fg { ("FG ", self.fg) } else { ("BG ", self.bg) };
                put(out, 1, label, l.dim);
                box2(out, 4, swatch(slot));
                put(out, 7, &format!("{:06X}", self.color_of(slot)), live);
            }
            Row::Palette(r) => {
                for (col, slot) in [(1, r), (5, r + 8)] {
                    let mark = match (self.fg == slot, self.bg == slot) {
                        (true, true) => '*',
                        (true, false) => 'F',
                        (false, true) => 'B',
                        _ => ' ',
                    };
                    put(out, col, &format!("{mark}"), live);
                    box2(out, col + 1, swatch(slot));
                }
            }
            Row::Custom => {
                let mark = match (self.fg == CUSTOM, self.bg == CUSTOM) {
                    (true, true) => '*',
                    (true, false) => 'F',
                    (false, true) => 'B',
                    _ => ' ',
                };
                put(out, 1, &format!("{mark}"), live);
                box2(out, 2, swatch(CUSTOM));
                put(out, 5, "custom", l.dim);
            }
            Row::Xy => put(out, 1, &layout::xy_text(self.cx, self.cy), l.normal),
            Row::Dims => put(out, 1, &format!("{}x{}", self.img.width, self.img.height), l.normal),
            Row::Zoom => put(out, 1, &format!("Zoom {}", zoom_label(self.z())), l.normal),
            Row::View => put(out, 1, &layout::view_text(self.view.0, self.view.1), l.normal),
            Row::Step => put(out, 1, &format!("Step {}", self.step), l.normal),
            Row::Blank => {}
        }
    }

    /// What the bar says: a prompt, else the message, else the hint for the state.
    pub fn bar(&self) -> String {
        let width = self.cols;
        let mut text = if let Some(p) = &self.prompt {
            layout::prompt_text(p.kind, &p.text, &p.text, width)
        } else if !self.message.is_empty() {
            self.message.clone()
        } else if self.shape.as_ref().is_some_and(|s| s.button.is_some()) {
            String::from(layout::HINT_DRAG)
        } else if self.shape.is_some() {
            String::from(layout::HINT_CORNER)
        } else if self.stroke.as_ref().is_some_and(|s| s.button.is_none()) {
            String::from(layout::HINT_PEN_DOWN)
        } else {
            String::from(layout::idle_hint(width, self.mouse_seen))
        };
        // With no room for the name row, the bar tells that the picture changed.
        if self.modified && !self.table().contains(&Row::Name) {
            match text.strip_prefix(' ') {
                Some(rest) => text = format!("*{rest}"),
                None => text.insert(0, '*'),
            }
        }
        text.truncate(width);
        text
    }

    fn bar_cells(&self) -> Vec<u16> {
        let mut cells = vec![cell(b' ', self.looks.status); self.cols];
        put(&mut cells, 0, &self.bar(), self.looks.status);
        cells
    }

    /// The cells of text row `r` as `draw` makes them now (for the self-tests).
    pub fn row_cells_for_test(&self, r: usize) -> Vec<u16> {
        self.row_cells(r, &self.table(), &self.frame())
    }

    /// The cells of text row `r`.
    fn row_cells(&self, r: usize, table: &[Row], frame: &Frame) -> Vec<u16> {
        let mut cells = vec![cell(b' ', self.looks.normal); self.cols];
        let panel = self.panel_col();
        if !self.graphics {
            for col in 0..panel {
                cells[col] = self.canvas_cell(col, r, frame);
            }
        }
        if let (Some(&kind), Some(out)) = (table.get(r), cells.get_mut(panel..panel + PANEL_W)) {
            self.panel_cells(kind, out);
        } else if let Some(sep) = cells.get_mut(panel) {
            *sep = cell(0xB3, self.looks.dim);
        }
        cells
    }

    // --- Drawing ---

    /// Brings the screen up to date: the cells that changed, then (graphics) the pixels.
    fn draw(&mut self, grab: &mouse::Grab) {
        let bar_row = self.work_rows();
        if self.shown.len() != self.rows {
            self.shown = vec![Vec::new(); self.rows];
        }
        if self.too_small() {
            grab.set_no_highlight(None);
            self.draw_too_small();
            return;
        }
        let table = self.table();
        let frame = self.frame();
        // (row, first column, end column) of every part of a row that was written.
        let mut rewritten: Vec<(usize, usize, usize)> = Vec::new();
        for r in 0..=bar_row {
            let cells = if r < bar_row { self.row_cells(r, &table, &frame) } else { self.bar_cells() };
            if let Some((c0, c1)) = changed_span(&self.shown[r], &cells) {
                if self.shown[r].len() == cells.len() {
                    console::draw_cells_at(r, c0, &cells[c0..c1]);
                } else {
                    console::draw_cells(r, &cells, self.looks.normal);
                }
                self.shown[r] = cells;
                rewritten.push((r, c0, c1));
            }
        }
        grab.set_no_highlight(if self.graphics { None } else { Some(self.canvas_cells()) });
        self.draw_prompt_cursor();
        if self.graphics {
            self.draw_pixels(&table, &rewritten);
        } else {
            self.dirty = Rect::EMPTY;
            self.px_full = false;
        }
    }

    fn draw_too_small(&mut self) {
        let bar_row = self.work_rows();
        for r in 0..=bar_row.min(self.rows.saturating_sub(1)) {
            let mut cells = vec![cell(b' ', self.looks.normal); self.cols];
            if r == 0 {
                put(&mut cells, 0, layout::TOO_SMALL, self.looks.normal);
            } else if r == bar_row {
                cells = vec![cell(b' ', self.looks.status); self.cols];
                put(&mut cells, 0, &self.message, self.looks.status);
            }
            if self.shown[r] != cells {
                console::draw_cells(r, &cells, self.looks.normal);
                self.shown[r] = cells;
            }
        }
    }

    /// The hardware cursor sits after what is typed in a prompt, and is hidden otherwise.
    fn draw_prompt_cursor(&mut self) {
        let Some(p) = &self.prompt else {
            if self.cursor_on {
                console::set_cursor_visible(false);
                self.cursor_on = false;
            }
            return;
        };
        let typed = match p.kind {
            PromptKind::SaveAs | PromptKind::NewSize | PromptKind::Colour | PromptKind::Load => {
                layout::prompt_label(p.kind).len() + p.text.len()
            }
            _ => self.bar().len(),
        };
        console::place_cursor(self.work_rows(), typed.min(self.cols.saturating_sub(1)));
        if !self.cursor_on {
            console::set_cursor_visible(true);
            self.cursor_on = true;
        }
    }

    /// The color the area outside the canvas has: the blank color of the editors.
    fn letterbox(&self) -> u32 {
        rgb_of(self.looks.palette[(self.looks.normal >> 4 & 15) as usize])
    }

    fn draw_pixels(&mut self, table: &[Row], rewritten: &[(usize, usize, usize)]) {
        let (aw, ah) = self.area();
        let (cw, ch) = (self.cell.0 as i32, self.cell.1 as i32);
        let mut ok = true;
        if self.px_full {
            ok = self.draw_area(0, 0, aw, ah) && self.draw_swatches(table, None);
            if ok {
                self.px_full = false;
                self.dirty = Rect::EMPTY;
            }
        } else {
            // Cells that were written lost their pixels: the canvas under them (only the columns
            // written), and the swatches of panel rows that were written.
            let mut panel_rows: Vec<usize> = Vec::new();
            for &(r, c0, c1) in rewritten.iter().filter(|&&(r, _, _)| r < self.work_rows()) {
                ok = ok && self.draw_area(c0 as i32 * cw, r as i32 * ch, c1 as i32 * cw, (r as i32 + 1) * ch);
                if c1 > self.panel_col() {
                    panel_rows.push(r);
                }
            }
            ok = ok && self.draw_swatches(table, Some(&panel_rows));
            if ok && !self.dirty.is_empty() {
                if let Some((x0, y0, x1, y1)) = self.dirty_area() {
                    ok = self.draw_area(x0, y0, x1, y1);
                }
                self.dirty = Rect::EMPTY;
            }
        }
        if !ok {
            // Not the screen shown: everything is drawn again when it is.
            self.px_full = true;
        }
    }

    /// The area rectangle (x0, y0, x1, y1) of the visible part of the dirty canvas rectangle.
    pub(super) fn dirty_area(&self) -> Option<(i32, i32, i32, i32)> {
        let d = self.dirty;
        let (vw, vh) = self.visible();
        let (x0, y0) = (d.x.max(self.view.0), d.y.max(self.view.1));
        let x1 = (d.x + d.w).min(self.view.0 + vw);
        let y1 = (d.y + d.h).min(self.view.1 + vh);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let (ox, oy) = self.canvas_origin();
        let z = self.z();
        let (kx0, ky0) = (x0 - self.view.0, y0 - self.view.1);
        let (kx1, ky1) = (x1 - self.view.0, y1 - self.view.1);
        Some((ox + unit_of(kx0, z), oy + unit_of(ky0, z), ox + extent(kx1, z), oy + extent(ky1, z)))
    }

    /// Graphics: the colour boxes of the panel in true color, over their blank cells. `rows`: only
    /// those text rows.
    fn draw_swatches(&self, table: &[Row], rows: Option<&[usize]>) -> bool {
        let (cw, ch) = self.cell;
        for (r, &kind) in table.iter().enumerate() {
            if rows.is_some_and(|rows| !rows.contains(&r)) {
                continue;
            }
            for (col, slot) in layout::swatch_cells(kind) {
                let rgb = match (slot, kind) {
                    (Some(slot), _) => self.color_of(slot),
                    (None, Row::Bg) => self.color_of(self.bg),
                    (None, _) => self.color_of(self.fg),
                };
                if !console::fill_pixels((self.panel_col() + col) * cw, r * ch, 2 * cw, ch, rgb) {
                    return false;
                }
            }
        }
        true
    }

    /// The colors of the area units `x0..x1` along the row that shows canvas pixel row `cy`, into
    /// `base`: the canvas at the zoom (one sample per unit when reduced, a run per pixel when
    /// magnified) and the letterbox around it.
    /// The colour shown for the canvas block of `d` x `d` pixels at (`x`, `y`) (clipped to the
    /// canvas; `d` is the zoom's `down`): the pixel itself at x1 and above; when reduced, the
    /// block's average, so a thin line stays visible (fainter) instead of falling between
    /// samples. Blocks up to 8x8 are averaged exactly, larger ones over an even 8x8 grid of their
    /// pixels, so a unit never costs more than 64 reads. None outside the canvas.
    pub fn shown(&self, x: i32, y: i32, d: i32) -> Option<u32> {
        if d <= 1 {
            return self.img.get(x, y);
        }
        let (w, h) = (self.img.width as i32, self.img.height as i32);
        if x < 0 || y < 0 || x >= w || y >= h {
            return None;
        }
        let (x1, y1) = ((x + d).min(w), (y + d).min(h));
        let step = (d + AVERAGE_GRID - 1) / AVERAGE_GRID;
        let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
        let mut yy = y;
        while yy < y1 {
            let row = (yy * w) as usize;
            let mut xx = x;
            while xx < x1 {
                let p = self.img.pixels.get(row + xx as usize).copied().unwrap_or(0);
                r += p >> 16 & 255;
                g += p >> 8 & 255;
                b += p & 255;
                n += 1;
                xx += step;
            }
            yy += step;
        }
        let avg = |sum: u32| (sum + n / 2) / n;
        Some(avg(r) << 16 | avg(g) << 8 | avg(b))
    }

    fn fill_row(&self, cy: i32, x0: i32, x1: i32, base: &mut Vec<u32>) {
        let z = self.z();
        let (ox, _) = self.canvas_origin();
        let (ew, _) = self.canvas_extent();
        let (cx0, cx1) = (ox, ox + ew);
        let letterbox = self.letterbox();
        base.clear();
        base.resize((x1 - x0).max(0) as usize, letterbox);
        let (xs, xe) = (x0.max(cx0), x1.min(cx1));
        let mut ax = xs;
        while ax < xe {
            let rel = ax - cx0;
            let cx = self.view.0 + sample_off(rel, z);
            // Magnified, a pixel is a run of units; reduced, every unit is a sample.
            let end = if z.1 == 1 { (cx0 + (rel / z.0 + 1) * z.0).min(xe) } else { ax + 1 };
            let color = self.shown(cx, cy, z.1).unwrap_or(letterbox);
            base[(ax - x0) as usize..(end - x0) as usize].fill(color);
            ax = end;
        }
    }

    /// The colors of the area units `x0..x1` of area row `ay` as `draw_area` makes them, without
    /// the cursor (for the self-tests).
    pub fn row_pixels_for_test(&self, ay: i32, x0: i32, x1: i32) -> Vec<u32> {
        let (_, oy) = self.canvas_origin();
        let (_, eh) = self.canvas_extent();
        let mut out = Vec::new();
        if ay < oy || ay >= oy + eh {
            out.resize((x1 - x0).max(0) as usize, self.letterbox());
        } else {
            self.fill_row(self.view.1 + sample_off(ay - oy, self.z()), x0, x1, &mut out);
        }
        out
    }

    /// Graphics: draws the area rectangle (x0, y0) to (x1, y1), exclusive: the canvas pixels at
    /// the zoom (one sample per unit when reduced), the blinking cursor inverted, and the
    /// letterbox around. False if the screen is not the one shown.
    fn draw_area(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> bool {
        let (aw, ah) = self.area();
        let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(aw), y1.min(ah));
        if x0 >= x1 || y0 >= y1 {
            return true;
        }
        let z = self.z();
        let (ox, oy) = self.canvas_origin();
        let (ew, eh) = self.canvas_extent();
        let (cx0, cx1, cy0, cy1) = (ox, ox + ew, oy, oy + eh);
        let letterbox = self.letterbox();
        let width = (x1 - x0) as usize;
        // The cursor's box in area units, with a ring of one unit around one under 4 units.
        let (bx0, by0, bx1, by1) = self.cursor_box();
        let ring = if (bx1 - bx0).min(by1 - by0) < 4 { 1 } else { 0 };
        let (fx0, fx1, fy0, fy1) = (bx0 - ring, bx1 + ring, by0 - ring, by1 + ring);
        let mut base: Vec<u32> = Vec::new();
        let mut out: Vec<u32> = Vec::new();
        let mut base_row = i32::MIN;
        let lit = self.cursor_lit();
        for ay in y0..y1 {
            if ay < cy0 || ay >= cy1 {
                if !console::fill_pixels(x0 as usize, ay as usize, width, 1, letterbox) {
                    return false;
                }
                continue;
            }
            let cy = self.view.1 + sample_off(ay - cy0, z);
            if cy != base_row {
                base_row = cy;
                self.fill_row(cy, x0, x1, &mut base);
            }
            let line: &[u32] = if lit && ay >= fy0 && ay < fy1 {
                out.clear();
                out.extend_from_slice(&base);
                for ax in fx0.max(x0).max(cx0)..fx1.min(x1).min(cx1) {
                    let i = (ax - x0) as usize;
                    out[i] = invert(out[i]);
                }
                &out
            } else {
                &base
            };
            if !console::draw_pixels(x0 as usize, ay as usize, line) {
                return false;
            }
        }
        true
    }
}

/// The geometry a text row is composed from, worked out once per drawing.
struct Frame {
    origin: (i32, i32),
    /// The area units the visible canvas covers.
    extent: (i32, i32),
    z: (i32, i32),
    view: (i32, i32),
    /// The cursor's footprint in area units.
    cursor: (i32, i32, i32, i32),
}
