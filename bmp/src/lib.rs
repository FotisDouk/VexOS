//! BMP: a pure `no_std` + `alloc` picture crate for the kernel-test OS. It has no dependencies and
//! uses integer maths only (the kernel target is soft-float), no recursion and no large stack
//! arrays, so the kernel's two viewers and paint can share it and `cargo test` covers it on the
//! host.
//!
//! * [`probe`], [`decode`], [`encode`], [`encoded_size`]: read and write BMP files (see
//!   `CLAUDE/product/specs/image-update.md`, "BMP: what is read and written").
//! * [`Image`] and [`Rect`]: a canvas of `0x00RRGGBB` pixels, clipped on every edge.
//! * [`fit`], [`scaled_row`], [`nearest`], [`quantize`], [`half_block`]: what the viewers need.
//! * [`brush_rect`], [`line_points`], [`ellipse_outline`], [`ellipse_spans`] and the drawing
//!   methods on [`Image`]: what paint needs.
//!
//! Every function works on any input without panicking.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod codec;
mod draw;
mod image;
mod ops;

pub use codec::{decode, encode, encoded_size, probe, Error, Header};
pub use draw::{brush_rect, ellipse_outline, ellipse_spans, line_points};
pub use image::{Image, Rect};
pub use ops::{fit, half_block, nearest, quantize, scaled_row, DITHER_STRENGTH};

/// The largest width or height of a picture, in pixels.
pub const MAX_SIDE: u32 = 4096;
/// The most pixels (width times height) a picture may have: 1920x1200.
pub const MAX_PIXELS: u64 = 2_304_000;
/// The width shown in the "too large" message.
pub const MAX_WIDTH_SHOWN: u32 = 1920;
/// The height shown in the "too large" message.
pub const MAX_HEIGHT_SHOWN: u32 = 1200;

#[cfg(test)]
mod tests;
