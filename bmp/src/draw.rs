//! Drawing for paint: brush, lines, rectangles, ellipses and flood fill. Everything clips at the
//! picture's edge; coordinates are `i32` and may start or end outside.

use alloc::vec::Vec;

use crate::image::{Image, Rect};
use crate::MAX_SIDE;

/// The brush: a `size` x `size` square (size 0 counts as 1, and the size is capped at 4096) whose
/// top-left corner is (x - (size-1)/2, y - (size-1)/2).
pub fn brush_rect(x: i32, y: i32, size: u32) -> Rect {
    let n = size.clamp(1, MAX_SIDE) as i32;
    let off = (n - 1) / 2;
    Rect { x: x.saturating_sub(off), y: y.saturating_sub(off), w: n, h: n }
}

/// Calls `f` for every point of the Bresenham line from (x0, y0) to (x1, y1), both ends included,
/// in order. The cost is proportional to the line's length.
pub fn line_points(x0: i32, y0: i32, x1: i32, y1: i32, mut f: impl FnMut(i32, i32)) {
    let (mut x, mut y) = (x0 as i64, y0 as i64);
    let (ex, ey) = (x1 as i64, y1 as i64);
    let dx = (ex - x).abs();
    let dy = -(ey - y).abs();
    let sx = if x < ex { 1 } else { -1 };
    let sy = if y < ey { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        f(x as i32, y as i32);
        if x == ex && y == ey {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// An ellipse in a box, described row by row.
///
/// A pixel (x, y) of a `w` x `h` box is on row `i = y - ya`. The true ellipse of semi-axes w/2
/// and h/2 fills the box; a row's span is the pixels whose centre is inside it. To make the
/// ellipse touch all four sides of the box, the two middle rows of an even height are given the
/// full width, and a row whose span would be empty gets the one or two centre pixels. A 1-wide
/// or 1-high box therefore gives a line and a 1x1 box a point.
struct Ellipse {
    xa: i64,
    ya: i64,
    w: i64,
    h: i64,
}

fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    let bits = 128 - n.leading_zeros();
    let mut x = 1u128 << ((bits + 1) / 2);
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

impl Ellipse {
    fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Ellipse {
        let (xa, xb) = (x0.min(x1) as i64, x0.max(x1) as i64);
        let (ya, yb) = (y0.min(y1) as i64, y0.max(y1) as i64);
        Ellipse { xa, ya, w: xb - xa + 1, h: yb - ya + 1 }
    }

    /// The first and last pixel column of row `i` (0 is the top row).
    fn span(&self, i: i64) -> (i64, i64) {
        let (xa, ww, hh) = (self.xa, self.w - 1, self.h - 1);
        let (w, h) = (self.w as i128, self.h as i128);
        if self.h == 1 {
            return (xa, xa + ww);
        }
        let v = 2 * i - hh;
        if self.h % 2 == 0 && v.abs() == 1 {
            return (xa, xa + ww);
        }
        let q = w * w * (h * h - (v as i128) * (v as i128)) / (h * h);
        let s = isqrt(q.max(0) as u128) as i64;
        let cx2 = 2 * xa + ww; // twice the centre column
        let l = (cx2 - s + 1).div_euclid(2);
        let r = (cx2 + s).div_euclid(2);
        if l > r {
            (cx2.div_euclid(2), (cx2 + 1).div_euclid(2))
        } else {
            (l, r)
        }
    }
}

/// Calls `f` for each point of the outline of the ellipse inscribed in the box with corners
/// (x0, y0) and (x1, y1) (any order). The outline is one pixel thick and 8-connected; no point is
/// given twice. Degenerate boxes work (a 1-wide or 1-high box is a line, 1x1 a point). The cost is
/// proportional to the box's height.
pub fn ellipse_outline(x0: i32, y0: i32, x1: i32, y1: i32, mut f: impl FnMut(i32, i32)) {
    let e = Ellipse::new(x0, y0, x1, y1);
    for i in 0..e.h {
        let (l, r) = e.span(i);
        let y = (e.ya + i) as i32;
        if i == 0 || i == e.h - 1 {
            for x in l..=r {
                f(x as i32, y);
            }
            continue;
        }
        // The neighbour row towards the nearer end of the ellipse is the narrower one.
        let n = if 2 * i <= e.h - 1 { e.span(i - 1) } else { e.span(i + 1) };
        let left_end = (n.0 - 1).max(l);
        let right_start = (n.1 + 1).min(r);
        if right_start <= left_end + 1 {
            for x in l..=r {
                f(x as i32, y);
            }
        } else {
            for x in l..=left_end {
                f(x as i32, y);
            }
            for x in right_start..=r {
                f(x as i32, y);
            }
        }
    }
}

/// Calls `span(y, xa, xb)` for every row of the filled ellipse inscribed in the box with corners
/// (x0, y0) and (x1, y1) (any order), top row first. The cost is proportional to the box's height.
pub fn ellipse_spans(x0: i32, y0: i32, x1: i32, y1: i32, mut span: impl FnMut(i32, i32, i32)) {
    let e = Ellipse::new(x0, y0, x1, y1);
    for i in 0..e.h {
        let (l, r) = e.span(i);
        span((e.ya + i) as i32, l as i32, r as i32);
    }
}

impl Image {
    /// Fills one row's pixels `xa..=xb` and returns the clipped rectangle.
    fn fill_span(&mut self, y: i32, xa: i32, xb: i32, rgb: u32) -> Rect {
        let r = Rect::from_corners(xa, y, xb, y).clip_to(self.width, self.height);
        self.fill_rect(r, rgb);
        r
    }

    /// Stamps the brush at (x, y). Returns the clipped rectangle it changed (may be empty).
    pub fn dot(&mut self, x: i32, y: i32, size: u32, rgb: u32) -> Rect {
        let r = brush_rect(x, y, size).clip_to(self.width, self.height);
        self.fill_rect(r, rgb);
        r
    }

    /// Draws a line with the brush stamped along it. Returns the clipped rectangle it changed.
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, size: u32, rgb: u32) -> Rect {
        let mut changed = Rect::EMPTY;
        line_points(x0, y0, x1, y1, |x, y| {
            changed = changed.union(self.dot(x, y, size, rgb));
        });
        changed
    }

    /// Draws the box with corners (x0, y0) and (x1, y1): an outline of `size` thickness drawn
    /// inward from the box, or solid if `filled`. Returns the clipped rectangle it changed.
    pub fn rect(
        &mut self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        size: u32,
        filled: bool,
        rgb: u32,
    ) -> Rect {
        let b = Rect::from_corners(x0, y0, x1, y1);
        let clipped = b.clip_to(self.width, self.height);
        if filled {
            self.fill_rect(b, rgb);
            return clipped;
        }
        let t = size.clamp(1, MAX_SIDE) as i64;
        let (bw, bh) = (b.w as i64, b.h as i64);
        let (tv, th) = (t.min(bh) as i32, t.min(bw) as i32);
        let (right, bottom) = (b.x as i64 + bw - th as i64, b.y as i64 + bh - tv as i64);
        let strips = [
            Rect::new(b.x, b.y, b.w, tv),
            Rect::new(b.x, bottom as i32, b.w, tv),
            Rect::new(b.x, b.y, th, b.h),
            Rect::new(right as i32, b.y, th, b.h),
        ];
        for s in strips {
            self.fill_rect(s, rgb);
        }
        clipped
    }

    /// Draws the ellipse inscribed in the box with corners (x0, y0) and (x1, y1): the outline
    /// stamped with the brush, or solid if `filled`. Returns the clipped rectangle it changed.
    pub fn ellipse(
        &mut self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        size: u32,
        filled: bool,
        rgb: u32,
    ) -> Rect {
        let mut changed = Rect::EMPTY;
        if filled {
            ellipse_spans(x0, y0, x1, y1, |y, xa, xb| {
                changed = changed.union(self.fill_span(y, xa, xb, rgb));
            });
        } else {
            ellipse_outline(x0, y0, x1, y1, |x, y| {
                changed = changed.union(self.dot(x, y, size, rgb));
            });
        }
        changed
    }

    /// Fills the 4-connected area of exactly the colour at (x, y) with `rgb`. It is iterative (a
    /// scanline fill with its own work list, one entry per run of pixels waiting to be filled),
    /// so it works on any picture without recursion. Returns the clipped rectangle it changed;
    /// empty if (x, y) is outside or already `rgb`.
    pub fn flood_fill(&mut self, x: i32, y: i32, rgb: u32) -> Rect {
        match self.try_flood_fill(x, y, rgb) {
            Ok(r) | Err(r) => r,
        }
    }

    /// `flood_fill`, but the work list grows only with memory the allocator can give
    /// (`try_reserve`), so a huge fill cannot fail an allocation. `Err` holds what was changed
    /// when memory ran out: the fill stopped part way.
    pub fn try_flood_fill(&mut self, x: i32, y: i32, rgb: u32) -> Result<Rect, Rect> {
        let target = match self.get(x, y) {
            Some(t) if t != rgb => t,
            _ => return Ok(Rect::EMPTY),
        };
        let w = self.width as usize;
        let h = self.height as usize;
        if w * h > u32::MAX as usize || self.pixels.len() < w * h {
            return Ok(Rect::EMPTY);
        }
        let (mut minx, mut maxx, mut miny, mut maxy) = (usize::MAX, 0usize, usize::MAX, 0usize);
        let mut work: Vec<u32> = Vec::new();
        let mut out_of_memory = false;
        if work.try_reserve(1).is_err() {
            return Err(Rect::EMPTY);
        }
        work.push((y as usize * w + x as usize) as u32);
        while let Some(seed) = work.pop() {
            if out_of_memory {
                break;
            }
            let (sy, sx) = (seed as usize / w, seed as usize % w);
            let row = sy * w;
            if self.pixels[row + sx] != target {
                continue;
            }
            let mut l = sx;
            while l > 0 && self.pixels[row + l - 1] == target {
                l -= 1;
            }
            let mut r = sx;
            while r + 1 < w && self.pixels[row + r + 1] == target {
                r += 1;
            }
            self.pixels[row + l..=row + r].fill(rgb);
            minx = minx.min(l);
            maxx = maxx.max(r);
            miny = miny.min(sy);
            maxy = maxy.max(sy);
            for ny in [sy.wrapping_sub(1), sy + 1] {
                if ny >= h {
                    continue;
                }
                let nrow = ny * w;
                let mut in_run = false;
                for xx in l..=r {
                    if self.pixels[nrow + xx] == target {
                        if !in_run {
                            if work.try_reserve(1).is_err() {
                                out_of_memory = true;
                                break;
                            }
                            work.push((nrow + xx) as u32);
                            in_run = true;
                        }
                    } else {
                        in_run = false;
                    }
                }
            }
        }
        let changed = Rect::new(minx as i32, miny as i32, (maxx - minx + 1) as i32, (maxy - miny + 1) as i32);
        if out_of_memory {
            Err(changed)
        } else {
            Ok(changed)
        }
    }
}
