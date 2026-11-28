//! Paint's keys, prompts and mouse. `handle` is the one entry point: the run loop feeds it every
//! key (mouse events are `Key::Mouse`), and so do the self-tests.

use super::edit::{parse_color, with_extension};
use super::*;
use crate::input::Key;
use crate::mouse::{Action, Event};
use crate::pit;

/// The longest name or text a prompt takes.
const PROMPT_MAX: usize = 47;

impl Paint {
    /// Applies one key or mouse event. Returns false to quit (Ctrl-Q, or `Key::Close`: the task
    /// is being closed, and nothing is saved).
    pub fn handle(&mut self, key: Key) -> bool {
        self.handle_at(key, pit::ticks())
    }

    /// Like `handle`, with the time of the event in timer ticks (for double-clicks).
    pub fn handle_at(&mut self, key: Key, now: u64) -> bool {
        match key {
            Key::Close => return false,
            Key::Mouse(event) => {
                self.mouse_seen = true;
                self.mouse_event(event, now);
                return true;
            }
            _ => {}
        }
        // Any key shows the cursor again, and drops the last message and the quit warning.
        let cursor = self.cursor_area();
        self.add_dirty(cursor);
        self.blink = true;
        let armed = core::mem::take(&mut self.quit_armed);
        self.message.clear();
        if self.too_small() {
            return key != Key::Ctrl(b'q') || self.quit(armed);
        }
        if self.prompt.is_some() {
            self.prompt_key(key);
            return true;
        }
        match key {
            Key::Left => self.arrow(-1, 0),
            Key::Right => self.arrow(1, 0),
            Key::Up => self.arrow(0, -1),
            Key::Down => self.arrow(0, 1),
            Key::Char(b'\t') => self.step = match self.step {
                1 => 8,
                8 => 64,
                _ => 1,
            },
            Key::Char(b'w' | b'W') => self.pan_key(0, -1),
            Key::Char(b'a' | b'A') => self.pan_key(-1, 0),
            Key::Char(b's' | b'S') => self.pan_key(0, 1),
            Key::Char(b'd' | b'D') => self.pan_key(1, 0),
            Key::Char(b'z') => self.zoom_fit(false),
            Key::Char(b'Z') => self.zoom_fit(true),
            Key::Char(b' ') | Key::Enter => self.act(),
            Key::Escape => self.escape(),
            Key::Char(b'[') => self.fg = Self::cycle(self.fg, false),
            Key::Char(b']') => self.fg = Self::cycle(self.fg, true),
            Key::Char(b'{') => self.bg = Self::cycle(self.bg, false),
            Key::Char(b'}') => self.bg = Self::cycle(self.bg, true),
            Key::Char(b'x' | b'X') => core::mem::swap(&mut self.fg, &mut self.bg),
            Key::Char(b'c') => self.colour_prompt(false),
            Key::Char(b'C') => self.colour_prompt(true),
            Key::Char(b'f' | b'F') => self.toggle_fill(),
            Key::Char(b'+' | b'=') => self.set_brush(true),
            Key::Char(b'-' | b'_') => self.set_brush(false),
            Key::Char(c) if Tool::from_letter(c).is_some() => {
                if let Some(tool) = Tool::from_letter(c) {
                    self.set_tool(tool);
                }
            }
            Key::PageUp => self.zoom_step(true),
            Key::PageDown => self.zoom_step(false),
            Key::Ctrl(b'z') => self.undo(),
            Key::Ctrl(b'y') => self.redo(),
            Key::Ctrl(b's' | b'o') => self.save_key(false),
            Key::Ctrl(b'a') => self.save_key(true),
            Key::Ctrl(b'n') => self.new_key(),
            Key::Ctrl(b'l') => self.load_key(),
            Key::Ctrl(b'q') => return self.quit(armed),
            _ => {}
        }
        true
    }

    /// Ctrl-Q: quits at once on a saved picture; otherwise warns, and a second Ctrl-Q quits.
    fn quit(&mut self, armed: bool) -> bool {
        if !self.modified || armed {
            return false;
        }
        self.quit_armed = true;
        self.message = String::from(" Unsaved changes! Press Ctrl-Q again to quit without saving.");
        true
    }

    fn arrow(&mut self, dx: i32, dy: i32) {
        let by = self.step;
        self.move_to(self.cx + dx * by, self.cy + dy * by);
    }

    /// Space or Enter: does what the tool does at the cursor.
    fn act(&mut self) {
        let at = (self.cx, self.cy);
        match self.tool {
            Tool::Pencil | Tool::Eraser => {
                if self.stroke.is_some() {
                    self.stroke_end();
                    self.message = String::from(" Pen up");
                } else {
                    let color = self.stroke_color_of(None);
                    self.stroke_start(at, color, None);
                    self.message = String::from(" Pen down");
                }
            }
            Tool::Line | Tool::Rect | Tool::Ellipse => {
                if self.shape.is_some() {
                    self.shape_commit();
                } else {
                    let color = self.color_of(self.fg);
                    self.shape_start(at, color, None);
                }
            }
            Tool::Bucket => {
                let color = self.color_of(self.fg);
                self.bucket(at.0, at.1, color);
            }
            Tool::Picker => self.pick(at.0, at.1, false),
            Tool::Hand => self.message = String::from(layout::HINT_HAND),
        }
    }

    /// Esc: cancels a shape, or lifts the pen. It never quits.
    fn escape(&mut self) {
        if self.shape.is_some() {
            self.shape_cancel(true);
        } else if self.stroke.is_some() {
            self.stroke_end();
            self.message = String::from(" Pen up");
        }
    }

    fn toggle_fill(&mut self) {
        self.fill = !self.fill;
        self.message = String::from(if self.fill { " Fill on" } else { " Fill off" });
    }

    fn colour_prompt(&mut self, bg: bool) {
        let mut prompt = Prompt::new(PromptKind::Colour, format!("{:06X}", self.custom), true);
        prompt.bg = bg;
        self.prompt = Some(prompt);
    }

    fn new_key(&mut self) {
        self.settle();
        self.prompt = Some(if self.modified {
            Prompt::new(PromptKind::DiscardNew, String::new(), false)
        } else {
            Prompt::new(PromptKind::NewSize, format!("{}x{}", self.img.width, self.img.height), true)
        });
    }

    fn load_key(&mut self) {
        self.settle();
        let kind = if self.modified { PromptKind::DiscardLoad } else { PromptKind::Load };
        self.prompt = Some(Prompt::new(kind, String::new(), false));
    }

    // --- Prompts ---

    fn prompt_key(&mut self, key: Key) {
        let Some(kind) = self.prompt.as_ref().map(|p| p.kind) else { return };
        match kind {
            PromptKind::Overwrite | PromptKind::DiscardNew | PromptKind::DiscardLoad => {
                // Only y goes on; anything else is no.
                if let Some(prompt) = self.prompt.take() {
                    if matches!(key, Key::Char(b'y' | b'Y')) {
                        self.confirmed(prompt);
                    }
                }
            }
            _ => match key {
                Key::Char(c) if (0x20..0x7F).contains(&c) => {
                    if let Some(p) = self.prompt.as_mut() {
                        if p.fresh {
                            p.text.clear();
                            p.fresh = false;
                        }
                        if p.text.len() < PROMPT_MAX {
                            p.text.push(c as char);
                        }
                    }
                }
                Key::Backspace => {
                    if let Some(p) = self.prompt.as_mut() {
                        p.fresh = false;
                        p.text.pop();
                    }
                }
                Key::Enter => {
                    if let Some(prompt) = self.prompt.take() {
                        self.submitted(prompt);
                    }
                }
                Key::Escape | Key::Ctrl(_) => self.prompt = None,
                _ => {}
            },
        }
    }

    /// The answer to a y/n question was yes.
    fn confirmed(&mut self, prompt: Prompt) {
        match prompt.kind {
            PromptKind::Overwrite => self.save_to(prompt.path),
            PromptKind::DiscardNew => {
                let text = format!("{}x{}", self.img.width, self.img.height);
                self.prompt = Some(Prompt::new(PromptKind::NewSize, text, true));
            }
            PromptKind::DiscardLoad => self.prompt = Some(Prompt::new(PromptKind::Load, String::new(), false)),
            _ => {}
        }
    }

    /// A text prompt was answered with Enter.
    fn submitted(&mut self, prompt: Prompt) {
        let text = String::from(prompt.text.trim());
        match prompt.kind {
            PromptKind::SaveAs => {
                if text.is_empty() {
                    return;
                }
                let path = kfs::resolve(&self.cwd, &with_extension(&text));
                if fs::with(|fs| fs.stat(&path)).is_ok() {
                    let shown = String::from(path.rsplit('/').next().unwrap_or(&path));
                    let mut ask = Prompt::new(PromptKind::Overwrite, shown, false);
                    ask.path = path;
                    self.prompt = Some(ask);
                } else {
                    self.save_to(path);
                }
            }
            PromptKind::NewSize => self.new_canvas(&text),
            PromptKind::Colour => match parse_color(&text) {
                Some(rgb) => {
                    self.custom = rgb;
                    if prompt.bg {
                        self.bg = CUSTOM;
                    } else {
                        self.fg = CUSTOM;
                    }
                    self.message = format!(" Colour {rgb:06X} is in the custom slot");
                }
                None => self.message = String::from(" Error: colour must be 6 hex digits, like FF8800"),
            },
            PromptKind::Load => {
                if !text.is_empty() {
                    self.load_path(&text);
                }
            }
            _ => {}
        }
    }

    // --- The mouse ---

    fn mouse_event(&mut self, e: Event, now: u64) {
        if self.too_small() || self.prompt.is_some() {
            // Clicks do nothing while a prompt is open (the panel is drawn dim); a button that
            // was held from before is let go of.
            if e.buttons == 0 {
                self.drag = None;
            }
            return;
        }
        match e.action {
            Action::Move => self.mouse_move(e),
            Action::Down(button) => {
                if self.drag.is_none() {
                    self.message.clear();
                    self.press(e, button, now);
                }
            }
            Action::Up(button) => {
                if self.drag.as_ref().is_some_and(|d| d.button == button) {
                    self.release();
                }
            }
            Action::Wheel(notches) => {
                // Not while a button is held: a stroke or a pan is going on.
                if self.drag.is_none() && e.buttons == 0 {
                    self.wheel_zoom(e.x as usize, e.y as usize, notches);
                }
            }
        }
    }

    fn mouse_move(&mut self, e: Event) {
        let (x, y) = (e.x as usize, e.y as usize);
        match self.drag.as_ref().map(|d| d.kind) {
            // A held button keeps the stroke going even outside the canvas: at its edge.
            Some(DragKind::Canvas) => {
                if let Some((px, py)) = self.locate(x, y, true) {
                    self.move_cursor(px, py, false);
                }
            }
            // A pan: the view follows the pointer; the cursor stays where it is.
            Some(DragKind::Pan) => {
                if let Some(drag) = &self.drag {
                    let ((px, py), (vx, vy)) = drag.anchor;
                    let (ax, ay) = self.area_pos(x, y);
                    let (up, down) = self.z();
                    self.set_view((vx - (ax - px) * down / up, vy - (ay - py) * down / up));
                }
            }
            Some(_) => {}
            // No button: the canvas cursor follows the pointer (the view does not scroll).
            None => {
                if let Some((px, py)) = self.locate(x, y, false) {
                    self.move_cursor(px, py, false);
                }
            }
        }
    }

    fn press(&mut self, e: Event, button: Button, now: u64) {
        let (row, col) = e.cell(self.cell);
        if col >= self.panel_col() {
            self.drag = Some(Drag::new(button, DragKind::Panel));
            if let Some(target) = self.panel_hit(row, col) {
                self.panel_click(target, button, now);
            }
            return;
        }
        // The middle button, and any button with the Hand tool, drag the view.
        if button == Button::Middle || self.tool == Tool::Hand {
            let mut drag = Drag::new(button, DragKind::Pan);
            drag.anchor = (self.area_pos(e.x as usize, e.y as usize), self.view);
            self.drag = Some(drag);
            return;
        }
        let Some((x, y)) = self.locate(e.x as usize, e.y as usize, false) else {
            self.drag = Some(Drag::new(button, DragKind::Ignore));
            return;
        };
        // A press while a shape waits for its second corner commits it, like the second Space.
        if self.shape.is_some() {
            self.move_cursor(x, y, false);
            let color = self.stroke_color_of(Some(button));
            if let Some(shape) = self.shape.as_mut() {
                shape.color = color;
            }
            self.shape_commit();
            self.drag = Some(Drag::new(button, DragKind::Ignore));
            return;
        }
        if self.stroke.is_some() {
            self.stroke_end();
        }
        self.drag = Some(Drag::new(button, DragKind::Canvas));
        self.move_cursor(x, y, false);
        let color = self.stroke_color_of(Some(button));
        match self.tool {
            Tool::Pencil | Tool::Eraser => self.stroke_start((x, y), color, Some(button)),
            Tool::Line | Tool::Rect | Tool::Ellipse => self.shape_start((x, y), color, Some(button)),
            Tool::Bucket => self.bucket(x, y, color),
            Tool::Picker => self.pick(x, y, button == Button::Right),
            Tool::Hand => {}
        }
    }

    /// The button of a drag was let go: a stroke ends and a shape is drawn.
    fn release(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        if drag.kind != DragKind::Canvas {
            return;
        }
        if self.stroke.as_ref().is_some_and(|s| s.button == Some(drag.button)) {
            self.stroke_end();
        }
        if self.shape.as_ref().is_some_and(|s| s.button == Some(drag.button)) {
            self.shape_commit();
        }
    }

    fn panel_click(&mut self, target: Target, button: Button, now: u64) {
        let left = button == Button::Left;
        let right = button == Button::Right;
        match target {
            Target::Tool(tool) => {
                if left {
                    self.set_tool(tool);
                }
            }
            Target::Fill => {
                if left {
                    self.toggle_fill();
                }
            }
            Target::Size => {
                if left || right {
                    self.set_brush(left);
                }
            }
            Target::Swap => {
                if left || right {
                    core::mem::swap(&mut self.fg, &mut self.bg);
                }
            }
            Target::Swatch(slot) => {
                // Two quick left presses on the custom swatch ask for its colour.
                let double = left && self.dbl.press(slot as usize, now);
                if left {
                    self.fg = slot;
                } else if right {
                    self.bg = slot;
                }
                if double && slot == CUSTOM {
                    self.colour_prompt(false);
                }
            }
        }
    }
}
