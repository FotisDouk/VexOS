//! The canvas and the rectangle type.

use alloc::vec;
use alloc::vec::Vec;

/// Clamps an `i64` into the `i32` range.
pub(crate) fn to_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// A rectangle of pixels: top-left corner and size. A rectangle with `w <= 0` or `h <= 0` is empty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in pixels.
    pub w: i32,
    /// Height in pixels.
    pub h: i32,
}

impl Rect {
    /// The empty rectangle at the origin.
    pub const EMPTY: Rect = Rect { x: 0, y: 0, w: 0, h: 0 };

    /// A rectangle from its corner and size.
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    /// The rectangle with the two corner pixels (both included, in any order).
    pub fn from_corners(x0: i32, y0: i32, x1: i32, y1: i32) -> Rect {
        let (xa, xb) = (x0.min(x1), x0.max(x1));
        let (ya, yb) = (y0.min(y1), y0.max(y1));
        Rect {
            x: xa,
            y: ya,
            w: to_i32(xb as i64 - xa as i64 + 1),
            h: to_i32(yb as i64 - ya as i64 + 1),
        }
    }

    /// True if the rectangle has no pixels.
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// True if the pixel (x, y) is inside.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        !self.is_empty()
            && x >= self.x
            && y >= self.y
            && (x as i64) < self.x as i64 + self.w as i64
            && (y as i64) < self.y as i64 + self.h as i64
    }

    /// The smallest rectangle holding both (an empty one adds nothing).
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return if other.is_empty() { Rect::EMPTY } else { other };
        }
        if other.is_empty() {
            return self;
        }
        let l = self.x.min(other.x) as i64;
        let t = self.y.min(other.y) as i64;
        let r = (self.x as i64 + self.w as i64).max(other.x as i64 + other.w as i64);
        let b = (self.y as i64 + self.h as i64).max(other.y as i64 + other.h as i64);
        Rect { x: l as i32, y: t as i32, w: to_i32(r - l), h: to_i32(b - t) }
    }

    /// The part inside a `width` x `height` image (`EMPTY` if none).
    pub fn clip_to(self, width: u32, height: u32) -> Rect {
        if self.is_empty() {
            return Rect::EMPTY;
        }
        let l = (self.x as i64).max(0);
        let t = (self.y as i64).max(0);
        let r = (self.x as i64 + self.w as i64).min(width as i64);
        let b = (self.y as i64 + self.h as i64).min(height as i64);
        if r <= l || b <= t {
            return Rect::EMPTY;
        }
        Rect { x: l as i32, y: t as i32, w: (r - l) as i32, h: (b - t) as i32 }
    }
}

/// A picture: pixels are `0x00RRGGBB`, rows top to bottom. `pixels.len()` is `width * height`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The pixels, row after row, top row first.
    pub pixels: Vec<u32>,
}

impl Image {
    /// A `width` x `height` picture of one colour.
    pub fn new(width: u32, height: u32, fill: u32) -> Image {
        Image { width, height, pixels: vec![fill; width as usize * height as usize] }
    }

    /// The index of the pixel (x, y), if it is inside.
    fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return None;
        }
        Some(y as usize * self.width as usize + x as usize)
    }

    /// The pixel at (x, y), or `None` outside the picture.
    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        self.index(x, y).and_then(|i| self.pixels.get(i).copied())
    }

    /// Sets the pixel at (x, y); outside the picture it does nothing.
    pub fn set(&mut self, x: i32, y: i32, rgb: u32) {
        if let Some(i) = self.index(x, y) {
            if let Some(p) = self.pixels.get_mut(i) {
                *p = rgb;
            }
        }
    }

    /// Fills the part of `r` that is inside the picture.
    pub fn fill_rect(&mut self, r: Rect, rgb: u32) {
        let c = r.clip_to(self.width, self.height);
        if c.is_empty() {
            return;
        }
        let w = self.width as usize;
        for y in c.y as usize..(c.y + c.h) as usize {
            let start = y * w + c.x as usize;
            if let Some(row) = self.pixels.get_mut(start..start + c.w as usize) {
                row.fill(rgb);
            }
        }
    }

    /// The pixels of `r` (clipped to the picture) row after row, for undo. Returns exactly
    /// `clipped.w * clipped.h` words.
    pub fn copy_rect(&self, r: Rect) -> Vec<u32> {
        let c = r.clip_to(self.width, self.height);
        let mut out = Vec::new();
        if c.is_empty() {
            return out;
        }
        out.reserve(c.w as usize * c.h as usize);
        let w = self.width as usize;
        for y in c.y as usize..(c.y + c.h) as usize {
            let start = y * w + c.x as usize;
            if let Some(row) = self.pixels.get(start..start + c.w as usize) {
                out.extend_from_slice(row);
            }
        }
        out
    }

    /// Writes `data` (laid out as `copy_rect` made it, `r.w` words per row) back into `r`. Parts
    /// outside the picture are skipped, and so is anything `data` is too short for.
    pub fn paste_rect(&mut self, r: Rect, data: &[u32]) {
        let c = r.clip_to(self.width, self.height);
        if c.is_empty() {
            return;
        }
        let w = self.width as usize;
        for y in c.y..c.y + c.h {
            let src = (y - r.y) as usize * r.w as usize + (c.x - r.x) as usize;
            let dst = y as usize * w + c.x as usize;
            let n = c.w as usize;
            match (data.get(src..src + n), self.pixels.get_mut(dst..dst + n)) {
                (Some(from), Some(to)) => to.copy_from_slice(from),
                _ => return,
            }
        }
    }
}
