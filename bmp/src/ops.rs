//! What the viewers need: fitting, scaling, colour reduction and text-cell output.

use crate::image::Image;

/// The largest offset the dither adds to or subtracts from a channel.
pub const DITHER_STRENGTH: i32 = 24;

/// The largest (w, h) that fits in (area_w, area_h) with the aspect ratio kept. Bigger pictures
/// shrink (any ratio, at least 1 px each side); smaller ones: if `enlarge`, they grow by the
/// largest WHOLE factor that fits (2x, 3x, ...), otherwise they stay 1:1. Returns (0, 0) if the
/// picture or the area is empty.
pub fn fit(w: u32, h: u32, area_w: u32, area_h: u32, enlarge: bool) -> (u32, u32) {
    if w == 0 || h == 0 || area_w == 0 || area_h == 0 {
        return (0, 0);
    }
    if w <= area_w && h <= area_h {
        if !enlarge {
            return (w, h);
        }
        let f = (area_w / w).min(area_h / h).max(1);
        return (w * f, h * f);
    }
    let (w64, h64, aw, ah) = (w as u64, h as u64, area_w as u64, area_h as u64);
    if w64 * ah >= h64 * aw {
        // limited by the width
        (area_w, ((h64 * aw / w64).max(1)) as u32)
    } else {
        (((w64 * ah / h64).max(1)) as u32, area_h)
    }
}

/// The source range [a, b) of output pixel `i` when `s` source pixels become `d`.
fn source_range(i: u64, s: u64, d: u64) -> (u64, u64) {
    if d >= s {
        let c = ((2 * i + 1) * s / (2 * d)).min(s - 1);
        (c, c + 1)
    } else {
        let a = (i * s / d).min(s - 1);
        let b = ((i + 1) * s / d).max(a + 1).min(s);
        (a, b)
    }
}

/// One output row of the picture scaled to `dst_w` x `dst_h`: output pixels `dst_x0 ..
/// dst_x0 + out.len()` of row `dst_y` (so a viewer can pan a zoomed picture without making a big
/// buffer). A dimension that shrinks is area-averaged (each output pixel averages its integer
/// source box, at least 1 px, rounded to nearest); one that grows or stays is nearest-pixel. The
/// two dimensions decide independently. Pixels outside the scaled picture are 0.
pub fn scaled_row(src: &Image, dst_w: u32, dst_h: u32, dst_y: u32, dst_x0: u32, out: &mut [u32]) {
    if src.width == 0 || src.height == 0 || dst_w == 0 || dst_h == 0 || dst_y >= dst_h {
        out.fill(0);
        return;
    }
    let (sw, sh, dw, dh) = (src.width as u64, src.height as u64, dst_w as u64, dst_h as u64);
    let (y0, y1) = source_range(dst_y as u64, sh, dh);
    for (i, o) in out.iter_mut().enumerate() {
        let x = dst_x0 as u64 + i as u64;
        if x >= dw {
            *o = 0;
            continue;
        }
        let (x0, x1) = source_range(x, sw, dw);
        let px = |sx: u64, sy: u64| src.pixels.get((sy * sw + sx) as usize).copied().unwrap_or(0);
        if x1 - x0 == 1 && y1 - y0 == 1 {
            *o = px(x0, y0);
            continue;
        }
        let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
        for sy in y0..y1 {
            for sx in x0..x1 {
                let p = px(sx, sy);
                r += (p >> 16 & 255) as u64;
                g += (p >> 8 & 255) as u64;
                b += (p & 255) as u64;
            }
        }
        let n = (x1 - x0) * (y1 - y0);
        let avg = |v: u64| ((v + n / 2) / n) as u32;
        *o = avg(r) << 16 | avg(g) << 8 | avg(b);
    }
}

/// The index of the palette colour nearest to `rgb` by `2*dr^2 + 4*dg^2 + 3*db^2`; the lowest
/// index wins a tie.
pub fn nearest(palette: &[[u8; 3]; 16], rgb: u32) -> u8 {
    let (r, g, b) = ((rgb >> 16 & 255) as i32, (rgb >> 8 & 255) as i32, (rgb & 255) as i32);
    let mut best = 0u8;
    let mut best_cost = i32::MAX;
    for (i, p) in palette.iter().enumerate() {
        let (dr, dg, db) = (r - p[0] as i32, g - p[1] as i32, b - p[2] as i32);
        let cost = 2 * dr * dr + 4 * dg * dg + 3 * db * db;
        if cost < best_cost {
            best_cost = cost;
            best = i as u8;
        }
    }
    best
}

const BAYER: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Maps a colour to a palette index. With `dither`, an ordered 4x4 Bayer offset (centred on 0, at
/// most `DITHER_STRENGTH` per channel, the pattern chosen by the pixel position `x`, `y`) is added
/// first and the result is clamped to 0..255. A colour that is exactly a palette colour is never
/// dithered.
pub fn quantize(palette: &[[u8; 3]; 16], rgb: u32, x: u32, y: u32, dither: bool) -> u8 {
    let exact = nearest(palette, rgb);
    if !dither {
        return exact;
    }
    let e = palette[exact as usize];
    if e[0] as u32 == rgb >> 16 & 255 && e[1] as u32 == rgb >> 8 & 255 && e[2] as u32 == rgb & 255 {
        return exact;
    }
    let m = BAYER[(y & 3) as usize][(x & 3) as usize];
    let off = (2 * m - 15) * DITHER_STRENGTH / 15;
    let ch = |shift: u32| ((rgb >> shift & 255) as i32 + off).clamp(0, 255) as u32;
    nearest(palette, ch(16) << 16 | ch(8) << 8 | ch(0))
}

/// A VGA text cell (`char | attr << 8`) showing two stacked pixels with colour indexes `upper` and
/// `lower`: character 0xDF (upper half block) with the upper colour as foreground (attribute low
/// nibble) and the lower as background (high nibble). If both are the same colour: a space (0x20)
/// on that background.
pub fn half_block(upper: u8, lower: u8) -> u16 {
    let attr = (upper & 15) as u16 | ((lower & 15) as u16) << 4;
    let ch = if upper & 15 == lower & 15 { 0x20 } else { 0xDF };
    ch | attr << 8
}
