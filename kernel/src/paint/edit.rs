//! What paint does to the canvas: the tools, undo and redo, saving and loading. Every allocation
//! that grows with the canvas (snapshots, the canvas itself) is checked with `picture::fits`
//! first, because a failed allocation panics the kernel; when memory is short the action still
//! happens but cannot be undone ("undo off").

use super::*;

/// `path` with `.bmp` added unless it already ends in it (any case).
pub(super) fn with_extension(path: &str) -> String {
    if path.to_ascii_lowercase().ends_with(".bmp") {
        String::from(path)
    } else {
        format!("{path}.bmp")
    }
}

/// The box with corners `a` and `b`, `pad` pixels larger on every side (not clipped).
fn bbox(a: (i32, i32), b: (i32, i32), pad: i32) -> Rect {
    let r = Rect::from_corners(a.0, a.1, b.0, b.1);
    Rect::new(r.x - pad, r.y - pad, r.w + 2 * pad, r.h + 2 * pad)
}

fn contains(outer: Rect, inner: Rect) -> bool {
    !inner.is_empty() && outer.contains(inner.x, inner.y) && outer.contains(inner.x + inner.w - 1, inner.y + inner.h - 1)
}

fn bytes_of(r: Rect) -> usize {
    r.w.max(0) as usize * r.h.max(0) as usize * 4
}

/// The part `to` of `data`, which holds the pixels of `from` row by row (`to` lies inside).
fn crop(data: &[u32], from: Rect, to: Rect) -> Vec<u32> {
    let mut out = Vec::with_capacity(to.w.max(0) as usize * to.h.max(0) as usize);
    for y in 0..to.h {
        let start = ((to.y - from.y + y) * from.w + (to.x - from.x)) as usize;
        if let Some(row) = data.get(start..start + to.w as usize) {
            out.extend_from_slice(row);
        }
    }
    out
}

impl Paint {
    // --- Snapshots and the undo history ---

    /// The pixels of `r` (already clipped to the canvas), if there is memory for them.
    fn snapshot(&self, r: Rect) -> Option<Vec<u32>> {
        picture::fits(bytes_of(r)).then(|| self.img.copy_rect(r))
    }

    /// No more undo: the history is thrown away, because a change that was not recorded would
    /// make the older steps wrong.
    fn drop_history(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.message = String::from(" Undo off: not enough memory to keep this change");
    }

    fn push_step(&mut self, step: Step) {
        self.redo.clear();
        let budget = undo_budget();
        if step.data.len() * 4 > budget {
            self.undo.clear();
            self.message = String::from(" Undo off: that change is too big to keep");
            return;
        }
        self.undo.push(step);
        while self.undo.len() > UNDO_STEPS || self.undo.iter().map(|s| s.data.len() * 4).sum::<usize>() > budget {
            self.undo.remove(0);
        }
    }

    /// Starts the undo snapshot of something about to change `around` (and, as it grows, more).
    fn begin_pending(&mut self, around: Rect) {
        let (w, h) = (self.img.width, self.img.height);
        let r = around.clip_to(w, h);
        let mut pending = Pending { rect: Rect::EMPTY, data: Vec::new(), dirty: Rect::EMPTY, recording: true };
        if !r.is_empty() {
            let grown = Rect::new(r.x - SNAP_MARGIN, r.y - SNAP_MARGIN, r.w + 2 * SNAP_MARGIN, r.h + 2 * SNAP_MARGIN).clip_to(w, h);
            match self.snapshot(grown).map(|d| (grown, d)).or_else(|| self.snapshot(r).map(|d| (r, d))) {
                Some((rect, data)) => {
                    pending.rect = rect;
                    pending.data = data;
                }
                None => {
                    pending.recording = false;
                    self.drop_history();
                }
            }
        }
        self.pending = Some(pending);
    }

    /// Makes sure the snapshot of the stroke in progress covers `r` (before it is drawn).
    fn cover(&mut self, r: Rect) {
        let (w, h) = (self.img.width, self.img.height);
        let r = r.clip_to(w, h);
        let Some(p) = self.pending.as_ref() else { return };
        if r.is_empty() || !p.recording || contains(p.rect, r) {
            return;
        }
        let old = p.rect;
        let union = old.union(r);
        // Grow in big steps, so a long stroke does not copy at every move.
        let g = SNAP_MARGIN.max(union.w.max(union.h) / 4);
        let grown = Rect::new(union.x - g, union.y - g, union.w + 2 * g, union.h + 2 * g).clip_to(w, h);
        let made = self.snapshot(grown).map(|d| (grown, d)).or_else(|| self.snapshot(union).map(|d| (union, d)));
        let Some(p) = self.pending.as_mut() else { return };
        let Some((rect, mut data)) = made else {
            p.recording = false;
            self.drop_history();
            return;
        };
        // What the canvas looked like before the stroke goes over the part that already changed.
        for row in 0..old.h {
            let dst = ((old.y - rect.y + row) * rect.w + (old.x - rect.x)) as usize;
            let src = (row * old.w) as usize;
            if let (Some(to), Some(from)) = (data.get_mut(dst..dst + old.w as usize), p.data.get(src..src + old.w as usize)) {
                to.copy_from_slice(from);
            }
        }
        p.rect = rect;
        p.data = data;
    }

    /// Ends the snapshot: what the stroke changed becomes an undo step.
    fn finish_pending(&mut self) {
        let Some(p) = self.pending.take() else { return };
        if p.dirty.is_empty() {
            return;
        }
        self.modified = true;
        if !p.recording {
            return;
        }
        let dirty = p.dirty.clip_to(self.img.width, self.img.height);
        let step = if dirty == p.rect || !picture::fits(bytes_of(dirty)) || !contains(p.rect, dirty) {
            Step { rect: p.rect, data: p.data }
        } else {
            Step { rect: dirty, data: crop(&p.data, p.rect, dirty) }
        };
        self.push_step(step);
    }

    /// Notes that `changed` was drawn.
    fn note(&mut self, changed: Rect) {
        if let Some(p) = self.pending.as_mut() {
            p.dirty = p.dirty.union(changed);
        }
        self.add_dirty(changed);
    }

    /// Swaps a step's pixels with the canvas's; the step that undoes it, or the step back if
    /// there is no memory for that.
    fn swap_step(&mut self, step: Step) -> Result<Step, Step> {
        if !picture::fits(step.data.len() * 4) {
            return Err(step);
        }
        let current = self.img.copy_rect(step.rect);
        self.img.paste_rect(step.rect, &step.data);
        self.add_dirty(step.rect);
        Ok(Step { rect: step.rect, data: current })
    }

    pub(super) fn undo(&mut self) {
        self.settle();
        let Some(step) = self.undo.pop() else {
            self.message = String::from(" Nothing to undo");
            return;
        };
        match self.swap_step(step) {
            Ok(inverse) => {
                self.redo.push(inverse);
                self.modified = true;
            }
            Err(step) => {
                self.undo.push(step);
                self.message = String::from(" Error: not enough memory to undo");
            }
        }
    }

    pub(super) fn redo(&mut self) {
        self.settle();
        let Some(step) = self.redo.pop() else {
            self.message = String::from(" Nothing to redo");
            return;
        };
        match self.swap_step(step) {
            Ok(inverse) => {
                self.undo.push(inverse);
                self.modified = true;
            }
            Err(step) => {
                self.redo.push(step);
                self.message = String::from(" Error: not enough memory to redo");
            }
        }
    }

    // --- Moving and the pen ---

    /// Ends what is in progress: lifts the pen (its stroke is kept) and cancels a shape.
    pub(super) fn settle(&mut self) {
        if self.stroke.is_some() {
            self.stroke_end();
        }
        if self.shape.is_some() {
            self.shape_cancel(true);
        }
    }

    pub(super) fn stroke_color_of(&self, button: Option<Button>) -> u32 {
        if self.tool == Tool::Eraser {
            self.color_of(self.bg)
        } else if button == Some(Button::Right) {
            self.color_of(self.bg)
        } else {
            self.color_of(self.fg)
        }
    }

    fn stamp(&mut self, x: i32, y: i32, color: u32) {
        self.cover(bmp::brush_rect(x, y, self.brush));
        let changed = self.img.dot(x, y, self.brush, color);
        self.note(changed);
    }

    fn draw_line(&mut self, a: (i32, i32), b: (i32, i32), color: u32) {
        self.cover(bbox(a, b, self.brush as i32));
        let changed = self.img.line(a.0, a.1, b.0, b.1, self.brush, color);
        self.note(changed);
    }

    pub(super) fn stroke_start(&mut self, at: (i32, i32), color: u32, button: Option<Button>) {
        self.begin_pending(bmp::brush_rect(at.0, at.1, self.brush));
        self.stamp(at.0, at.1, color);
        self.stroke = Some(Stroke { last: at, color, button });
        // The cursor is not drawn during a mouse stroke (`cursor_lit`): draw its area again.
        let cursor = self.cursor_area();
        self.add_dirty(cursor);
    }

    pub(super) fn stroke_end(&mut self) {
        self.stroke = None;
        self.finish_pending();
        let cursor = self.cursor_area();
        self.add_dirty(cursor);
    }

    /// Moves the cursor to a canvas pixel (kept inside the canvas). With the pen down this draws
    /// the line from the last point; with a shape in progress it updates the preview.
    pub(super) fn move_to(&mut self, x: i32, y: i32) {
        self.move_cursor(x, y, true);
    }

    /// `move_to`; `follow`: the view scrolls to keep the cursor in sight (the keys do, the mouse
    /// does not: the pointer is over a visible pixel, and hovering must not scroll).
    pub(super) fn move_cursor(&mut self, x: i32, y: i32, follow: bool) {
        let (w, h) = self.canvas_size();
        let (x, y) = (x.clamp(0, w - 1), y.clamp(0, h - 1));
        if (x, y) == (self.cx, self.cy) {
            return;
        }
        let before = self.cursor_area();
        self.add_dirty(before);
        self.cx = x;
        self.cy = y;
        if let Some(s) = &self.stroke {
            let (last, color) = (s.last, s.color);
            self.draw_line(last, (x, y), color);
            if let Some(s) = self.stroke.as_mut() {
                s.last = (x, y);
            }
        }
        if self.shape.is_some() {
            self.shape_update();
        }
        let after = self.cursor_area();
        self.add_dirty(after);
        if follow {
            self.ensure_visible();
        }
    }

    // --- Shapes ---

    fn draw_shape(&mut self, a: (i32, i32), b: (i32, i32), color: u32) -> Rect {
        let (brush, fill) = (self.brush, self.fill);
        match self.tool {
            Tool::Line => self.img.line(a.0, a.1, b.0, b.1, brush, color),
            Tool::Rect => self.img.rect(a.0, a.1, b.0, b.1, brush, fill, color),
            Tool::Ellipse => self.img.ellipse(a.0, a.1, b.0, b.1, brush, fill, color),
            _ => Rect::EMPTY,
        }
    }

    pub(super) fn shape_start(&mut self, start: (i32, i32), color: u32, button: Option<Button>) {
        self.shape = Some(Shape { start, color, button, snap: None });
        self.shape_update();
    }

    /// Takes the preview out of the canvas.
    fn shape_unpreview(&mut self) {
        let Some(snap) = self.shape.as_mut().and_then(|s| s.snap.take()) else { return };
        self.img.paste_rect(snap.rect, &snap.data);
        self.add_dirty(snap.rect);
    }

    /// Draws the preview from the shape's start to the cursor.
    pub(super) fn shape_update(&mut self) {
        self.shape_unpreview();
        let Some(s) = &self.shape else { return };
        let (start, color) = (s.start, s.color);
        let area = bbox(start, (self.cx, self.cy), self.brush as i32).clip_to(self.img.width, self.img.height);
        let Some(data) = self.snapshot(area) else {
            self.message = String::from(" Error: not enough memory for the preview");
            return;
        };
        let changed = self.draw_shape(start, (self.cx, self.cy), color);
        self.add_dirty(area.union(changed));
        if let Some(s) = self.shape.as_mut() {
            s.snap = Some(Snap { rect: area, data });
        }
    }

    pub(super) fn shape_commit(&mut self) {
        self.shape_update();
        let Some(mut shape) = self.shape.take() else { return };
        match shape.snap.take() {
            Some(Snap { rect, data }) => {
                self.push_step(Step { rect, data });
                self.modified = true;
            }
            None => {
                // No memory for the preview: draw it for good, without undo.
                self.drop_history();
                let changed = self.draw_shape(shape.start, (self.cx, self.cy), shape.color);
                self.add_dirty(changed);
                self.modified = true;
            }
        }
    }

    pub(super) fn shape_cancel(&mut self, say: bool) {
        self.shape_unpreview();
        if self.shape.take().is_some() && say {
            self.message = String::from(" Shape cancelled");
        }
    }

    // --- Bucket and picker ---

    pub(super) fn bucket(&mut self, x: i32, y: i32, color: u32) {
        if self.img.get(x, y).is_none_or(|c| c == color) {
            return;
        }
        let whole = Rect::new(0, 0, self.img.width as i32, self.img.height as i32);
        let all = self.snapshot(whole);
        let changed = match self.img.try_flood_fill(x, y, color) {
            Ok(changed) => changed,
            Err(partial) => {
                // The fill's work list could not grow. With the snapshot the canvas goes back
                // as it was; without it the part that was filled stays, and cannot be undone.
                if let Some(all) = all {
                    self.img.paste_rect(whole, &all);
                    self.add_dirty(whole);
                    self.message = String::from(" Error: not enough memory to fill");
                } else {
                    if !partial.is_empty() {
                        self.add_dirty(partial);
                        self.modified = true;
                    }
                    self.drop_history();
                    self.message = String::from(" Fill stopped: not enough memory");
                }
                return;
            }
        };
        if changed.is_empty() {
            return;
        }
        self.add_dirty(changed);
        self.modified = true;
        match all {
            Some(all) if picture::fits(bytes_of(changed)) => {
                let data = crop(&all, whole, changed);
                drop(all);
                self.push_step(Step { rect: changed, data });
            }
            _ => self.drop_history(),
        }
    }

    /// Takes the colour under (`x`, `y`) into the foreground (or the background); the picker
    /// then hands over to the tool before it.
    pub(super) fn pick(&mut self, x: i32, y: i32, to_bg: bool) {
        let Some(rgb) = self.img.get(x, y) else { return };
        let slot = match VGA.iter().position(|&c| c == rgb) {
            Some(i) => i as u8,
            None => {
                self.custom = rgb;
                CUSTOM
            }
        };
        if to_bg {
            self.bg = slot;
        } else {
            self.fg = slot;
        }
        self.message = format!(" Picked {rgb:06X}");
        if self.tool == Tool::Picker {
            self.tool = self.prev_tool;
        }
    }

    pub(super) fn set_tool(&mut self, tool: Tool) {
        self.settle();
        if tool != self.tool {
            self.prev_tool = self.tool;
            self.tool = tool;
        }
    }

    // --- Settings ---

    pub(super) fn set_brush(&mut self, bigger: bool) {
        let before = self.cursor_area();
        if bigger && self.brush < MAX_BRUSH {
            self.brush += 1;
            self.message = format!(" Brush {}", self.brush);
        } else if !bigger && self.brush > 1 {
            self.brush -= 1;
            self.message = format!(" Brush {}", self.brush);
        } else if bigger {
            self.message = String::from(" Brush is already at the largest size");
        } else {
            self.message = String::from(" Brush is already at the smallest size");
        }
        self.add_dirty(before);
        let after = self.cursor_area();
        self.add_dirty(after);
    }

    /// Previous or next swatch, wrapping through the 17.
    pub(super) fn cycle(slot: u8, forward: bool) -> u8 {
        let n = CUSTOM + 1;
        if forward {
            (slot + 1) % n
        } else {
            (slot + n - 1) % n
        }
    }

    /// Moves to zoom level `level` (limited to the screen's), keeping `anchor` (a canvas pixel
    /// and the absolute area unit it is shown at) where it is if the canvas has room to scroll.
    pub(super) fn set_zoom(&mut self, level: usize, anchor: ((i32, i32), (i32, i32))) {
        self.zoom = level.min(self.max_zoom());
        let (c, a) = anchor;
        let (ox, oy) = self.canvas_origin();
        let z = self.z();
        self.view = (c.0 - sample_off(a.0 - ox, z), c.1 - sample_off(a.1 - oy, z));
        self.clamp_view();
        self.px_full = true;
    }

    /// Where the keyboard cursor's pixel is shown now: the canvas pixel and its area unit.
    fn cursor_anchor(&self) -> ((i32, i32), (i32, i32)) {
        let (ox, oy) = self.canvas_origin();
        let z = self.z();
        ((self.cx, self.cy), (ox + unit_of(self.cx - self.view.0, z), oy + unit_of(self.cy - self.view.1, z)))
    }

    /// `Zoom x4` and the like, for the bar.
    fn zoom_message(&mut self, suffix: &str) {
        self.message = format!(" Zoom {}{suffix}", zoom_label(self.z()));
    }

    /// PgUp, PgDn: one zoom level in or out, the cursor staying where it is on the screen.
    pub(super) fn zoom_step(&mut self, up: bool) {
        let to = if up { (self.zoom + 1).min(self.max_zoom()) } else { self.zoom.saturating_sub(1) };
        self.zoom_set = true;
        if to == self.zoom {
            let which = if up { "largest" } else { "smallest" };
            self.message = format!(" Already at the {which} zoom ({})", zoom_label(self.z()));
            return;
        }
        let anchor = self.cursor_anchor();
        self.set_zoom(to, anchor);
        self.ensure_visible();
        self.zoom_message("");
    }

    /// `z`: the fit (the start zoom: the whole canvas, at most x8 in graphics, x1 in text mode),
    /// and a change of display may choose the zoom again. `Z`: x1.
    pub(super) fn zoom_fit(&mut self, one: bool) {
        let level = if one { ZOOM_ONE } else { self.default_zoom() };
        self.zoom_set = one;
        let anchor = self.cursor_anchor();
        self.set_zoom(level, anchor);
        self.ensure_visible();
        self.zoom_message(if one { "" } else { " (fit)" });
    }

    /// The wheel: `notches` levels in (negative) or out, about the canvas pixel under the pointer
    /// at (`x`, `y`), or about the middle of the view if the pointer is not over the canvas.
    pub(super) fn wheel_zoom(&mut self, x: usize, y: usize, notches: i8) {
        let n = notches.unsigned_abs() as usize;
        let to = if notches < 0 { (self.zoom + n).min(self.max_zoom()) } else { self.zoom.saturating_sub(n) };
        self.zoom_set = true;
        if to == self.zoom {
            return;
        }
        let z = self.z();
        let anchor = match self.locate(x, y, false) {
            Some(pixel) => (pixel, self.area_pos(x, y)),
            None => {
                let (ox, oy) = self.canvas_origin();
                let (ew, eh) = self.canvas_extent();
                let (mx, my) = (ew / 2, eh / 2);
                ((self.view.0 + sample_off(mx, z), self.view.1 + sample_off(my, z)), (ox + mx, oy + my))
            }
        };
        self.set_zoom(to, anchor);
        self.zoom_message("");
    }

    /// W A S D: the view moves by half of what is visible (at least a pixel) and the cursor by
    /// the same, so it keeps its place on the screen; at a canvas edge the view stops and the
    /// cursor still goes on. The cursor moves like the arrows do (a pen down draws).
    pub(super) fn pan_key(&mut self, sx: i32, sy: i32) {
        let (vw, vh) = self.visible();
        let (dx, dy) = (sx * (vw / 2).max(1), sy * (vh / 2).max(1));
        self.set_view((self.view.0 + dx, self.view.1 + dy));
        // No scrolling to the margin: the cursor keeps its place. Only a cursor that was out of
        // sight is brought back.
        self.move_cursor(self.cx + dx, self.cy + dy, false);
        let (vw, vh) = self.visible();
        let seen = (self.view.0..self.view.0 + vw).contains(&self.cx) && (self.view.1..self.view.1 + vh).contains(&self.cy);
        if !seen {
            self.ensure_visible();
        }
    }

    // --- Files ---

    /// Writes the canvas to `path` (absolute).
    pub(super) fn save_to(&mut self, path: String) {
        let name = String::from(path.rsplit('/').next().unwrap_or(&path));
        match picture::save(&path, &self.img) {
            Ok(_bytes) => {
                self.path = Some(path);
                self.modified = false;
                self.message = format!(" Saved {name} ({}x{})", self.img.width, self.img.height);
            }
            Err(e) => self.message = format!(" Error: cannot save {name}: {e}"),
        }
    }

    /// Ctrl-S / Ctrl-O (`as_new` false) and Ctrl-A: saves, asking for a name when there is none
    /// or when asked to.
    pub(super) fn save_key(&mut self, as_new: bool) {
        self.settle();
        match &self.path {
            Some(path) if !as_new => {
                let path = path.clone();
                self.save_to(path);
            }
            _ => {
                let text = if as_new && self.path.is_some() { String::from(self.name()) } else { String::new() };
                let fresh = !text.is_empty();
                self.prompt = Some(Prompt::new(PromptKind::SaveAs, text, fresh));
            }
        }
    }

    /// A whole new picture in place of the canvas: history, cursor and view start over.
    pub(super) fn replace_image(&mut self, img: Image, path: Option<String>) {
        self.img = img;
        self.path = path;
        self.modified = false;
        self.undo.clear();
        self.redo.clear();
        self.stroke = None;
        self.shape = None;
        self.pending = None;
        self.drag = None;
        self.cx = 0;
        self.cy = 0;
        self.view = (0, 0);
        if !self.zoom_set {
            self.zoom = self.default_zoom();
        }
        self.clamp_view();
        self.dirty = Rect::EMPTY;
        self.px_full = true;
    }

    pub(super) fn load_path(&mut self, name: &str) {
        let path = kfs::resolve(&self.cwd, name);
        match picture::load(&path) {
            Ok(img) => {
                let (w, h) = (img.width, img.height);
                self.replace_image(img, Some(path));
                self.message = format!(" Loaded {name} ({w}x{h})");
            }
            Err(picture::Error::Bmp(bmp::Error::NotBmp)) => {
                self.message = format!(" Error: cannot read {name}: not a BMP picture paint can open");
            }
            Err(e) => self.message = format!(" Error: cannot read {name}: {e}"),
        }
    }

    pub(super) fn new_canvas(&mut self, text: &str) {
        const SIZE_ERROR: &str = " Error: size must be WxH, like 64x40 (up to 4096x4096)";
        let Some((w, h)) = parse_size(text) else {
            self.message = String::from(SIZE_ERROR);
            return;
        };
        match picture::blank(w, h, 0xFFFFFF) {
            Ok(img) => {
                self.replace_image(img, None);
                self.message = format!(" New canvas {w}x{h}");
            }
            Err(picture::Error::Memory(_)) => self.message = String::from(" Error: not enough memory for that size"),
            Err(_) => self.message = String::from(SIZE_ERROR),
        }
    }
}

/// `WxH` (either case of the x): both parts decimal numbers.
pub fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.trim().split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// `RRGGBB` with an optional `#`.
pub fn parse_color(text: &str) -> Option<u32> {
    let digits = text.trim().trim_start_matches('#');
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u32::from_str_radix(digits, 16).ok())?
}
