//! Loading and saving pictures (BMP, through the `bmp` crate), for `view`, `gview` and paint.
//!
//! A picture's size comes from the user, and a failed allocation panics the kernel, so every
//! allocation that grows with a picture is checked first with `fits`: the file before it is
//! read, the decoded pixels before they are decoded, the encoded file before it is written.
//! Paint checks its canvas and undo steps the same way.

use crate::{fs, heap};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bmp::Image;

/// Memory left over after a picture's allocation, so the rest of the system keeps working.
const RESERVE: usize = 512 * 1024;

/// True if an allocation of `bytes` can be made and still leave `RESERVE` free.
pub fn fits(bytes: usize) -> bool {
    bytes.saturating_add(RESERVE) <= heap::largest_free()
}

/// Why a picture could not be opened or saved. `Display` gives the text after `<cmd>: <name>: `.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Error {
    NotFound,
    /// A folder, not a file.
    Folder,
    Bmp(bmp::Error),
    /// Not enough memory: the bytes needed.
    Memory(usize),
    /// Not enough free disk space in one piece: the bytes needed and the largest free run.
    Disk { needed: usize, free: usize },
    Fs(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Error::NotFound => f.write_str("not found"),
            Error::Folder => f.write_str("is a folder"),
            Error::Bmp(e) => write!(f, "{e}"),
            // The free memory is not shown: by the time this is printed the file that was
            // loaded first may have been freed, so the number would not explain the refusal.
            Error::Memory(bytes) => write!(f, "not enough memory (needs {} KiB in one piece)", bytes.div_ceil(1024)),
            Error::Disk { needed, free } => write!(
                f,
                "not enough free disk space (needs {} KiB in one piece; the largest free piece is {} KiB)",
                needed.div_ceil(1024),
                free / 1024
            ),
            Error::Fs(e) => f.write_str(e),
        }
    }
}

fn fs_error(e: fs::FsError) -> Error {
    match e {
        fs::FsError::Fs(kfs::Error::NotFound) => Error::NotFound,
        e => Error::Fs(format!("{e}")),
    }
}

/// Reads and decodes the picture at `path` (absolute). Any file is tried, whatever its name.
pub fn load(path: &str) -> Result<Image, Error> {
    let info = fs::with(|fs| fs.stat(path)).map_err(fs_error)?;
    if info.is_dir {
        return Err(Error::Folder);
    }
    if !fits(info.size() as usize) {
        return Err(Error::Memory(info.size() as usize));
    }
    let data = fs::with(|fs| fs.read(path)).map_err(fs_error)?;
    let header = bmp::probe(&data).map_err(Error::Bmp)?;
    let bytes = header.pixel_bytes() as usize;
    if !fits(bytes) {
        return Err(Error::Memory(bytes));
    }
    bmp::decode(&data).map_err(Error::Bmp)
}

/// Whether a file of `bytes` can be written to `path` now: a file is one extent, and the new
/// extent is allocated before an old file's is freed, so it needs a free run of that size.
fn disk_room(path: &str, bytes: usize) -> Result<(), Error> {
    let (free, in_place) = fs::with(|fs| {
        // A file in the boot area is written in place (and kfs checks its size).
        let in_place = fs.stat(path).is_ok_and(|info| info.extent.is_boot_area());
        Ok((fs.largest_free_run() as usize, in_place))
    })
    .map_err(fs_error)?;
    room(bytes, free, in_place)
}

/// The rule of `disk_room`, pure: a file of `bytes` needs that many whole sectors in one free run
/// of `free_run` sectors, unless it is written in place.
pub fn room(bytes: usize, free_run: usize, in_place: bool) -> Result<(), Error> {
    if in_place || bytes.div_ceil(kfs::SECTOR) <= free_run {
        return Ok(());
    }
    Err(Error::Disk { needed: bytes, free: free_run * kfs::SECTOR })
}

/// Encodes `image` as a 24-bit BMP and writes it to `path` (absolute). Returns the file size.
/// Refuses before encoding if the disk has no room for it, so a failed save changes nothing.
pub fn save(path: &str, image: &Image) -> Result<usize, Error> {
    let bytes = bmp::encoded_size(image.width, image.height);
    disk_room(path, bytes)?;
    if !fits(bytes) {
        return Err(Error::Memory(bytes));
    }
    let data: Vec<u8> = bmp::encode(image);
    fs::with(|fs| fs.write(path, &data)).map_err(|e| Error::Fs(format!("{e}")))?;
    Ok(data.len())
}

/// A blank picture of `width` x `height` in `rgb`, if the size is allowed and fits in memory.
pub fn blank(width: u32, height: u32, rgb: u32) -> Result<Image, Error> {
    if width == 0 || height == 0 || width > bmp::MAX_SIDE || height > bmp::MAX_SIDE || width as u64 * height as u64 > bmp::MAX_PIXELS {
        return Err(Error::Bmp(bmp::Error::TooLarge { width, height }));
    }
    let bytes = width as usize * height as usize * 4;
    if !fits(bytes) {
        return Err(Error::Memory(bytes));
    }
    Ok(Image::new(width, height, rgb))
}
