//! Paint's boot self-tests (run from `selftest::run`). They drive `Paint::handle` on small
//! canvases without a screen; the checks that write files are `qemu-test` only, like the other
//! disk-writing checks.

use super::layout::{self, PromptKind, Row, Target};
use super::{extent, sample_off, unit_of, Paint, Tool, ZOOMS, ZOOM_ONE, ZOOM_TEXT_MAX};
use crate::input::Key;
use crate::mouse::{Action, Button, Event};
use alloc::format;

fn paint_on(w: u32, h: u32) -> Paint {
    Paint::new(bmp::Image::new(w, h, 0xFFFFFF), None, "/")
}

fn keys(p: &mut Paint, keys: &[Key]) {
    for &k in keys {
        assert!(p.handle(k), "paint quit early on {k:?}");
    }
}

fn chars(p: &mut Paint, text: &str) {
    for b in text.bytes() {
        assert!(p.handle(Key::Char(b)), "paint quit early on {}", b as char);
    }
}

/// Moves the canvas cursor by (dx, dy) with arrow keys.
fn step_by(p: &mut Paint, dx: i32, dy: i32) {
    for _ in 0..dx.abs() {
        keys(p, &[if dx > 0 { Key::Right } else { Key::Left }]);
    }
    for _ in 0..dy.abs() {
        keys(p, &[if dy > 0 { Key::Down } else { Key::Up }]);
    }
}

fn count(p: &Paint, rgb: u32) -> usize {
    p.image().pixels.iter().filter(|&&v| v == rgb).count()
}

const BLACK: u32 = 0;
const WHITE: u32 = 0xFFFFFF;

fn c(ch: char) -> Key {
    Key::Char(ch as u8)
}

fn ctrl(ch: char) -> Key {
    Key::Ctrl(ch as u8)
}

pub fn tools() {
    const BLUE: u32 = 0x0000AA;
    let px = |p: &Paint, x: i32, y: i32| p.image().get(x, y).unwrap();

    // Pencil: Space puts the pen down and draws; ten Rights make an 11 pixel line; Space lifts
    // the pen and further moves draw nothing. The whole stroke is one undo step.
    let mut p = paint_on(40, 20);
    keys(&mut p, &[c(' ')]);
    assert!(p.pen_is_down());
    keys(&mut p, &[Key::Right; 10]);
    for x in 0..=10 {
        assert_eq!(px(&p, x, 0), BLACK, "pencil line at x={x}");
    }
    assert_eq!(px(&p, 11, 0), WHITE);
    keys(&mut p, &[c(' ')]);
    assert!(!p.pen_is_down());
    step_by(&mut p, 2, 1);
    assert_eq!(count(&p, BLACK), 11, "pen up draws nothing");
    assert!(p.is_modified());
    assert_eq!(p.undo_depth(), (1, 0));

    // Tab: the step is 8, and the line from the old to the new position leaves no gap; the
    // cursor stops at the canvas edge.
    let mut p = paint_on(40, 20);
    keys(&mut p, &[Key::Down, Key::Down, c(' '), c('\t'), Key::Right]);
    assert_eq!(p.step_size(), 8);
    for x in 0..=8 {
        assert_eq!(px(&p, x, 2), BLACK, "Tab step at x={x}");
    }
    assert_eq!(px(&p, 9, 2), WHITE);
    step_by(&mut p, 10, 0);
    assert_eq!(p.cursor(), (39, 2));
    keys(&mut p, &[c('\t')]);
    assert_eq!(p.step_size(), 64, "Tab goes on to 64");
    keys(&mut p, &[c('\t')]);
    assert_eq!(p.step_size(), 1, "and round to 1");

    // Line: a preview that is not a change, Esc cancels it, Space commits it.
    let mut p = paint_on(40, 20);
    keys(&mut p, &[c('l')]);
    step_by(&mut p, 2, 1);
    keys(&mut p, &[c(' ')]);
    assert!(p.shape_in_progress());
    step_by(&mut p, 5, 3);
    assert_eq!(p.cursor(), (7, 4));
    assert_eq!(px(&p, 7, 4), BLACK, "the preview is drawn");
    assert!(!p.is_modified(), "a preview is no change");
    keys(&mut p, &[Key::Escape]);
    assert!(!p.shape_in_progress() && p.message().contains("cancelled"));
    assert_eq!(count(&p, BLACK), 0, "Esc leaves the canvas as it was");
    assert!(!p.is_modified() && p.undo_depth() == (0, 0));
    keys(&mut p, &[c(' ')]);
    step_by(&mut p, 5, 0);
    keys(&mut p, &[c(' ')]);
    assert!(!p.shape_in_progress() && p.is_modified());
    assert_eq!([px(&p, 7, 4), px(&p, 9, 4), px(&p, 12, 4), px(&p, 13, 4)], [BLACK, BLACK, BLACK, WHITE]);
    keys(&mut p, &[ctrl('z')]);
    assert_eq!(count(&p, BLACK), 0, "undo takes the whole line away");

    // Rectangle: an outline, and solid with F.
    for filled in [false, true] {
        let mut p = paint_on(40, 20);
        keys(&mut p, &[c('r')]);
        if filled {
            keys(&mut p, &[c('f')]);
            assert!(p.fill_on());
        }
        step_by(&mut p, 5, 5);
        keys(&mut p, &[c(' ')]);
        step_by(&mut p, 4, 3);
        keys(&mut p, &[c(' ')]);
        for (x, y) in [(5, 5), (9, 5), (5, 8), (9, 8), (7, 5), (5, 6), (9, 7)] {
            assert_eq!(px(&p, x, y), BLACK, "rectangle edge at {x},{y}");
        }
        assert_eq!(px(&p, 7, 6), if filled { BLACK } else { WHITE }, "rectangle middle, filled={filled}");
        assert_eq!((px(&p, 4, 5), px(&p, 10, 8)), (WHITE, WHITE));
    }

    // Ellipse: touches the middle of each side of its box; the corners and (outline) the middle
    // stay white.
    for filled in [false, true] {
        let mut p = paint_on(40, 20);
        keys(&mut p, &[c('o')]);
        if filled {
            keys(&mut p, &[c('F')]);
        }
        keys(&mut p, &[c(' ')]);
        step_by(&mut p, 8, 8);
        keys(&mut p, &[c(' ')]);
        for (x, y) in [(4, 0), (4, 8), (0, 4), (8, 4)] {
            assert_eq!(px(&p, x, y), BLACK, "ellipse at {x},{y}");
        }
        assert_eq!(px(&p, 0, 0), WHITE, "ellipse corner");
        assert_eq!(px(&p, 4, 4), if filled { BLACK } else { WHITE }, "ellipse middle, filled={filled}");
    }

    // Bucket: fills the closed area only; undo gives it back. Then the picker.
    let mut p = paint_on(40, 20);
    keys(&mut p, &[c('r')]);
    step_by(&mut p, 5, 5);
    keys(&mut p, &[c(' ')]);
    step_by(&mut p, 4, 3);
    keys(&mut p, &[c(' '), c(']'), c('b')]);
    step_by(&mut p, -2, -1);
    keys(&mut p, &[c(' ')]);
    assert_eq!([px(&p, 7, 7), px(&p, 6, 6), px(&p, 5, 5), px(&p, 0, 0)], [BLUE, BLUE, BLACK, WHITE]);
    keys(&mut p, &[ctrl('z')]);
    assert_eq!(px(&p, 7, 7), WHITE);
    step_by(&mut p, -7, -7);
    keys(&mut p, &[c(' ')]);
    assert_eq!([px(&p, 39, 19), px(&p, 7, 7), px(&p, 5, 5)], [BLUE, WHITE, BLACK], "outside only");
    // The picker takes a palette colour into its slot, then the earlier tool is back.
    step_by(&mut p, 5, 5);
    keys(&mut p, &[c(']'), c('i'), c(' ')]);
    assert_eq!(p.tool(), Tool::Bucket);
    assert_eq!(p.colors().0, 0, "picked black");
    // A colour that is not in the palette goes into the custom slot.
    keys(&mut p, &[c('p'), c('c')]);
    chars(&mut p, "3F7A9C");
    keys(&mut p, &[Key::Enter]);
    assert_eq!(p.colors(), (16, 15, 0x3F7A9C));
    step_by(&mut p, 20, 3);
    keys(&mut p, &[c(' '), c(' '), c('x')]);
    assert_eq!(px(&p, 25, 8), 0x3F7A9C);
    keys(&mut p, &[c('['), c('i'), c(' ')]);
    assert_eq!((p.colors().0, p.colors().2, p.tool()), (16, 0x3F7A9C, Tool::Pencil));
    assert!(p.message().contains("3F7A9C"), "{}", p.message());

    // Eraser: the background colour, the same way as the pencil.
    let mut p = paint_on(40, 20);
    keys(&mut p, &[c(' ')]);
    step_by(&mut p, 2, 0);
    keys(&mut p, &[c(' ')]);
    assert_eq!(count(&p, BLACK), 3);
    step_by(&mut p, -2, 0);
    keys(&mut p, &[c('E'), c(' ')]);
    step_by(&mut p, 2, 0);
    keys(&mut p, &[c(' ')]);
    assert_eq!(count(&p, BLACK), 0, "the eraser erased");

    // Colours: [ ] move the foreground, { } the background, both wrap through the 17; X swaps.
    let mut p = paint_on(10, 10);
    assert_eq!(p.colors(), (0, 15, 0x808080));
    keys(&mut p, &[c(']')]);
    assert_eq!(p.colors().0, 1);
    keys(&mut p, &[c('['), c('[')]);
    assert_eq!(p.colors().0, 16, "wraps to the custom slot");
    keys(&mut p, &[c(']')]);
    assert_eq!(p.colors().0, 0);
    keys(&mut p, &[c('}')]);
    assert_eq!(p.colors().1, 16);
    keys(&mut p, &[c('}')]);
    assert_eq!(p.colors().1, 0);
    keys(&mut p, &[c('{'), c('{')]);
    assert_eq!(p.colors().1, 15);
    keys(&mut p, &[c('x')]);
    assert_eq!((p.colors().0, p.colors().1), (15, 0));
    // c and C: the colour prompt; the typed text replaces the suggestion; # is allowed.
    keys(&mut p, &[c('c')]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::Colour));
    chars(&mut p, "FF8800");
    keys(&mut p, &[Key::Enter]);
    assert_eq!(p.prompt_kind(), None);
    assert_eq!(p.colors(), (16, 0, 0xFF8800));
    assert!(p.message().contains("FF8800"), "{}", p.message());
    keys(&mut p, &[c('C')]);
    chars(&mut p, "#00ff00");
    keys(&mut p, &[Key::Enter]);
    assert_eq!(p.colors(), (16, 16, 0x00FF00));
    // A bad entry shows the error and changes nothing.
    keys(&mut p, &[c('c')]);
    chars(&mut p, "zzz");
    keys(&mut p, &[Key::Enter]);
    assert!(p.message().contains("colour must be 6 hex digits"), "{}", p.message());
    assert_eq!(p.colors().2, 0x00FF00);
    // Esc closes a prompt, and never quits.
    keys(&mut p, &[c('c')]);
    assert!(p.handle(Key::Escape) && p.prompt_kind().is_none());
    assert!(p.handle(Key::Escape));

    // Brush: 1 to 8, and a 3x3 dot is clipped at the edge.
    let mut p = paint_on(40, 20);
    chars(&mut p, "++++++++++");
    assert_eq!(p.brush(), 8);
    assert!(p.message().contains("largest"), "{}", p.message());
    chars(&mut p, "----------");
    assert_eq!(p.brush(), 1);
    assert!(p.message().contains("smallest"), "{}", p.message());
    chars(&mut p, "=+_");
    assert_eq!(p.brush(), 2);
    chars(&mut p, "+");
    assert_eq!(p.brush(), 3);
    keys(&mut p, &[c(' '), c(' ')]);
    assert_eq!([px(&p, 0, 0), px(&p, 1, 1), px(&p, 2, 0), px(&p, 0, 2)], [BLACK, BLACK, WHITE, WHITE]);
    assert_eq!(count(&p, BLACK), 4, "a 3x3 dot at the corner is 2x2 on the canvas");
    keys(&mut p, &[c('\t')]);
    step_by(&mut p, 5, 3);
    keys(&mut p, &[c('\t'), c(' '), c(' ')]);
    assert_eq!(p.cursor(), (39, 19));
    assert_eq!([px(&p, 38, 18), px(&p, 39, 19), px(&p, 37, 19)], [BLACK, BLACK, WHITE]);
    assert_eq!(count(&p, BLACK), 8);

    // Undo and redo: one stroke is one step; a new action clears redo; history is 16 deep.
    let mut p = paint_on(40, 20);
    keys(&mut p, &[ctrl('z')]);
    assert!(p.message().contains("Nothing to undo"));
    keys(&mut p, &[c(' ')]);
    step_by(&mut p, 5, 0);
    keys(&mut p, &[c(' ')]);
    assert_eq!((count(&p, BLACK), p.undo_depth()), (6, (1, 0)));
    keys(&mut p, &[ctrl('z')]);
    assert_eq!((count(&p, BLACK), p.undo_depth()), (0, (0, 1)));
    keys(&mut p, &[ctrl('y')]);
    assert_eq!((count(&p, BLACK), p.undo_depth()), (6, (1, 0)));
    keys(&mut p, &[ctrl('z'), c(' '), Key::Down, c(' ')]);
    assert_eq!(p.undo_depth(), (1, 0), "a new action clears redo");
    keys(&mut p, &[ctrl('y')]);
    assert!(p.message().contains("Nothing to redo"));
    for _ in 0..20 {
        keys(&mut p, &[Key::Right, c(' '), c(' ')]);
    }
    assert_eq!(p.undo_depth().0, 16, "at most 16 steps");
    // Esc lifts the pen; the stroke so far stays.
    let mut p = paint_on(40, 20);
    keys(&mut p, &[c(' '), Key::Right, Key::Escape, Key::Right]);
    assert!(!p.pen_is_down());
    assert_eq!(count(&p, BLACK), 2);

    // Ctrl-Q: at once on a saved picture; otherwise a warning first, and any other key takes it
    // back.
    let mut p = paint_on(10, 10);
    assert!(!p.handle(ctrl('q')), "nothing changed: quit at once");
    keys(&mut p, &[c(' '), c(' ')]);
    assert!(p.handle(ctrl('q')) && p.message().contains("Unsaved changes"), "{}", p.message());
    assert!(!p.handle(ctrl('q')), "the second Ctrl-Q quits");
    assert!(p.handle(ctrl('q')) && p.handle(Key::Right) && p.handle(ctrl('q')), "a key in between takes the warning back");
    assert!(!p.handle(Key::Close), "Close quits at once");
}

pub fn panel_layout() {
    let count = |rows: &[Row], kind: Row| rows.iter().filter(|&&r| r == kind).count();

    // The table for the four screens the spec names: 80x25, 80x24 (desktop), 128x47, 40x12.
    for (cols, rows, work, len) in [(80, 25, 24, 24), (80, 24, 23, 23), (128, 47, 46, 25), (40, 12, 11, 11)] {
        let mut p = paint_on(64, 40);
        p.set_screen(cols, rows, false, (8, 16));
        assert_eq!((p.work_rows(), p.table().len(), p.too_small()), (work, len, false), "{cols}x{rows}");
        assert_eq!(p.panel_col(), cols - layout::PANEL_W);
    }
    let full = layout::panel_rows(25);
    assert_eq!(
        full,
        [
            Row::Name,
            Row::Tools1,
            Row::Tools2,
            Row::Fill,
            Row::Size,
            Row::Pen,
            Row::Blank,
            Row::Fg,
            Row::Bg,
            Row::Blank,
            Row::Palette(0),
            Row::Palette(1),
            Row::Palette(2),
            Row::Palette(3),
            Row::Palette(4),
            Row::Palette(5),
            Row::Palette(6),
            Row::Palette(7),
            Row::Custom,
            Row::Blank,
            Row::Xy,
            Row::Dims,
            Row::Zoom,
            Row::View,
            Row::Step
        ]
    );
    assert_eq!(layout::panel_rows(40), full, "a taller screen leaves blank rows at the bottom");
    // What goes first: the three blanks (19, 9, 6), then step, view, canvas size, pen state,
    // x:y, zoom, the name.
    assert_eq!(layout::panel_rows(24).len(), 24);
    assert_eq!(layout::panel_rows(24)[19], Row::Xy);
    assert_eq!(count(&layout::panel_rows(24), Row::Blank), 2);
    assert_eq!(count(&layout::panel_rows(23), Row::Blank), 1);
    assert_eq!(count(&layout::panel_rows(22), Row::Blank), 0);
    for (n, gone) in
        [(21, Row::Step), (20, Row::View), (19, Row::Dims), (18, Row::Pen), (17, Row::Xy), (16, Row::Zoom), (15, Row::Name)]
    {
        let t = layout::panel_rows(n);
        assert_eq!((t.len(), t.contains(&gone)), (n, false), "N={n}");
    }
    // Each is gone only at its own height: Step, View, Dims, Pen, Xy, Zoom, Name in order.
    assert!(layout::panel_rows(22).contains(&Row::Step) && layout::panel_rows(21).contains(&Row::View));
    assert!(layout::panel_rows(20).contains(&Row::Dims) && layout::panel_rows(19).contains(&Row::Pen));
    assert!(layout::panel_rows(18).contains(&Row::Xy) && layout::panel_rows(17).contains(&Row::Zoom));
    assert!(layout::panel_rows(16).contains(&Row::Name));
    // Then the custom row and the palette rows from the bottom.
    assert!(!layout::panel_rows(14).contains(&Row::Custom));
    assert!(layout::panel_rows(14).contains(&Row::Palette(7)));
    assert!(!layout::panel_rows(13).contains(&Row::Palette(7)) && layout::panel_rows(13).contains(&Row::Palette(6)));
    // Every mouse target outlasts every information line.
    for n in 11..=30 {
        let t = layout::panel_rows(n);
        for kind in [Row::Tools1, Row::Tools2, Row::Fill, Row::Size, Row::Fg, Row::Bg, Row::Palette(0), Row::Palette(4)] {
            assert!(t.contains(&kind), "{kind:?} at N={n}");
        }
    }
    assert_eq!(layout::panel_rows(11)[10], Row::Palette(4));

    // Hit tests read the same table (80x24: N=23, two blanks gone).
    let t = layout::panel_rows(23);
    assert_eq!(t[9], Row::Palette(0));
    assert_eq!(t[17], Row::Custom);
    let hit = |row, col| layout::hit(&t, row, col);
    let tool = |tool| Some(Target::Tool(tool));
    assert_eq!(
        [hit(1, 1), hit(1, 3), hit(1, 4), hit(1, 7), hit(1, 10), hit(1, 12)],
        [tool(Tool::Pencil), tool(Tool::Pencil), tool(Tool::Line), tool(Tool::Rect), tool(Tool::Ellipse), tool(Tool::Ellipse)]
    );
    assert_eq!([hit(1, 0), hit(1, 13), hit(2, 13), hit(3, 13), hit(7, 13)], [None; 5]);
    assert_eq!(
        [hit(2, 1), hit(2, 4), hit(2, 7), hit(2, 9), hit(2, 10), hit(2, 12)],
        [tool(Tool::Bucket), tool(Tool::Picker), tool(Tool::Eraser), tool(Tool::Eraser), tool(Tool::Hand), tool(Tool::Hand)]
    );
    assert_eq!([hit(3, 1), hit(3, 12), hit(4, 1), hit(4, 12)], [Some(Target::Fill), Some(Target::Fill), Some(Target::Size), Some(Target::Size)]);
    assert_eq!([hit(7, 3), hit(8, 12), hit(7, 1)], [Some(Target::Swap); 3]);
    assert_eq!(
        [hit(9, 1), hit(9, 4), hit(9, 5), hit(9, 8), hit(9, 9)],
        [Some(Target::Swatch(0)), Some(Target::Swatch(0)), Some(Target::Swatch(8)), Some(Target::Swatch(8)), None]
    );
    assert_eq!([hit(16, 1), hit(16, 5)], [Some(Target::Swatch(7)), Some(Target::Swatch(15))]);
    assert_eq!([hit(17, 1), hit(17, 10), hit(17, 11)], [Some(Target::Swatch(16)), Some(Target::Swatch(16)), None]);
    for row in [0, 5, 6, 18, 19, 20, 21, 22, 23, 50] {
        for col in 0..layout::PANEL_W {
            assert_eq!(hit(row, col), None, "row {row} col {col}");
        }
    }
    // The short table (40x12): the targets moved up, and a click below the table hits nothing.
    let t = layout::panel_rows(11);
    let hit = |row, col| layout::hit(&t, row, col);
    assert_eq!(
        [hit(0, 1), hit(1, 1), hit(2, 1), hit(3, 1), hit(4, 1), hit(5, 1)],
        [tool(Tool::Pencil), tool(Tool::Bucket), Some(Target::Fill), Some(Target::Size), Some(Target::Swap), Some(Target::Swap)]
    );
    assert_eq!([hit(6, 1), hit(10, 5), hit(11, 1)], [Some(Target::Swatch(0)), Some(Target::Swatch(12)), None]);
    // From screen cells: the panel starts at column 66 of 80, 26 of 40; the canvas is not a target.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    assert_eq!([p.panel_hit(1, 67), p.panel_hit(1, 65), p.panel_hit(23, 67)], [tool(Tool::Pencil), None, None]);
    assert_eq!(p.panel_hit(2, 66 + 11), tool(Tool::Hand), "the H button");
    p.set_screen(40, 12, false, (8, 16));
    assert_eq!(p.panel_hit(4, 27), Some(Target::Swap));
    assert_eq!(p.panel_hit(4, 25), None);

    // The panel's texts: the Dims row is only the size, then Zoom and View; x:y and View shorten
    // when the 13 text cells are full.
    let text = |cells: &[u16], from: usize| -> alloc::string::String {
        let s: alloc::string::String = cells[from..].iter().map(|&c| (c & 0xFF) as u8 as char).collect();
        alloc::string::String::from(s.trim_end())
    };
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    let table = p.table();
    let panel = p.panel_col() + 1;
    let row_text = |p: &Paint, kind: Row| {
        let r = table.iter().position(|&k| k == kind).expect("row on 80x24");
        text(&p.row_cells_for_test(r), panel)
    };
    assert_eq!(row_text(&p, Row::Dims), "300x200");
    assert_eq!(row_text(&p, Row::Zoom), "Zoom 1/6", "300x200 fits at 1/6 in 66x46 units");
    assert_eq!(row_text(&p, Row::View), "View 0,0");
    assert_eq!(row_text(&p, Row::Xy), "x:0  y:0");
    keys(&mut p, &[c('Z')]);
    assert_eq!(row_text(&p, Row::Zoom), "Zoom x1");
    keys(&mut p, &[c('d'), c('d')]);
    assert_eq!(row_text(&p, Row::View), "View 66,0");
    assert_eq!(row_text(&p, Row::Xy), "x:66  y:0");
    assert_eq!(layout::xy_text(12, 30), "x:12  y:30");
    assert_eq!(layout::xy_text(4095, 4095), "x:4095 y:4095");
    assert_eq!(layout::xy_text(1000, 4095), "x:1000 y:4095");
    assert_eq!(layout::xy_text(999, 4095), "x:999  y:4095");
    assert_eq!(layout::view_text(0, 0), "View 0,0");
    assert_eq!(layout::view_text(1000, 100), "View 1000,100");
    assert_eq!(layout::view_text(1000, 1000), "V 1000,1000");
    assert_eq!(layout::view_text(4095, 4095), "V 4095,4095");
    assert!(layout::view_text(4095, 4095).len() < layout::PANEL_W && layout::xy_text(4095, 4095).len() < layout::PANEL_W);
    // At 80x20 (N=19) the zoom row is still there but not the view; at 80x14 no information row is.
    p.set_screen(80, 20, false, (8, 16));
    assert!(p.table().contains(&Row::Zoom) && !p.table().contains(&Row::View) && !p.table().contains(&Row::Dims));
    p.set_screen(80, 14, false, (8, 16));
    assert!(![Row::Zoom, Row::View, Row::Dims, Row::Xy, Row::Name, Row::Step].iter().any(|k| p.table().contains(k)));

    // The hint strings (the design file's lengths), and the prompts.
    assert_eq!(layout::idle_hint(80, false).len(), 68);
    assert_eq!(layout::idle_hint(80, true).len(), 68, "the mouse hint with the middle button");
    assert_eq!(layout::idle_hint(67, true).len(), 56, "too narrow for the middle button");
    assert_eq!(layout::idle_hint(55, true).len(), 45, "too narrow for the mouse hint");
    assert_eq!(layout::idle_hint(68, false).len(), 68);
    assert_eq!(layout::idle_hint(67, false).len(), 45);
    assert_eq!(layout::idle_hint(44, false).len(), 28);
    // The widest hints say how to zoom and pan.
    let widest = layout::idle_hint(128, false);
    assert!(widest.contains("PgUp/PgDn zoom") && widest.contains("z fit") && widest.contains("WASD pan"), "{widest}");
    assert_eq!(layout::idle_hint(widest.len(), false), widest);
    assert_ne!(layout::idle_hint(widest.len() - 1, false), widest);
    let wide_mouse = layout::idle_hint(128, true);
    assert!(wide_mouse.contains("Middle pan") && wide_mouse.contains("PgUp/PgDn zoom"), "{wide_mouse}");
    for width in 29..140 {
        for mouse in [false, true] {
            assert!(layout::idle_hint(width, mouse).len() <= width, "{width} {mouse}");
        }
    }
    assert_eq!(layout::prompt_text(PromptKind::SaveAs, "pic", "", 80), " Save as: pic  Enter ok  Esc no");
    assert_eq!(layout::prompt_text(PromptKind::Overwrite, "pic.bmp", "pic.bmp", 80), " pic.bmp exists. Overwrite? (y/n)");
    assert_eq!(layout::prompt_text(PromptKind::NewSize, "64x40", "", 80), " New size WxH: 64x40  Enter ok  Esc no");
    assert_eq!(layout::prompt_text(PromptKind::Colour, "FF8800", "", 80), " Colour RRGGBB: FF8800  Enter ok  Esc no");
    assert_eq!(layout::prompt_text(PromptKind::Load, "a", "", 14), " Load file: a");
    assert_eq!(layout::short_name("a_very_long_file_name.bmp"), "a_very_long~");

    // The bar: the idle hint, the mouse hint once a mouse event came, state hints, and the `*`
    // that moves to the bar when the panel has no room for the name.
    let mut p = paint_on(64, 40);
    assert_eq!(p.bar(), layout::idle_hint(80, false));
    keys(&mut p, &[c(' ')]);
    assert!(p.bar().contains("Pen down"), "{}", p.bar());
    keys(&mut p, &[Key::Right]);
    assert_eq!(p.bar(), layout::HINT_PEN_DOWN);
    keys(&mut p, &[c(' '), Key::Right, c('l'), c(' '), Key::Right]);
    assert_eq!(p.bar(), layout::HINT_CORNER);
    keys(&mut p, &[Key::Escape, Key::Right]);
    assert_eq!(p.bar(), layout::idle_hint(80, false));
    p.handle(Key::Mouse(Event { action: Action::Move, x: 3, y: 3, dx: 1, dy: 1, buttons: 0 }));
    assert_eq!(p.bar(), layout::idle_hint(80, true));
    p.set_screen(80, 14, false, (8, 16));
    assert!(!p.table().contains(&Row::Name));
    assert!(p.bar().starts_with("*Left fg"), "{}", p.bar());
    p.set_screen(80, 24, false, (8, 16));
    assert!(p.bar().starts_with(" Left fg"), "{}", p.bar());
}

pub fn mouse_use() {
    let ev = |action: Action, (x, y): (usize, usize), buttons: u8| {
        Key::Mouse(Event { action, x: x as u16, y: y as u16, dx: 0, dy: 0, buttons })
    };
    let px = |p: &Paint, x: i32, y: i32| p.image().get(x, y).unwrap();
    // The pointer position (area pixels) in the middle of canvas pixel (x, y), as the mouse
    // driver reports it: text mode has a virtual 8x16 per cell, the upper half of a cell is the
    // even pixel row.
    let at = |p: &Paint, x: i32, y: i32| -> (usize, usize) {
        let (ox, oy) = p.canvas_origin();
        let (up, _) = p.zoom();
        let v = p.view();
        // The middle of the block of area units that shows the pixel (magnified).
        let (ax, ay) = (ox + unit_of(x - v.0, p.zoom()) + up / 2, oy + unit_of(y - v.1, p.zoom()) + up / 2);
        pointer_of(p, ax, ay)
    };

    // Text mode, 80x24 (the desktop): the canvas is at column 1, text row 1.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    assert_eq!(p.canvas_origin(), (1, 2));
    assert_eq!(p.locate(8 * 6 + 3, 16 + 2, false), Some((5, 0)), "y 0-7 of a cell: the upper pixel");
    assert_eq!(p.locate(8 * 6 + 3, 16 + 7, false), Some((5, 0)));
    assert_eq!(p.locate(8 * 6 + 3, 16 + 8, false), Some((5, 1)), "y 8-15: the lower pixel");
    assert_eq!(p.locate(8 * 6 + 3, 16 * 20 + 15, false), Some((5, 39)));
    assert_eq!(p.locate(3, 16 + 2, false), None, "column 0 is the letterbox");
    assert_eq!(p.locate(8 * 6, 3, false), None, "above the canvas");
    assert_eq!(p.locate(8 * 65, 16 * 5, false), None, "right of the canvas");
    assert_eq!(p.locate(8 * 70, 3, true), Some((63, 0)), "clamped to the edge");
    // Hovering moves the canvas cursor, and the keys go on from there.
    assert!(p.handle(ev(Action::Move, at(&p, 12, 7), 0)));
    assert_eq!(p.cursor(), (12, 7));
    keys(&mut p, &[Key::Right, Key::Down]);
    assert_eq!(p.cursor(), (13, 8));
    assert!(p.handle(ev(Action::Move, (3, 3), 0)));
    assert_eq!(p.cursor(), (13, 8), "a move outside the canvas is ignored");

    // A left drag with jumps of 20: no gaps, in the foreground colour; one undo step.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    p.handle(ev(Action::Down(Button::Left), at(&p, 2, 5), 1));
    for x in [22, 42] {
        p.handle(ev(Action::Move, at(&p, x, 5), 1));
    }
    p.handle(ev(Action::Up(Button::Left), at(&p, 42, 5), 0));
    for x in 2..=42 {
        assert_eq!(px(&p, x, 5), BLACK, "drag at x={x}");
    }
    assert_eq!((count(&p, BLACK), px(&p, 1, 5), px(&p, 43, 5)), (41, WHITE, WHITE));
    assert_eq!(p.undo_depth(), (1, 0), "press to release is one undo step");
    keys(&mut p, &[Key::Ctrl(b'z')]);
    assert_eq!(count(&p, BLACK), 0);
    // The right button draws in the background colour.
    keys(&mut p, &[c('{')]);
    assert_eq!(p.colors().1, 14);
    p.handle(ev(Action::Down(Button::Right), at(&p, 2, 6), 2));
    p.handle(ev(Action::Move, at(&p, 22, 6), 2));
    p.handle(ev(Action::Up(Button::Right), at(&p, 22, 6), 0));
    assert_eq!([px(&p, 2, 6), px(&p, 12, 6), px(&p, 22, 6)], [0xFFFF55; 3]);
    assert_eq!(p.undo_depth(), (1, 0));
    // The middle button draws nothing (it pans; a canvas that fits has nowhere to go).
    p.handle(ev(Action::Down(Button::Middle), at(&p, 2, 9), 4));
    p.handle(ev(Action::Move, at(&p, 12, 9), 4));
    p.handle(ev(Action::Up(Button::Middle), at(&p, 12, 9), 0));
    assert_eq!((px(&p, 2, 9), px(&p, 12, 9), p.view()), (WHITE, WHITE, (0, 0)));

    // B-007: ONE big move (80 counts at zoom 8 = 10 pixels, and a long one of 40 pixels) draws
    // the whole line including the pixel under the pointer, for the left button, the right
    // button and the eraser; and the inverted cursor is not drawn over the stroke.
    let mut p = paint_on(64, 40);
    p.set_screen(128, 48, true, (8, 16));
    assert_eq!(p.zoom(), (8, 1));
    p.handle(ev(Action::Down(Button::Left), at(&p, 30, 10), 1));
    assert!(!p.cursor_lit(), "no inverted cursor while the mouse draws");
    p.handle(ev(Action::Move, at(&p, 40, 10), 1));
    for x in 30..=40 {
        assert_eq!(px(&p, x, 10), BLACK, "one move: x={x}");
    }
    assert_eq!((count(&p, BLACK), px(&p, 29, 10), px(&p, 41, 10)), (11, WHITE, WHITE));
    p.handle(ev(Action::Up(Button::Left), at(&p, 40, 10), 0));
    assert!(p.cursor_lit(), "the cursor shows again when the button is let go");
    assert_eq!(p.undo_depth(), (1, 0), "the whole drag is one undo step");
    // The longest move the canvas allows, with a slope.
    p.handle(ev(Action::Down(Button::Left), at(&p, 5, 20), 1));
    p.handle(ev(Action::Move, at(&p, 55, 30), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 55, 30), 0));
    assert_eq!((px(&p, 5, 20), px(&p, 55, 30)), (BLACK, BLACK), "both ends of a 50-pixel move");
    assert_eq!(count(&p, BLACK), 11 + 51, "one pixel per column, no gaps");
    assert_eq!(p.undo_depth(), (2, 0));
    keys(&mut p, &[Key::Ctrl(b'z'), Key::Ctrl(b'z')]);
    assert_eq!(count(&p, BLACK), 0);
    // The right button: the background colour, end pixel included.
    keys(&mut p, &[c('{')]);
    p.handle(ev(Action::Down(Button::Right), at(&p, 12, 4), 2));
    p.handle(ev(Action::Move, at(&p, 16, 4), 2));
    p.handle(ev(Action::Up(Button::Right), at(&p, 16, 4), 0));
    assert_eq!([px(&p, 12, 4), px(&p, 14, 4), px(&p, 16, 4), px(&p, 17, 4)], [0xFFFF55, 0xFFFF55, 0xFFFF55, WHITE]);
    assert_eq!(count(&p, 0xFFFF55), 5);
    // The eraser, with either button, over a black bar.
    for (button, bit, y) in [(Button::Left, 1, 20), (Button::Right, 2, 22)] {
        keys(&mut p, &[c('p')]);
        p.handle(ev(Action::Down(Button::Left), at(&p, 10, y), 1));
        p.handle(ev(Action::Move, at(&p, 40, y), 1));
        p.handle(ev(Action::Up(Button::Left), at(&p, 40, y), 0));
        assert_eq!((px(&p, 10, y), px(&p, 40, y)), (BLACK, BLACK));
        keys(&mut p, &[c('e')]);
        p.handle(ev(Action::Down(button), at(&p, 10, y), bit));
        assert!(!p.cursor_lit());
        p.handle(ev(Action::Move, at(&p, 40, y), bit));
        p.handle(ev(Action::Up(button), at(&p, 40, y), 0));
        for x in 10..=40 {
            assert_eq!(px(&p, x, y), 0xFFFF55, "eraser, button {bit}, x={x}");
        }
    }
    assert_eq!(count(&p, BLACK), 0);

    // A stroke that leaves the canvas area is clamped to the edge, and carries on when it returns.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    let row5 = at(&p, 0, 5).1;
    p.handle(ev(Action::Down(Button::Left), at(&p, 60, 5), 1));
    p.handle(ev(Action::Move, (8 * 70 + 3, row5), 1));
    assert_eq!(px(&p, 63, 5), BLACK, "drawn to the edge");
    p.handle(ev(Action::Move, (8 * 75 + 3, row5), 1));
    p.handle(ev(Action::Move, at(&p, 50, 5), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 50, 5), 0));
    assert_eq!([px(&p, 55, 5), px(&p, 50, 5), px(&p, 49, 5)], [BLACK, BLACK, WHITE], "and back");
    assert_eq!(count(&p, BLACK), 14, "60 to 63 and 63 back to 50");
    assert_eq!(p.undo_depth(), (1, 0));
    // A press outside the canvas does nothing.
    p.handle(ev(Action::Down(Button::Left), (3, 40), 1));
    p.handle(ev(Action::Move, at(&p, 20, 20), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 20, 20), 0));
    assert_eq!(count(&p, BLACK), 14);

    // Shapes by dragging: the preview shows, release commits, Esc cancels.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('l')]);
    p.handle(ev(Action::Down(Button::Left), at(&p, 3, 3), 1));
    p.handle(ev(Action::Move, at(&p, 13, 9), 1));
    assert!(p.shape_in_progress() && !p.is_modified());
    assert_eq!(px(&p, 13, 9), BLACK, "the preview");
    assert!(p.bar().contains("Drag: release draws"), "{}", p.bar());
    p.handle(ev(Action::Up(Button::Left), at(&p, 13, 9), 0));
    assert!(!p.shape_in_progress() && p.is_modified());
    assert_eq!([px(&p, 3, 3), px(&p, 13, 9)], [BLACK, BLACK]);
    keys(&mut p, &[Key::Ctrl(b'z')]);
    assert_eq!(count(&p, BLACK), 0);
    p.handle(ev(Action::Down(Button::Left), at(&p, 3, 3), 1));
    p.handle(ev(Action::Move, at(&p, 13, 9), 1));
    keys(&mut p, &[Key::Escape]);
    assert!(!p.shape_in_progress());
    p.handle(ev(Action::Move, at(&p, 20, 9), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 20, 9), 0));
    assert_eq!((count(&p, BLACK), p.undo_depth()), (0, (0, 1)), "Esc cancelled the drag");
    // A filled rectangle, and a right-button ellipse in the background colour.
    keys(&mut p, &[c('r'), c('f')]);
    p.handle(ev(Action::Down(Button::Left), at(&p, 20, 10), 1));
    p.handle(ev(Action::Move, at(&p, 25, 14), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 25, 14), 0));
    assert_eq!((px(&p, 22, 12), px(&p, 26, 12)), (BLACK, WHITE));
    keys(&mut p, &[c('o'), c('{')]);
    p.handle(ev(Action::Down(Button::Right), at(&p, 30, 10), 2));
    p.handle(ev(Action::Move, at(&p, 40, 20), 2));
    p.handle(ev(Action::Up(Button::Right), at(&p, 40, 20), 0));
    assert_eq!(px(&p, 35, 15), 0xFFFF55, "right button: the background colour");
    assert_eq!(p.undo_depth(), (2, 0));
    // A press while a shape waits for its second corner commits it, like the second Space.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('l')]);
    step_by(&mut p, 3, 3);
    keys(&mut p, &[c(' ')]);
    p.handle(ev(Action::Down(Button::Left), at(&p, 13, 9), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 13, 9), 0));
    assert!(!p.shape_in_progress());
    assert_eq!([px(&p, 3, 3), px(&p, 13, 9)], [BLACK, BLACK]);

    // Bucket and picker by clicking, with either button; the eraser with both.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('b'), c('{')]);
    p.handle(ev(Action::Down(Button::Right), at(&p, 5, 5), 2));
    p.handle(ev(Action::Up(Button::Right), at(&p, 5, 5), 0));
    assert_eq!((px(&p, 0, 0), px(&p, 63, 39)), (0xFFFF55, 0xFFFF55), "the right button fills with the background");
    p.handle(ev(Action::Down(Button::Left), at(&p, 5, 5), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 5, 5), 0));
    assert_eq!(px(&p, 0, 0), BLACK, "the left button with the foreground");
    keys(&mut p, &[c('i')]);
    p.handle(ev(Action::Down(Button::Right), at(&p, 9, 9), 2));
    p.handle(ev(Action::Up(Button::Right), at(&p, 9, 9), 0));
    assert_eq!((p.colors().1, p.tool()), (0, Tool::Bucket), "the picker took the background, and the tool is back");
    keys(&mut p, &[c('e'), c('}')]);
    assert_eq!(p.colors().1, 1);
    p.handle(ev(Action::Down(Button::Left), at(&p, 9, 9), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 9, 9), 0));
    assert_eq!(px(&p, 9, 9), 0x0000AA, "the eraser uses the background with either button");

    // Clicks do nothing while a prompt is open.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[Key::Ctrl(b'a')]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::SaveAs));
    p.handle(ev(Action::Down(Button::Left), at(&p, 5, 5), 1));
    p.handle(ev(Action::Move, at(&p, 15, 5), 1));
    p.handle(ev(Action::Up(Button::Left), at(&p, 15, 5), 0));
    p.handle(ev(Action::Down(Button::Left), (8 * 70 + 3, 16 + 3), 1));
    p.handle(ev(Action::Up(Button::Left), (8 * 70 + 3, 16 + 3), 0));
    assert_eq!((count(&p, BLACK), p.prompt_kind(), p.tool()), (0, Some(PromptKind::SaveAs), Tool::Pencil));
    keys(&mut p, &[Key::Escape]);

    // The panel: each tool button, Fill, Size (left +, right -), swatches, swap, custom.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    let mut now = 1000u64;
    let mut click = |p: &mut Paint, row: usize, col: usize, button: Button| {
        let spot = (col * 8 + 3, row * 16 + 3);
        p.handle_at(ev(Action::Down(button), spot, button.bit()), now);
        p.handle_at(ev(Action::Up(button), spot, 0), now);
        now += 100;
    };
    let pc = 66;
    for (row, col, tool) in [
        (1, 4, Tool::Line),
        (1, 7, Tool::Rect),
        (1, 10, Tool::Ellipse),
        (2, 1, Tool::Bucket),
        (2, 4, Tool::Picker),
        (2, 7, Tool::Eraser),
        (2, 10, Tool::Hand),
        (1, 1, Tool::Pencil),
    ] {
        click(&mut p, row, pc + col, Button::Left);
        assert_eq!(p.tool(), tool, "button at row {row} col {col}");
    }
    click(&mut p, 1, pc + 4, Button::Right);
    assert_eq!(p.tool(), Tool::Pencil, "a right click does not choose a tool");
    click(&mut p, 3, pc + 5, Button::Left);
    assert!(p.fill_on());
    click(&mut p, 3, pc + 12, Button::Left);
    assert!(!p.fill_on());
    click(&mut p, 4, pc + 3, Button::Left);
    click(&mut p, 4, pc + 3, Button::Left);
    assert_eq!(p.brush(), 3);
    click(&mut p, 4, pc + 3, Button::Right);
    assert_eq!(p.brush(), 2);
    click(&mut p, 9, pc + 5, Button::Left);
    click(&mut p, 9, pc + 1, Button::Right);
    assert_eq!((p.colors().0, p.colors().1), (8, 0), "left: foreground, right: background");
    click(&mut p, 7, pc + 3, Button::Left);
    assert_eq!((p.colors().0, p.colors().1), (0, 8), "the FG row swaps");
    click(&mut p, 8, pc + 9, Button::Right);
    assert_eq!((p.colors().0, p.colors().1), (8, 0), "the BG row swaps");
    for (row, col) in [(0, pc + 3), (5, pc + 3), (6, pc + 3), (19, pc + 3), (1, pc - 1), (1, 79)] {
        click(&mut p, row, col, Button::Left);
    }
    assert_eq!((p.tool(), p.brush(), p.colors().0), (Tool::Pencil, 2, 8), "nothing else is a target");
    click(&mut p, 17, pc + 2, Button::Left);
    assert_eq!((p.colors().0, p.prompt_kind()), (16, None), "one click on the custom swatch sets the foreground");
    click(&mut p, 17, pc + 2, Button::Left);
    assert_eq!(p.prompt_kind(), None, "two clicks 100 ticks apart are not a double-click");
    let custom = ((pc + 2) * 8 + 3, 17 * 16 + 3);
    p.handle_at(ev(Action::Down(Button::Left), custom, 1), 5000);
    p.handle_at(ev(Action::Up(Button::Left), custom, 0), 5002);
    p.handle_at(ev(Action::Down(Button::Left), custom, 1), 5020);
    assert_eq!(p.prompt_kind(), Some(PromptKind::Colour), "a double-click opens the colour prompt");
    keys(&mut p, &[Key::Escape]);

    // Graphics, 1024x768 at 100% (128x48 cells): the canvas is at zoom 8, centred at 200,216; the
    // pointer 3 pixels into a zoomed pixel is that pixel.
    let mut g = paint_on(64, 40);
    g.set_screen(128, 48, true, (8, 16));
    assert_eq!((g.zoom(), g.area(), g.canvas_origin()), ((8, 1), (912, 752), (200, 216)));
    for (n, m) in [(0usize, 0usize), (1, 1), (13, 7), (63, 39)] {
        assert_eq!(g.locate(200 + 8 * n + 3, 216 + 8 * m + 3, false), Some((n as i32, m as i32)), "pixel {n},{m}");
    }
    assert_eq!(g.locate(200 + 7, 216, false), Some((0, 0)));
    assert_eq!(g.locate(199, 216 + 3, false), None);
    assert_eq!(g.locate(200 + 512, 216 + 3, false), None);
    assert_eq!(g.locate(200 + 512 + 50, 216 + 8 * 40 + 50, true), Some((63, 39)));
    g.handle(ev(Action::Down(Button::Left), at(&g, 10, 10), 1));
    for x in [30, 50] {
        g.handle(ev(Action::Move, at(&g, x, 10), 1));
    }
    g.handle(ev(Action::Up(Button::Left), at(&g, 50, 10), 0));
    assert_eq!(count(&g, BLACK), 41, "no gaps at zoom 8 either");
    // The zoom steps with PgUp and PgDn.
    keys(&mut g, &[Key::PageDown]);
    assert_eq!(g.zoom(), (6, 1));
    assert!(g.message().contains("Zoom x6"), "{}", g.message());
    assert_eq!(g.canvas_origin(), ((912 - 6 * 64) / 2, (752 - 6 * 40) / 2));
    keys(&mut g, &[Key::PageUp, Key::PageUp, Key::PageUp, Key::PageUp, Key::PageUp]);
    assert_eq!(g.zoom(), (16, 1));
    let mut big = paint_on(300, 200);
    big.set_screen(128, 48, true, (8, 16));
    assert_eq!(big.zoom(), (3, 1), "the largest zoom that shows the whole canvas");
    // A bigger cell (200% text): the panel's cells are bigger, the targets the same.
    let mut g2 = paint_on(64, 40);
    g2.set_screen(64, 24, true, (16, 32));
    assert_eq!(g2.area(), (50 * 16, 23 * 32));
    assert_eq!(g2.panel_hit(1, 51), Some(Target::Tool(Tool::Pencil)));
    // In text mode the zoom keys work too (up to x8).
    let mut p = paint_on(64, 40);
    keys(&mut p, &[Key::PageUp]);
    assert!(p.message().contains("Zoom x2") && p.zoom() == (2, 1), "{}", p.message());

    // A canvas bigger than the area scrolls to follow the cursor, in text mode too.
    let mut p = paint_on(200, 100);
    p.set_screen(80, 24, false, (8, 16));
    assert_eq!((p.zoom(), p.visible()), ((1, 4), (200, 100)), "a big canvas starts at the fit");
    keys(&mut p, &[c('Z')]);
    assert_eq!(p.canvas_origin(), (0, 0));
    assert_eq!(p.visible(), (66, 46));
    keys(&mut p, &[Key::Char(b'\t')]);
    step_by(&mut p, 20, 0);
    assert_eq!(p.cursor(), (160, 0));
    let first = p.locate(3, 3, false).unwrap();
    assert!(first.0 >= 160 - 65 && first.0 <= 160, "the view followed the cursor: {first:?}");
    assert_eq!(p.locate(8 * 65 + 3, 3, false).map(|q| q.0), Some(first.0 + 65));
    step_by(&mut p, -30, 0);
    assert_eq!(p.cursor(), (0, 0));
    assert_eq!(p.locate(3, 3, false), Some((0, 0)), "scrolled back");

    // Below 40x12 paint only says so; Ctrl-Q still leaves.
    let mut p = paint_on(64, 40);
    for (cols, rows) in [(39, 12), (40, 11), (16, 6)] {
        p.set_screen(cols, rows, false, (8, 16));
        assert!(p.too_small(), "{cols}x{rows}");
        assert!(p.handle(c('l')) && p.handle(Key::Right) && p.handle(ev(Action::Down(Button::Left), (20, 20), 1)));
        assert_eq!((p.tool(), p.cursor()), (Tool::Pencil, (0, 0)), "keys and clicks do nothing");
    }
    assert!(!p.handle(Key::Ctrl(b'q')));
    p.set_screen(40, 12, false, (8, 16));
    assert!(!p.too_small());
    keys(&mut p, &[c('l')]);
    assert_eq!(p.tool(), Tool::Line);
    // 40x12: a canvas that does not fit scrolls, and every key still works without a panic.
    keys(&mut p, &[c(' '), Key::Right, Key::Down, c(' '), c('['), c(']'), c('}'), c('{'), c('x'), c('+'), c('-')]);
}

// --- Zoom and pan (T-015, D-006) ---

fn mev(action: Action, (x, y): (usize, usize), buttons: u8) -> Key {
    Key::Mouse(Event { action, x: x as u16, y: y as u16, dx: 0, dy: 0, buttons })
}

/// The pointer position (area pixels, as mouse events carry it) in the middle of area unit
/// (`ax`, `ay`): text mode has a virtual 8x16 per cell, two area rows per cell.
fn pointer_of(p: &Paint, ax: i32, ay: i32) -> (usize, usize) {
    if p.is_graphics() {
        (ax as usize, ay as usize)
    } else {
        ((ax * 8 + 3) as usize, ((ay / 2) * 16 + (ay % 2) * 8 + 3) as usize)
    }
}

/// A canvas whose pixels are told apart by colour (the 16 fixed colours, in a pattern that neither
/// neighbours nor the pixels of one block repeat), so a wrong sample shows.
fn patterned(w: u32, h: u32) -> Paint {
    let mut img = bmp::Image::new(w, h, 0xFFFFFF);
    for y in 0..h {
        for x in 0..w {
            let k = (x * 5 + y * 3 + (x >> 4) * 7 + (y >> 4) * 11 + (x >> 6) * 5 + (y >> 6) * 9) & 15;
            img.pixels[(y * w + x) as usize] = super::VGA[k as usize];
        }
    }
    Paint::new(img, None, "/")
}

/// Zoom to level `idx` with PgUp and PgDn.
fn go_zoom(p: &mut Paint, idx: usize) {
    for _ in 0..40 {
        if p.zoom_index() == idx {
            return;
        }
        keys(p, &[if p.zoom_index() < idx { Key::PageUp } else { Key::PageDown }]);
    }
    panic!("zoom level {idx} not reached, at {}", p.zoom_index());
}

/// The canvas pixel area unit (`ax`, `ay`) shows by the rule `view + a * down / up`, worked out
/// here again from the plain formula; None where there is no canvas.
fn shown_at(p: &Paint, ax: i32, ay: i32) -> Option<(i32, i32)> {
    let (ox, oy) = p.canvas_origin();
    let (ew, eh) = p.canvas_extent();
    let (up, down) = p.zoom();
    let v = p.view();
    let (rx, ry) = (ax - ox, ay - oy);
    if rx < 0 || ry < 0 || rx >= ew || ry >= eh {
        return None;
    }
    Some((v.0 + rx * down / up, v.1 + ry * down / up))
}

fn shown_pixel(p: &Paint, ax: i32, ay: i32) -> Option<u32> {
    let (x, y) = shown_at(p, ax, ay)?;
    assert!(p.image().get(x, y).is_some(), "a shown unit is on the canvas");
    Some(block_colour(p.image(), x, y, p.zoom().1))
}

/// What a reduced view shows for the block of `d` x `d` pixels at (x, y), worked out again here:
/// the rounded average over the block (clipped to the canvas), every `ceil(d / 8)`th pixel each
/// way; the pixel itself when `d` is 1.
fn block_colour(img: &bmp::Image, x: i32, y: i32, d: i32) -> u32 {
    if d == 1 {
        return img.get(x, y).unwrap();
    }
    let step = (d + 7) / 8;
    let mut sums = [0u32; 3];
    let mut n = 0;
    for yy in (y..(y + d).min(img.height as i32)).step_by(step as usize) {
        for xx in (x..(x + d).min(img.width as i32)).step_by(step as usize) {
            let px = img.get(xx, yy).unwrap();
            for (i, s) in sums.iter_mut().enumerate() {
                *s += px >> (16 - 8 * i) & 255;
            }
            n += 1;
        }
    }
    let [r, g, b] = sums.map(|s| (s + n / 2) / n);
    r << 16 | g << 8 | b
}

/// A reduced view averages its blocks, so a one-pixel line is fainter but never vanishes
/// between samples (it did with one sample per block: a stroke on an odd row disappeared at 1/2).
pub fn reduced_view_average() {
    let mut p = paint_on(64, 40);
    p.set_screen(80, 25, true, (8, 16));
    for x in 0..64 {
        p.img.set(x, 13, BLACK); // an odd row: 1/2, 1/4 and 1/8 all start their blocks elsewhere
    }
    for (idx, down, grey) in [(10, 2, 0x80), (8, 4, 0xBF), (6, 8, 0xDF)] {
        go_zoom(&mut p, idx);
        assert_eq!(p.zoom(), (1, down));
        let block_y = 13 / down * down;
        // Grey: 255 times the share of white pixels in the block, rounded.
        let want = grey << 16 | grey << 8 | grey;
        assert_eq!(p.shown(0, block_y, down), Some(want), "1/{down}");
        assert_eq!(p.shown(0, block_y + down, down), Some(WHITE), "1/{down}: the next block is white");
        // On the screen: the unit row of that block, in every column of the canvas.
        let (ox, oy) = p.canvas_origin();
        let row = p.row_pixels_for_test(oy + 13 / down, ox, ox + 64 / down);
        assert!(row.iter().all(|&px| px == want), "1/{down}: {row:x?}");
    }
    // Text mode: at 1/2 the line still shows (dark grey half blocks, not white).
    let mut p = paint_on(64, 40);
    for x in 0..64 {
        p.img.set(x, 13, BLACK);
    }
    go_zoom(&mut p, 10);
    p.blink = false;
    let (ox, oy) = p.canvas_origin();
    let unit_row = oy + 13 / 2;
    let cells = p.row_cells_for_test((unit_row / 2) as usize);
    let grey = bmp::nearest(&p.looks.palette, 0x808080);
    assert_ne!(grey, 15);
    let white = bmp::nearest(&p.looks.palette, WHITE);
    let want = if unit_row % 2 == 0 { bmp::half_block(grey, white) } else { bmp::half_block(white, grey) };
    assert!(cells[ox as usize..(ox + 32) as usize].iter().all(|&c| c == want), "text 1/2");
    // A block clipped by the canvas's edge averages only what is there.
    let mut img = bmp::Image::new(5, 5, WHITE);
    img.set(4, 4, BLACK);
    let mut p = Paint::new(img, None, "/");
    p.set_screen(80, 25, true, (8, 16));
    assert_eq!(p.shown(4, 4, 4), Some(BLACK), "a 1x1 corner block is its one pixel");
    assert_eq!(p.shown(0, 0, 4), Some(WHITE));
    assert_eq!(p.shown(5, 0, 4), None);
}

/// Every cell of every canvas row in text mode is what the mapping says (cursor off).
fn check_text_cells(p: &mut Paint, what: &str) {
    p.blink = false;
    let blank = (p.looks.normal >> 4) & 15;
    for r in 0..p.work_rows() {
        let cells = p.row_cells_for_test(r);
        for col in 0..p.panel_col() {
            let index = |px: Option<u32>| px.map(|px| bmp::nearest(&p.looks.palette, px));
            let top = index(shown_pixel(p, col as i32, 2 * r as i32));
            let bottom = index(shown_pixel(p, col as i32, 2 * r as i32 + 1));
            let want = if top.is_none() && bottom.is_none() {
                crate::vga::cell(b' ', p.looks.normal)
            } else {
                bmp::half_block(top.unwrap_or(blank), bottom.unwrap_or(blank))
            };
            assert_eq!(cells[col], want, "{what}: row {r} col {col}");
        }
    }
    p.blink = true;
}

/// Every pixel row of the area in a graphics mode is what the mapping says (cursor off).
fn check_pixel_rows(p: &Paint, what: &str) {
    let (aw, ah) = p.area();
    let [r, g, b] = p.looks.palette[(p.looks.normal >> 4 & 15) as usize];
    let letterbox = (r as u32) << 16 | (g as u32) << 8 | b as u32;
    for ay in 0..ah {
        let row = p.row_pixels_for_test(ay, 0, aw);
        assert_eq!(row.len(), aw as usize);
        for ax in 0..aw {
            assert_eq!(row[ax as usize], shown_pixel(p, ax, ay).unwrap_or(letterbox), "{what}: unit {ax},{ay}");
        }
        if ay % 41 == 0 {
            assert_eq!(p.row_pixels_for_test(ay, 5, aw - 3)[..], row[5..aw as usize - 3], "{what}: part of row {ay}");
        }
    }
}

/// `locate` agrees with the mapping at some area units.
fn check_locate(p: &Paint, what: &str) {
    let (aw, ah) = p.area();
    for ax in [0, 1, 2, 7, aw / 3, aw / 2, aw - 2, aw - 1] {
        for ay in [0, 1, 2, 11, ah / 3, ah / 2, ah - 2, ah - 1] {
            let (x, y) = pointer_of(p, ax, ay);
            assert_eq!(p.locate(x, y, false), shown_at(p, ax, ay), "{what}: pointer at unit {ax},{ay}");
        }
    }
}

pub fn zoom_levels() {
    use super::zoom_label;

    // The table: 19 levels from 1/64 to x16, x1 at ZOOM_ONE, x8 at ZOOM_TEXT_MAX, increasing.
    assert_eq!(ZOOMS.len(), 19);
    assert_eq!((ZOOMS[0], ZOOMS[ZOOM_ONE], ZOOMS[ZOOM_TEXT_MAX], ZOOMS[18]), ((1, 64), (1, 1), (8, 1), (16, 1)));
    for w in ZOOMS.windows(2) {
        assert!(w[0].0 * w[1].1 < w[1].0 * w[0].1, "{:?} then {:?}", w[0], w[1]);
    }
    assert_eq!([zoom_label((4, 1)), zoom_label((1, 4)), zoom_label((1, 64))], ["x4", "1/4", "1/64"]);
    let budget = super::undo_budget();
    assert!((8 << 20..=64 << 20).contains(&budget), "{budget}");
    assert_eq!(budget, (crate::heap::stats().size / 8).clamp(8 << 20, 64 << 20));

    // Text mode: PgUp goes x1 to x8 and stops, PgDn goes down to 1/64 and stops.
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z')]);
    assert_eq!((p.zoom(), p.message()), ((1, 1), " Zoom x1"));
    for want in [(2, 1), (3, 1), (4, 1), (6, 1), (8, 1)] {
        keys(&mut p, &[Key::PageUp]);
        assert_eq!(p.zoom(), want);
        assert_eq!(p.message(), format!(" Zoom {}", zoom_label(want)));
    }
    keys(&mut p, &[Key::PageUp]);
    assert_eq!(p.zoom(), (8, 1), "text mode stops at x8");
    assert!(p.message().contains("largest") && p.message().contains("x8"), "{}", p.message());
    for idx in (0..ZOOM_TEXT_MAX).rev() {
        keys(&mut p, &[Key::PageDown]);
        assert_eq!(p.zoom_index(), idx);
    }
    assert_eq!(p.message(), " Zoom 1/64");
    keys(&mut p, &[Key::PageDown]);
    assert_eq!(p.zoom(), (1, 64));
    assert!(p.message().contains("smallest") && p.message().contains("1/64"), "{}", p.message());
    // Graphics modes go on to x16.
    let mut g = paint_on(300, 200);
    g.set_screen(128, 48, true, (8, 16));
    go_zoom(&mut g, 18);
    assert_eq!(g.zoom(), (16, 1));
    keys(&mut g, &[Key::PageUp]);
    assert!(g.message().contains("largest") && g.zoom() == (16, 1), "{}", g.message());
    go_zoom(&mut g, 0);
    assert_eq!(g.zoom(), (1, 64));

    // z fits the whole canvas (the start zoom, capped), Z is x1.
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    assert_eq!((p.zoom(), p.visible()), ((1, 6), (300, 200)), "300x200 fits whole at 1/6 in 66x46");
    keys(&mut p, &[c('Z')]);
    assert_eq!((p.zoom(), p.visible()), ((1, 1), (66, 46)));
    keys(&mut p, &[c('z')]);
    assert_eq!((p.zoom(), p.message()), ((1, 6), " Zoom 1/6 (fit)"));
    let mut g = paint_on(300, 200);
    g.set_screen(128, 48, true, (8, 16));
    keys(&mut g, &[c('Z')]);
    assert_eq!(g.zoom(), (1, 1));
    keys(&mut g, &[c('z')]);
    assert_eq!((g.zoom(), g.message()), ((3, 1), " Zoom x3 (fit)"));
    keys(&mut g, &[c('p')]);
    assert_eq!(g.tool(), Tool::Pencil);

    // The default zoom: small canvases x1 in text mode and x8 in graphics modes, big ones shrunk
    // to fit, and 1/64 when even that is not enough.
    for (w, h, cols, rows, graphics, want) in [
        (64, 40, 80, 25, false, (1, 1)),
        (10, 10, 80, 25, false, (1, 1)),
        (64, 40, 128, 48, true, (8, 1)),
        (10, 10, 128, 48, true, (8, 1)),
        (300, 200, 128, 48, true, (3, 1)),
        (300, 200, 80, 25, false, (1, 6)),
        (1000, 400, 80, 24, false, (1, 16)),
        (1000, 400, 128, 48, true, (1, 2)),
        (4096, 16, 40, 12, false, (1, 64)),
        (4096, 16, 40, 12, true, (1, 24)),
    ] {
        let mut p = paint_on(w, h);
        p.set_screen(cols, rows, graphics, (8, 16));
        assert_eq!(p.zoom(), want, "{w}x{h} on {cols}x{rows} graphics={graphics}");
    }
    // The user's level survives a change of display when it is valid (text mode clamps it to x8);
    // otherwise the fit is chosen again; z gives the choice back.
    let mut p = paint_on(300, 200);
    p.set_screen(128, 48, true, (8, 16));
    p.set_screen(80, 24, false, (8, 16));
    assert_eq!(p.zoom(), (1, 6), "no choice: the fit of the new display");
    p.set_screen(128, 48, true, (8, 16));
    assert_eq!(p.zoom(), (3, 1));
    keys(&mut p, &[Key::PageUp]);
    p.set_screen(80, 24, false, (8, 16));
    assert_eq!(p.zoom(), (4, 1), "chosen: kept");
    p.set_screen(128, 48, true, (8, 16));
    go_zoom(&mut p, 17);
    p.set_screen(80, 24, false, (8, 16));
    assert_eq!(p.zoom(), (8, 1), "x12 does not exist in text mode");
    keys(&mut p, &[c('z')]);
    p.set_screen(128, 48, true, (8, 16));
    assert_eq!(p.zoom(), (3, 1), "z: the display chooses again");
    // A new canvas starts at its fit, unless the user chose a zoom.
    let mut p = paint_on(64, 40);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[ctrl('n')]);
    chars(&mut p, "1000x400");
    keys(&mut p, &[Key::Enter]);
    assert_eq!((p.image().width, p.zoom()), (1000, (1, 16)));
    keys(&mut p, &[Key::PageUp, ctrl('n')]);
    chars(&mut p, "64x40");
    keys(&mut p, &[Key::Enter]);
    assert_eq!((p.image().width, p.zoom()), (64, (1, 12)), "a chosen zoom is kept for the next canvas");
}

pub fn zoom_mapping() {
    // The helpers alone: for every level, canvas length and area, the units of a pixel map back
    // to it (magnified: every unit of its block; reduced: the unit that samples it), and the units
    // of the visible canvas stay inside the area.
    for &z in ZOOMS.iter() {
        for len in [1, 7, 300, 4096] {
            for area in [26, 46, 66, 208, 912, 1920] {
                let vw = super::visible_len(len, area, z);
                let e = extent(vw, z);
                assert!(vw >= 1 && vw <= len && e >= 1 && e <= area, "{z:?} len {len} area {area}: {vw} {e}");
                for a in 0..e {
                    let k = sample_off(a, z);
                    assert!(k < vw && a >= unit_of(k, z) && a < extent(k + 1, z), "{z:?} unit {a} shows {k}");
                }
                for k in 0..vw {
                    let (lo, hi) = (unit_of(k, z), extent(k + 1, z));
                    assert!(lo < hi && hi <= e, "{z:?} pixel {k}: {lo}..{hi}");
                    if z.1 == 1 {
                        assert!((lo..hi).all(|a| sample_off(a, z) == k), "{z:?} block of {k}");
                    } else if k % z.1 == 0 {
                        assert_eq!(sample_off(lo, z), k, "{z:?} sample {k}");
                    }
                }
            }
        }
    }

    // Through Paint, at every level of both modes: the pointer, the text cells and the pixel rows
    // all show the same pixel, for the view at the start and after panning.
    let mut p = patterned(300, 200);
    p.set_screen(80, 25, false, (8, 16));
    for idx in 0..=ZOOM_TEXT_MAX {
        go_zoom(&mut p, idx);
        keys(&mut p, &[c('Z')]);
        go_zoom(&mut p, idx);
        let what = format!("text level {idx}");
        check_locate(&p, &what);
        check_text_cells(&mut p, &what);
        keys(&mut p, &[c('d'), c('s')]);
        check_locate(&p, &what);
        check_text_cells(&mut p, &what);
        keys(&mut p, &[c('a'), c('w')]);
    }
    let mut g = patterned(300, 200);
    g.set_screen(40, 12, true, (8, 16));
    assert_eq!(g.area(), (208, 176));
    for idx in 0..ZOOMS.len() {
        go_zoom(&mut g, idx);
        let what = format!("graphics level {idx}");
        check_locate(&g, &what);
        check_pixel_rows(&g, &what);
        keys(&mut g, &[c('d'), c('s')]);
        check_locate(&g, &what);
        check_pixel_rows(&g, &what);
        keys(&mut g, &[c('a'), c('w')]);
    }
    // A bigger cell (200% text) and a one-pixel-wide canvas work as well.
    let mut g = patterned(300, 200);
    g.set_screen(64, 24, true, (16, 32));
    for idx in [0, 5, 11, 14, 18] {
        go_zoom(&mut g, idx);
        check_locate(&g, "200% text");
    }
    let mut thin = patterned(1, 300);
    thin.set_screen(80, 24, false, (8, 16));
    for idx in 0..=ZOOM_TEXT_MAX {
        go_zoom(&mut thin, idx);
        check_text_cells(&mut thin, "1x300");
    }

    // The dirty rectangle of a pixel holds every unit that samples it, and the cursor's box and
    // ring are always inside what is drawn again when the cursor moves.
    let mut g = patterned(300, 200);
    g.set_screen(40, 12, true, (8, 16));
    for idx in 0..ZOOMS.len() {
        go_zoom(&mut g, idx);
        keys(&mut g, &[c('d'), c('s')]);
        let (ew, eh) = g.canvas_extent();
        let (ox, oy) = g.canvas_origin();
        for (ax, ay) in [(ox, oy), (ox + ew / 2, oy + eh / 2), (ox + ew - 1, oy + eh - 1), (ox + 3i32.min(ew - 1), oy + 2i32.min(eh - 1))] {
            let Some((cx, cy)) = shown_at(&g, ax, ay) else { panic!("unit {ax},{ay} is over the canvas") };
            g.dirty = bmp::Rect::new(cx, cy, 1, 1);
            let (x0, y0, x1, y1) = g.dirty_area().expect("a visible pixel has an area");
            assert!(x0 <= ax && ax < x1 && y0 <= ay && ay < y1, "level {idx}: pixel {cx},{cy} at unit {ax},{ay} is in {x0},{y0}..{x1},{y1}");
        }
        // Hover: the cursor goes to the pixel under the pointer, whose box holds that unit.
        let (ax, ay) = (ox + ew / 2, oy + eh / 2);
        g.handle(mev(Action::Move, pointer_of(&g, ax, ay), 0));
        let (bx0, by0, bx1, by1) = g.cursor_box();
        assert!(bx0 <= ax && ax < bx1 && by0 <= ay && ay < by1 && bx1 > bx0 && by1 > by0, "level {idx}: box {bx0},{by0}..{bx1},{by1} at {ax},{ay}");
        g.dirty = g.cursor_area();
        let (x0, y0, x1, y1) = g.dirty_area().expect("the cursor is in view");
        let (aw, ah) = g.area();
        assert!(x0 <= (bx0 - 1).max(ox) && x1 >= (bx1 + 1).min(ox + ew) && y0 <= (by0 - 1).max(oy) && y1 >= (by1 + 1).min(oy + eh), "level {idx}: ring {x0},{y0}..{x1},{y1} of {bx0},{by0}..{bx1},{by1} in {aw}x{ah}");
        keys(&mut g, &[c('a'), c('w')]);
    }

    // The cursor is always seen at a reduction: in text mode the cells of its box change when
    // it blinks, at every level and with a bigger brush.
    for brush_keys in [0, 2] {
        let mut p = paint_on(300, 200);
        p.set_screen(80, 24, false, (8, 16));
        for _ in 0..brush_keys {
            keys(&mut p, &[c('+')]);
        }
        for idx in 0..=ZOOM_TEXT_MAX {
            go_zoom(&mut p, idx);
            let (bx0, by0, bx1, by1) = p.cursor_box();
            assert!(bx1 > bx0 && by1 > by0);
            let (ox, oy) = p.canvas_origin();
            assert_eq!((bx0, by0), (ox, oy), "the cursor is at the canvas's corner");
            p.blink = true;
            let lit = p.row_cells_for_test(by0 as usize / 2)[bx0 as usize];
            p.blink = false;
            let unlit = p.row_cells_for_test(by0 as usize / 2)[bx0 as usize];
            assert_ne!(lit, unlit, "level {idx}, brush {}: the cursor shows", p.brush());
            p.blink = true;
        }
    }
}

pub fn zoom_keys_and_pan() {
    let units = |p: &Paint| p.cursor_box();

    // W A S D (either case) pan the view by half of what is visible and the cursor with it, so
    // the cursor stays where it is on the screen; at the edge the view stops and the cursor goes
    // on, clamped to the canvas.
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z')]);
    assert_eq!((p.view(), p.cursor(), p.visible()), ((0, 0), (0, 0), (66, 46)));
    let spot = units(&p);
    keys(&mut p, &[c('d')]);
    assert_eq!((p.view(), p.cursor()), ((33, 0), (33, 0)));
    assert_eq!(units(&p), spot, "the cursor keeps its place on the screen");
    keys(&mut p, &[c('S')]);
    assert_eq!((p.view(), p.cursor()), ((33, 23), (33, 23)));
    assert_eq!(units(&p), spot);
    for _ in 0..12 {
        keys(&mut p, &[c('d')]);
    }
    assert_eq!((p.view().0, p.cursor().0), (234, 299), "the view stops at 300 - 66, the cursor at 299");
    keys(&mut p, &[c('a')]);
    assert_eq!((p.view().0, p.cursor().0), (201, 266));
    keys(&mut p, &[c('W'), c('w'), c('w')]);
    assert_eq!((p.view().1, p.cursor().1), (0, 0));
    // A canvas that fits does not scroll, but the cursor still goes by half of it.
    let mut small = paint_on(64, 40);
    small.set_screen(80, 24, false, (8, 16));
    keys(&mut small, &[c('d'), c('s')]);
    assert_eq!((small.view(), small.cursor()), ((0, 0), (32, 20)));
    keys(&mut small, &[c('d'), c('d'), c('s'), c('s')]);
    assert_eq!(small.cursor(), (63, 39));
    // With the pen down a pan draws a line, with a corner set it moves the preview, like arrows.
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z'), c(' '), c('d')]);
    assert_eq!(count(&p, BLACK), 34, "0 to 33 on the first row");
    assert!(p.pen_is_down());
    keys(&mut p, &[c(' ')]);
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z'), c('l'), c(' '), c('d')]);
    assert!(p.shape_in_progress() && !p.is_modified());
    assert_eq!(p.image().get(33, 0), Some(BLACK), "the preview follows");
    keys(&mut p, &[Key::Escape]);
    assert_eq!(count(&p, BLACK), 0);
    // Reduced, half of the visible part is a lot of pixels.
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z')]);
    go_zoom(&mut p, 8);
    assert_eq!(p.visible(), (264, 184));
    keys(&mut p, &[c('d'), c('s')]);
    assert_eq!((p.view(), p.cursor()), ((36, 16), (132, 92)));
    // Tab: 1, 8, 64, 1.
    let mut p = paint_on(300, 200);
    keys(&mut p, &[c('\t')]);
    assert_eq!(p.step_size(), 8);
    keys(&mut p, &[c('\t')]);
    assert_eq!(p.step_size(), 64);
    keys(&mut p, &[Key::Right, Key::Down]);
    assert_eq!(p.cursor(), (64, 64));
    keys(&mut p, &[c('\t'), Key::Right]);
    assert_eq!((p.step_size(), p.cursor()), (1, (65, 64)));

    // The Hand tool: the H key and button; Space only says what it does; it is no pen and no shape.
    assert!(!Tool::Hand.is_pen() && !Tool::Hand.is_shape());
    assert_eq!((Tool::from_letter(b'h'), Tool::Hand.letter()), (Some(Tool::Hand), b'H'));
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('H')]);
    assert_eq!(p.tool(), Tool::Hand);
    keys(&mut p, &[c(' ')]);
    assert!(p.message().contains("Hand") && !p.pen_is_down() && !p.shape_in_progress(), "{}", p.message());
    keys(&mut p, &[Key::Enter]);
    assert_eq!((count(&p, BLACK), p.is_modified()), (0, false));
    let row = p.table().iter().position(|&r| r == Row::Tools2).unwrap();
    let cells = p.row_cells_for_test(row);
    let shown: alloc::string::String = cells[p.panel_col() + 1..].iter().map(|&c| (c & 0xFF) as u8 as char).collect();
    assert!(shown.starts_with("[B][I][E]>H<"), "{shown}");

    // Drags pan: the view goes by (pointer - anchor) * down / up, clamped, from where it was at
    // the press, and the cursor stays. Middle button with any tool, either button with Hand.
    let pan = |p: &mut Paint, button: Button, bit: u8, from: (i32, i32), to: &[(i32, i32)]| {
        p.handle(mev(Action::Down(button), pointer_of(p, from.0, from.1), bit));
        for &(ax, ay) in to {
            p.handle(mev(Action::Move, pointer_of(p, ax, ay), bit));
        }
        let last = to.last().copied().unwrap_or(from);
        p.handle(mev(Action::Up(button), pointer_of(p, last.0, last.1), 0));
    };
    for (tool_key, button, bit) in [('p', Button::Middle, 4), ('r', Button::Middle, 4), ('h', Button::Left, 1), ('h', Button::Right, 2), ('h', Button::Middle, 4)] {
        // x4 (text mode): 4 units are a pixel.
        let mut p = paint_on(300, 200);
        p.set_screen(80, 24, false, (8, 16));
        keys(&mut p, &[c('Z'), c(tool_key)]);
        go_zoom(&mut p, 14);
        for _ in 0..6 {
            keys(&mut p, &[c('d'), c('s')]);
        }
        let (v0, cursor) = (p.view(), p.cursor());
        assert_eq!((v0, p.visible()), ((48, 30), (16, 11)), "setup for {tool_key} {button:?}");
        pan(&mut p, button, bit, (50, 30), &[(30, 20)]);
        assert_eq!(p.view(), (v0.0 + 5, v0.1 + 2), "x4: 20 units left, 10 up: 5 and 2 pixels ({tool_key} {button:?})");
        // Moves count from the press, not from the last move; a new drag starts at the view then.
        pan(&mut p, button, bit, (50, 30), &[(70, 40), (30, 20)]);
        assert_eq!(p.view(), (v0.0 + 10, v0.1 + 4));
        pan(&mut p, button, bit, (50, 30), &[(70, 40)]);
        assert_eq!(p.view(), (v0.0 + 5, v0.1 + 2));
        assert_eq!(p.cursor(), cursor, "the cursor does not move in a pan");
        assert_eq!((count(&p, BLACK), p.is_modified(), p.pen_is_down(), p.shape_in_progress()), (0, false, false, false));
        // Far away: the view goes on by 60 and 40 units, 15 and 10 pixels, and the cursor is
        // out of sight; an arrow then brings the view back to it.
        pan(&mut p, button, bit, (60, 40), &[(0, 0)]);
        assert_eq!(p.view(), (v0.0 + 5 + 15, v0.1 + 2 + 10));
        assert_eq!(p.cursor(), cursor);
        keys(&mut p, &[Key::Right]);
        let (vw, vh) = p.visible();
        let (view, at) = (p.view(), p.cursor());
        assert_eq!(at, (cursor.0 + 1, cursor.1));
        assert!(view.0 <= at.0 && at.0 < view.0 + vw && view.1 <= at.1 && at.1 < view.1 + vh, "{view:?} {at:?}");
        // 1/4: a unit is 4 pixels.
        let mut p = paint_on(300, 200);
        p.set_screen(80, 24, false, (8, 16));
        keys(&mut p, &[c('Z'), c(tool_key)]);
        go_zoom(&mut p, 8);
        assert_eq!((p.view(), p.visible()), ((0, 0), (264, 184)));
        pan(&mut p, button, bit, (40, 30), &[(37, 29)]);
        assert_eq!(p.view(), (12, 4), "1/4: 3 units left, 1 up: 12 and 4 pixels");
        pan(&mut p, button, bit, (40, 30), &[(30, 20)]);
        assert_eq!(p.view(), (36, 16), "clamped to 300 - 264 and 200 - 184");
        pan(&mut p, button, bit, (40, 30), &[(50, 38)]);
        assert_eq!(p.view(), (0, 0));
        assert_eq!(p.cursor(), (0, 0));
    }
    // A middle drag on the panel does nothing; the wheel and drag leave a stroke alone.
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z')]);
    p.handle(mev(Action::Down(Button::Middle), (8 * 70, 40), 4));
    p.handle(mev(Action::Move, (8 * 70, 90), 4));
    p.handle(mev(Action::Up(Button::Middle), (8 * 70, 90), 0));
    assert_eq!((p.view(), p.tool(), count(&p, BLACK)), ((0, 0), Tool::Pencil, 0));
    // Hovering follows the pointer but never scrolls the view (the keys do).
    keys(&mut p, &[c('d')]);
    assert_eq!(p.view(), (33, 0));
    p.handle(mev(Action::Move, pointer_of(&p, 1, 5), 0));
    assert_eq!((p.cursor(), p.view()), ((34, 5), (33, 0)), "the view stays under a hovering pointer");
    // A mouse press at 1/4 sets exactly the pixel view + unit * 4; at x4, view + unit / 4.
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z')]);
    go_zoom(&mut p, 8);
    keys(&mut p, &[c('d'), c('s')]);
    let v = p.view();
    assert_eq!(v, (36, 16));
    p.handle(mev(Action::Down(Button::Left), pointer_of(&p, 10, 5), 1));
    p.handle(mev(Action::Up(Button::Left), pointer_of(&p, 10, 5), 0));
    assert_eq!(count(&p, BLACK), 1);
    assert_eq!(p.image().get(v.0 + 40, v.1 + 20), Some(BLACK), "unit 10,5 at 1/4 is pixel view + 40,20");
    assert_eq!(p.cursor(), (v.0 + 40, v.1 + 20));
    go_zoom(&mut p, 14);
    let v = p.view();
    p.handle(mev(Action::Down(Button::Left), pointer_of(&p, 10, 6), 1));
    p.handle(mev(Action::Up(Button::Left), pointer_of(&p, 10, 6), 0));
    assert_eq!(p.image().get(v.0 + 2, v.1 + 1), Some(BLACK), "unit 10,6 at x4 is pixel view + 2,1");
    assert_eq!((count(&p, BLACK), p.cursor()), (2, (v.0 + 2, v.1 + 1)));

    // The wheel zooms about the pixel under the pointer: one level per notch, in on negative.
    let mut p = paint_on(1000, 400);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z')]);
    for _ in 0..14 {
        keys(&mut p, &[c('d')]);
    }
    for _ in 0..6 {
        keys(&mut p, &[c('s')]);
    }
    assert_eq!(p.view(), (462, 138));
    let wheel = |p: &mut Paint, at: (usize, usize), notches: i8| {
        p.handle(mev(Action::Wheel(notches), at, 0));
    };
    let pointer = pointer_of(&p, 30, 20);
    let mut pixel = p.locate(pointer.0, pointer.1, false).expect("over the canvas");
    assert_eq!(pixel, (492, 158));
    for notches in [-1i8, -3, 5, 1, -2, 2, -5, 3, -1, 1, -4] {
        let before = p.zoom_index();
        wheel(&mut p, pointer, notches);
        let want = if notches < 0 { before + notches.unsigned_abs() as usize } else { before.saturating_sub(notches as usize) };
        assert_eq!(p.zoom_index(), want.min(ZOOM_TEXT_MAX), "wheel {notches} from {before}");
        let (vw, vh) = p.visible();
        let now = p.locate(pointer.0, pointer.1, false).expect("still over the canvas");
        // The pixel stays under the pointer along every axis that scrolls and is not at an end.
        let (z, v) = (p.zoom(), p.view());
        for (axis, (len, vis, pos, got, view)) in [(1000, vw, pixel.0, now.0, v.0), (400, vh, pixel.1, now.1, v.1)].into_iter().enumerate() {
            let want_view = pos - sample_off(if axis == 0 { 30 } else { 20 }, z);
            if vis < len && want_view >= 0 && want_view <= len - vis {
                assert_eq!((got, view), (pos, want_view), "wheel {notches} to {z:?}, axis {axis}");
            }
        }
        pixel = now;
    }
    // The message names the level; the wheel at the limits does nothing.
    let mut p = paint_on(300, 200);
    p.set_screen(80, 24, false, (8, 16));
    keys(&mut p, &[c('Z')]);
    let at = pointer_of(&p, 10, 10);
    wheel(&mut p, at, -1);
    assert_eq!((p.zoom(), p.message()), ((2, 1), " Zoom x2"));
    wheel(&mut p, at, -100);
    assert_eq!(p.zoom(), (8, 1), "text mode stops at x8");
    wheel(&mut p, at, 127);
    assert_eq!(p.zoom(), (1, 64));
    // Over the panel (not the canvas) it zooms about the view's middle.
    wheel(&mut p, (8 * 70, 40), -2);
    assert_eq!(p.zoom_index(), 2);
    // Not while a button is held, and not while a prompt is open.
    let held = p.zoom_index();
    let corner = pointer_of(&p, 3, 3);
    p.handle(mev(Action::Down(Button::Left), corner, 1));
    p.handle(mev(Action::Wheel(-1), corner, 1));
    assert_eq!(p.zoom_index(), held, "a button is held");
    p.handle(mev(Action::Up(Button::Left), corner, 0));
    keys(&mut p, &[ctrl('a')]);
    wheel(&mut p, at, -1);
    assert_eq!((p.zoom_index(), p.prompt_kind()), (held, Some(PromptKind::SaveAs)), "a prompt is open");
    keys(&mut p, &[Key::Escape]);
    wheel(&mut p, at, -1);
    assert_eq!(p.zoom_index(), held + 1);
    // Graphics: the same, with more levels (this canvas scrolls both ways from x1 to 1/2).
    let mut g = paint_on(1000, 400);
    g.set_screen(40, 12, true, (8, 16));
    keys(&mut g, &[c('Z'), c('d'), c('d'), c('d'), c('s')]);
    assert_eq!(g.view(), (312, 88));
    let at = pointer_of(&g, 100, 80);
    let pixel = g.locate(at.0, at.1, false).unwrap();
    assert_eq!(pixel, (412, 168));
    for notches in [-2i8, -3, 4, 1, -1, 2] {
        wheel(&mut g, at, notches);
        assert_eq!(g.locate(at.0, at.1, false), Some(pixel), "graphics wheel {notches} to {:?}", g.zoom());
    }
    assert_eq!(g.zoom(), (1, 2));
}

/// A 4096x4096 canvas on a graphics screen: it starts at the fit, the cursor reaches the corner,
/// a stroke and its undo work. Memory needed: 64 MiB for the canvas; skipped if it does not fit.
#[cfg(feature = "qemu-test")]
pub fn zoom_4096() {
    let Ok(img) = crate::picture::blank(4096, 4096, 0xFFFFFF) else {
        crate::println!("  (no memory for a 4096x4096 canvas here: skipped)");
        return;
    };
    let mut p = Paint::new(img, None, "/");
    p.set_screen(128, 48, true, (8, 16));
    assert_eq!((p.zoom(), p.visible()), ((1, 6), (4096, 4096)), "the fit of 912x752 units");
    // To the far corner in steps of 64.
    keys(&mut p, &[c('\t'), c('\t')]);
    assert_eq!(p.step_size(), 64);
    for _ in 0..64 {
        keys(&mut p, &[Key::Right, Key::Down]);
    }
    assert_eq!(p.cursor(), (4095, 4095));
    keys(&mut p, &[c('\t')]);
    assert_eq!(p.step_size(), 1);
    // A stroke at the corner, and its undo.
    keys(&mut p, &[c(' '), Key::Left, Key::Left, Key::Left, c(' ')]);
    assert_eq!(count(&p, BLACK), 4);
    assert_eq!(p.image().get(4092, 4095), Some(BLACK));
    assert_eq!(p.undo_depth(), (1, 0));
    keys(&mut p, &[ctrl('z')]);
    assert_eq!(count(&p, BLACK), 0);
    // The pixel rows at the fit are drawn from one sample per unit; zooming in with PgUp keeps
    // the cursor in sight at every level, and W and A then move it by half of what is visible.
    let (aw, _) = p.area();
    assert_eq!(p.row_pixels_for_test(400, 0, aw).len(), aw as usize);
    for _ in 0..7 {
        keys(&mut p, &[Key::PageUp]);
        let (vw, vh) = p.visible();
        let (v, at) = (p.view(), p.cursor());
        assert!(v.0 <= at.0 && at.0 < v.0 + vw && v.1 <= at.1 && at.1 < v.1 + vh, "{:?}: {v:?} {at:?}", p.zoom());
    }
    assert_eq!((p.zoom(), p.visible()), ((4, 1), (228, 188)));
    keys(&mut p, &[c('a'), c('w')]);
    assert_eq!(p.cursor(), (4092 - 114, 4095 - 94), "the stroke left the cursor at 4092");
    keys(&mut p, &[c('z')]);
    assert_eq!((p.zoom(), p.view()), ((1, 6), (0, 0)));
}

#[cfg(feature = "qemu-test")]
pub fn files() {
    use crate::fs;
    let _ = fs::with(|fs| fs.remove("/st-pic.bmp", false));

    // An untitled 33x7 canvas: Ctrl-S asks for a name, `st-pic` is saved as `st-pic.bmp`, and the
    // file decodes to exactly the canvas.
    let mut p = paint_on(33, 7);
    keys(&mut p, &[c('r'), c('F'), c(']'), Key::Right, Key::Down, c(' ')]);
    step_by(&mut p, 5, 3);
    keys(&mut p, &[c(' '), c('p'), c('}'), c('}'), c('c')]);
    chars(&mut p, "12AB34");
    keys(&mut p, &[Key::Enter, Key::Right, c(' ')]);
    step_by(&mut p, 9, 1);
    keys(&mut p, &[c(' ')]);
    assert!(p.is_modified() && p.path().is_none());
    keys(&mut p, &[Key::Ctrl(b's')]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::SaveAs));
    assert_eq!(p.bar(), " Save as:   Enter ok  Esc no");
    chars(&mut p, "st-pic");
    keys(&mut p, &[Key::Enter]);
    assert_eq!(p.prompt_kind(), None);
    assert_eq!(p.message(), " Saved st-pic.bmp (33x7)");
    assert_eq!((p.path(), p.is_modified()), (Some("/st-pic.bmp"), false));
    let data = fs::with(|fs| fs.read("/st-pic.bmp")).expect("the file was written");
    assert_eq!(&data[..2], b"BM");
    assert_eq!(bmp::decode(&data).expect("a valid BMP"), *p.image(), "the file is the canvas");
    // Ctrl-O on a named picture saves at once, and says so (a change first, to see it saved).
    keys(&mut p, &[Key::Up, Key::Up, Key::Up, c(' '), c(' ')]);
    assert!(p.is_modified());
    keys(&mut p, &[Key::Ctrl(b'o')]);
    assert_eq!((p.prompt_kind(), p.is_modified()), (None, false));
    assert_eq!(bmp::decode(&fs::with(|fs| fs.read("/st-pic.bmp")).unwrap()).unwrap(), *p.image());

    // Reopen: pixel for pixel the same, odd width and all.
    let again = Paint::open("/", Some("st-pic.bmp"), None).expect("reopen");
    assert_eq!((again.image(), again.path()), (p.image(), Some("/st-pic.bmp")));
    // A name that does not exist starts a blank canvas that will be saved under that name.
    let blank = Paint::open("/", Some("st-none"), Some((10, 5))).expect("blank");
    assert_eq!((blank.image().width, blank.image().height, blank.path()), (10, 5, Some("/st-none.bmp")));
    assert_eq!(blank.image().get(9, 4), Some(0xFFFFFF));
    assert!(Paint::open("/", Some("st-pic.bmp"), Some((3, 3))).is_err(), "-s with an existing file");
    assert!(Paint::open("/", None, Some((0, 5))).is_err() && Paint::open("/", None, Some((5000, 2000))).is_err());
    assert!(Paint::open("/", Some("/docs/index.txt"), None).err().is_some_and(|e| e.contains("not a BMP")));
    assert_eq!(Paint::open("/", None, None).map(|p| (p.image().width, p.image().height)), Ok((64, 40)));

    // Ctrl-A on an existing name asks whether to replace it; only y does.
    keys(&mut p, &[Key::Ctrl(b'z'), Key::Ctrl(b'a')]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::SaveAs));
    chars(&mut p, "st-pic");
    keys(&mut p, &[Key::Enter]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::Overwrite));
    assert_eq!(p.bar(), " st-pic.bmp exists. Overwrite? (y/n)");
    keys(&mut p, &[c('n')]);
    assert_eq!(p.prompt_kind(), None);
    assert_ne!(bmp::decode(&fs::with(|fs| fs.read("/st-pic.bmp")).unwrap()).unwrap(), *p.image(), "n kept the old file");
    keys(&mut p, &[Key::Ctrl(b'a')]);
    chars(&mut p, "st-pic2.BMP");
    keys(&mut p, &[Key::Enter]);
    assert_eq!((p.prompt_kind(), p.path()), (None, Some("/st-pic2.BMP")), "an extension in any case is kept");
    assert!(fs::with(|fs| fs.stat("/st-pic2.BMP")).is_ok());
    keys(&mut p, &[c(' '), Key::Right, c(' ')]);
    keys(&mut p, &[Key::Ctrl(b'a')]);
    chars(&mut p, "st-pic");
    keys(&mut p, &[Key::Enter, c('y')]);
    assert_eq!((p.prompt_kind(), p.is_modified(), p.path()), (None, false, Some("/st-pic.bmp")));
    assert_eq!(bmp::decode(&fs::with(|fs| fs.read("/st-pic.bmp")).unwrap()).unwrap(), *p.image(), "y replaced it");
    // A name in a folder that does not exist is an error, and the picture stays modified.
    keys(&mut p, &[c(' '), c(' '), Key::Ctrl(b'a')]);
    chars(&mut p, "no-such-folder/x");
    keys(&mut p, &[Key::Enter]);
    assert!(p.message().starts_with(" Error: cannot save x.bmp"), "{}", p.message());
    assert!(p.is_modified());

    // Ctrl-N and Ctrl-L ask before they drop unsaved changes.
    keys(&mut p, &[Key::Ctrl(b'n')]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::DiscardNew));
    keys(&mut p, &[c('n')]);
    assert_eq!((p.prompt_kind(), p.image().width), (None, 33));
    keys(&mut p, &[Key::Ctrl(b'l')]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::DiscardLoad));
    keys(&mut p, &[Key::Escape]);
    keys(&mut p, &[Key::Ctrl(b'n'), c('y')]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::NewSize));
    assert_eq!(p.bar(), " New size WxH: 33x7  Enter ok  Esc no", "the current size is suggested");
    chars(&mut p, "5x4");
    keys(&mut p, &[Key::Enter]);
    assert_eq!((p.image().width, p.image().height, p.path(), p.is_modified()), (5, 4, None, false));
    assert_eq!(p.message(), " New canvas 5x4");
    keys(&mut p, &[Key::Ctrl(b'n')]);
    chars(&mut p, "0x4");
    keys(&mut p, &[Key::Enter]);
    assert!(p.message().contains("size must be WxH"), "{}", p.message());
    keys(&mut p, &[Key::Ctrl(b'n')]);
    chars(&mut p, "5000x5000");
    keys(&mut p, &[Key::Enter]);
    assert!(p.message().contains("size must be WxH"), "{}", p.message());
    // Ctrl-L: nothing changed, so no question; a good file loads, a bad one is an error.
    keys(&mut p, &[Key::Ctrl(b'l')]);
    assert_eq!(p.prompt_kind(), Some(PromptKind::Load));
    chars(&mut p, "st-nothing");
    keys(&mut p, &[Key::Enter]);
    assert_eq!(p.message(), " Error: cannot read st-nothing: not found");
    keys(&mut p, &[Key::Ctrl(b'l')]);
    chars(&mut p, "st-pic.bmp");
    keys(&mut p, &[Key::Enter]);
    assert_eq!(p.message(), " Loaded st-pic.bmp (33x7)");
    assert_eq!((p.image().width, p.path(), p.is_modified()), (33, Some("/st-pic.bmp"), false));
    keys(&mut p, &[Key::Ctrl(b'l')]);
    chars(&mut p, "/docs/index.txt");
    keys(&mut p, &[Key::Enter]);
    assert_eq!(p.message(), " Error: cannot read /docs/index.txt: not a BMP picture paint can open");
    assert_eq!(p.image().width, 33, "the picture stays");

    // A mouse drawing saved with Ctrl-S equals the canvas.
    let mut p = Paint::open("/", Some("st-pic.bmp"), None).unwrap();
    p.set_screen(80, 24, false, (8, 16));
    let (ox, oy) = p.canvas_origin();
    let at = |x: i32, y: i32| -> (u16, u16) {
        let (ax, ay) = (ox + x, oy + y);
        ((ax * 8 + 3) as u16, ((ay / 2) * 16 + (ay % 2) * 8 + 3) as u16)
    };
    let ev = |a: Action, (x, y): (u16, u16), buttons: u8| Key::Mouse(Event { action: a, x, y, dx: 0, dy: 0, buttons });
    p.handle(ev(Action::Down(Button::Left), at(1, 1), 1));
    p.handle(ev(Action::Move, at(30, 5), 1));
    p.handle(ev(Action::Up(Button::Left), at(30, 5), 0));
    keys(&mut p, &[Key::Ctrl(b'z'), Key::Ctrl(b'y')]);
    assert_eq!([p.image().get(1, 1), p.image().get(30, 5)], [Some(BLACK); 2], "the stroke is back after undo and redo");
    keys(&mut p, &[Key::Ctrl(b's')]);
    assert_eq!(bmp::decode(&fs::with(|fs| fs.read("/st-pic.bmp")).unwrap()).unwrap(), *p.image());
    fs::with(|fs| fs.remove("/st-pic.bmp", false)).unwrap();
    fs::with(|fs| fs.remove("/st-pic2.BMP", false)).unwrap();
}

/// A 1920x1200 canvas: the bucket fills all of it without a panic. The undo step for that is 9 MB
/// big: it is kept when the undo budget (a function of the heap) allows it, else paint says so.
#[cfg(feature = "qemu-test")]
pub fn big_canvas() {
    let Ok(img) = crate::picture::blank(1920, 1200, 0xFFFFFF) else {
        crate::println!("  (no memory for a 1920x1200 canvas here: skipped)");
        return;
    };
    let mut p = Paint::new(img, None, "/");
    keys(&mut p, &[c('b'), c(' ')]);
    assert!(p.is_modified());
    assert_eq!(count(&p, BLACK), 1920 * 1200, "the bucket filled the whole canvas");
    if 1920 * 1200 * 4 > super::undo_budget() {
        assert!(p.message().contains("Undo off"), "{}", p.message());
        assert_eq!(p.undo_depth(), (0, 0));
    } else {
        assert!(!p.message().contains("Undo off") || p.undo_depth() == (0, 0), "{}", p.message());
        if p.undo_depth() == (1, 0) {
            keys(&mut p, &[ctrl('z')]);
            assert_eq!(count(&p, BLACK), 0, "undo gives the white canvas back");
            keys(&mut p, &[ctrl('y')]);
            assert_eq!(count(&p, BLACK), 1920 * 1200);
        }
    }
    // A smaller change on a big canvas is an ordinary undo step.
    let before = p.undo_depth().0;
    keys(&mut p, &[c('p'), c(' '), Key::Right, Key::Right, c(' ')]);
    assert_eq!(p.undo_depth().0, before + 1);
}

/// B-008: only the part of a row that changed is written again (the x:y line must not rewrite
/// the cells over the canvas, which would erase its pixels, nor send the whole row to serial).
pub fn redraw_spans() {
    use super::render::changed_span;
    let row = |text: &str| -> alloc::vec::Vec<u16> { text.bytes().map(|b| b as u16 | 0x0700).collect() };
    let old = row("canvas....|x:5  y:7     ");
    assert_eq!(changed_span(&old, &old), None, "nothing changed");
    assert_eq!(changed_span(&old, &row("canvas....|x:6  y:7     ")), Some((13, 14)));
    assert_eq!(changed_span(&old, &row("canvas....|x:12  y:7    ")), Some((13, 20)));
    assert_eq!(changed_span(&old, &row("Canvas....|x:5  y:7    !")), Some((0, 24)));
    assert_eq!(changed_span(&[], &old), Some((0, 24)), "a row never drawn is drawn whole");
    assert_eq!(changed_span(&old, &old[..10]), Some((0, 10)), "another width: whole");
    assert_eq!(changed_span(&old, &[]), None);
    // Moving over the canvas changes only the x:y part of that row (and the cursor's cells).
    let mut p = paint_on(64, 40);
    p.set_screen(80, 25, true, (8, 16));
    let table = p.table();
    let xy = table.iter().position(|&r| r == Row::Xy).expect("an x:y row on 80x25");
    let before = p.row_cells_for_test(xy);
    keys(&mut p, &[Key::Right]);
    let after = p.row_cells_for_test(xy);
    let (c0, _) = changed_span(&before, &after).expect("x:y changed");
    assert!(c0 >= p.panel_col(), "only the panel part of the x:y row changes in graphics modes");
}
