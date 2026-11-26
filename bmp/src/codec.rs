//! Reading and writing BMP files.

use alloc::vec::Vec;
use core::fmt;

use crate::image::Image;
use crate::{MAX_HEIGHT_SHOWN, MAX_PIXELS, MAX_SIDE, MAX_WIDTH_SHOWN};

/// Why a BMP could not be read. `Display` gives the message the kernel prints after
/// `view: x.bmp: `.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// Not a BMP file (no `BM`, a header that is too short, or a side of 0).
    NotBmp,
    /// RLE, JPEG or PNG data inside the BMP.
    Compressed,
    /// A 16-bit BMP.
    SixteenBit,
    /// A valid BMP of a kind that is not read (OS/2 header, other masks, other depths...).
    Unsupported,
    /// The palette or the pixel data runs past the end of the file.
    CutShort,
    /// A side above 4096 or more than 2,304,000 pixels.
    TooLarge {
        /// The picture's width.
        width: u32,
        /// The picture's height.
        height: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotBmp => f.write_str("not a BMP picture"),
            Error::Compressed => {
                f.write_str("compressed BMP is not supported (save it uncompressed)")
            }
            Error::SixteenBit => f.write_str("16-bit BMP is not supported"),
            Error::Unsupported => f.write_str("this kind of BMP is not supported"),
            Error::CutShort => f.write_str("the file is cut short"),
            Error::TooLarge { width, height } => write!(
                f,
                "{}x{} is too large (at most {}x{})",
                width, height, MAX_WIDTH_SHOWN, MAX_HEIGHT_SHOWN
            ),
        }
    }
}

/// What `probe` learned from a valid file. Every byte range it names is inside the data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    /// Width in pixels (1..=4096).
    pub width: u32,
    /// Height in pixels (1..=4096).
    pub height: u32,
    /// Bits per pixel: 1, 4, 8, 24 or 32.
    pub bpp: u16,
    /// True if the rows are stored top to bottom (negative height in the file).
    pub top_down: bool,
    /// Where the pixel data starts in the file (`bfOffBits`).
    pub pixel_offset: usize,
    /// Bytes per stored row, padding included.
    pub row_bytes: usize,
    /// Where the palette starts in the file (1, 4 and 8 bit only).
    pub palette_offset: usize,
    /// How many palette entries there are (0 for 24 and 32 bit).
    pub palette_len: usize,
}

impl Header {
    /// The memory `decode` needs for the pixels, in bytes (`width * height * 4`).
    pub fn pixel_bytes(&self) -> u64 {
        self.width as u64 * self.height as u64 * 4
    }
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    d.get(o..o + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    d.get(o..o + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Checks the headers, the size limits and that the palette and pixel data are inside `data`,
/// without allocating, so the kernel can check the free heap (`Header::pixel_bytes`) before
/// calling `decode`. A too-large size is reported before a cut-short file.
pub fn probe(data: &[u8]) -> Result<Header, Error> {
    if data.len() < 18 || &data[0..2] != b"BM" {
        return Err(Error::NotBmp);
    }
    let dib = u32_at(data, 14);
    if dib == 12 {
        return Err(Error::Unsupported);
    }
    if dib < 40 || data.len() < 54 {
        return Err(Error::NotBmp);
    }
    let width = u32_at(data, 18) as i32;
    let height = u32_at(data, 22) as i32;
    if width <= 0 || height == 0 {
        return Err(Error::NotBmp);
    }
    let planes = u16_at(data, 26);
    let bpp = u16_at(data, 28);
    let compression = u32_at(data, 30);
    let clr_used = u32_at(data, 46);

    if matches!(compression, 1 | 2 | 4 | 5) {
        return Err(Error::Compressed);
    }
    if bpp == 16 {
        return Err(Error::SixteenBit);
    }
    if planes != 1 || !matches!(bpp, 1 | 4 | 8 | 24 | 32) {
        return Err(Error::Unsupported);
    }
    match compression {
        0 => {}
        3 if bpp == 32 => {
            if data.len() < 66 {
                return Err(Error::CutShort);
            }
            if u32_at(data, 54) != 0x00FF_0000
                || u32_at(data, 58) != 0x0000_FF00
                || u32_at(data, 62) != 0x0000_00FF
            {
                return Err(Error::Unsupported);
            }
        }
        _ => return Err(Error::Unsupported),
    }

    let w = width as u32;
    let top_down = height < 0;
    let h = height.unsigned_abs();
    if w > MAX_SIDE || h > MAX_SIDE || w as u64 * h as u64 > MAX_PIXELS {
        return Err(Error::TooLarge { width: w, height: h });
    }

    let palette_offset = 14 + dib as u64;
    let mut palette_len = 0usize;
    if bpp <= 8 {
        let max = 1u32 << bpp;
        let n = if clr_used == 0 || clr_used > max { max } else { clr_used };
        if palette_offset + n as u64 * 4 > data.len() as u64 {
            return Err(Error::CutShort);
        }
        palette_len = n as usize;
    }

    let row_bytes = ((w as u64 * bpp as u64 + 31) / 32) * 4;
    let pixel_offset = u32_at(data, 10) as u64;
    if pixel_offset + row_bytes * h as u64 > data.len() as u64 {
        return Err(Error::CutShort);
    }
    Ok(Header {
        width: w,
        height: h,
        bpp,
        top_down,
        pixel_offset: pixel_offset as usize,
        row_bytes: row_bytes as usize,
        palette_offset: if palette_len > 0 { palette_offset as usize } else { 0 },
        palette_len,
    })
}

/// Reads a BMP into an `Image` (see `probe` for what is accepted).
pub fn decode(data: &[u8]) -> Result<Image, Error> {
    let hd = probe(data)?;
    let (w, h) = (hd.width as usize, hd.height as usize);
    let mut palette: Vec<u32> = Vec::with_capacity(hd.palette_len);
    for i in 0..hd.palette_len {
        let o = hd.palette_offset + i * 4;
        let (b, g, r) = (data[o] as u32, data[o + 1] as u32, data[o + 2] as u32);
        palette.push(r << 16 | g << 8 | b);
    }
    let color = |i: usize| palette.get(i).copied().unwrap_or(0);

    let mut img = Image::new(hd.width, hd.height, 0);
    for fy in 0..h {
        let dy = if hd.top_down { fy } else { h - 1 - fy };
        let start = hd.pixel_offset + fy * hd.row_bytes;
        let src = &data[start..start + hd.row_bytes];
        let dst = &mut img.pixels[dy * w..(dy + 1) * w];
        match hd.bpp {
            1 => {
                for (x, d) in dst.iter_mut().enumerate() {
                    *d = color(((src[x >> 3] >> (7 - (x & 7))) & 1) as usize);
                }
            }
            4 => {
                for (x, d) in dst.iter_mut().enumerate() {
                    let b = src[x >> 1];
                    *d = color(if x & 1 == 0 { b >> 4 } else { b & 15 } as usize);
                }
            }
            8 => {
                for (x, d) in dst.iter_mut().enumerate() {
                    *d = color(src[x] as usize);
                }
            }
            24 => {
                for (x, d) in dst.iter_mut().enumerate() {
                    let p = &src[x * 3..x * 3 + 3];
                    *d = (p[2] as u32) << 16 | (p[1] as u32) << 8 | p[0] as u32;
                }
            }
            _ => {
                for (x, d) in dst.iter_mut().enumerate() {
                    let p = &src[x * 4..x * 4 + 4];
                    *d = (p[2] as u32) << 16 | (p[1] as u32) << 8 | p[0] as u32;
                }
            }
        }
    }
    Ok(img)
}

/// The size of the file `encode` makes: 54 + padded row * height.
pub fn encoded_size(width: u32, height: u32) -> usize {
    54 + ((width as usize * 3 + 3) & !3) * height as usize
}

/// Writes a 24-bit BI_RGB bottom-up BMP with a 40-byte BITMAPINFOHEADER, rows padded to 4 bytes,
/// no palette, 2835 pixels per metre both ways and `biSizeImage` filled in.
pub fn encode(image: &Image) -> Vec<u8> {
    let (w, h) = (image.width as usize, image.height as usize);
    let row = (w * 3 + 3) & !3;
    let total = encoded_size(image.width, image.height);
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&image.width.to_le_bytes());
    out.extend_from_slice(&image.height.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&((row * h) as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        for x in 0..w {
            let p = image.pixels.get(y * w + x).copied().unwrap_or(0);
            out.push(p as u8);
            out.push((p >> 8) as u8);
            out.push((p >> 16) as u8);
        }
        for _ in w * 3..row {
            out.push(0);
        }
    }
    out
}
