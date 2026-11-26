//! KFS: the kernel-test file system. Shared by the kernel (on the boot disk) and the xtask (which
//! builds the disk image), so both agree on the on-disk format.
//!
//! The file system covers the whole disk, in 512-byte sectors:
//!
//! | LBA                | contents                                                     |
//! |--------------------|--------------------------------------------------------------|
//! | 0 .. 17            | the boot area: MBR + stage2, visible as `/system/boot.boot`  |
//! | 17                 | superblock                                                   |
//! | 18 ..              | allocation bitmap, one bit per sector of the disk            |
//! | after the bitmap   | data: directories and files                                  |
//!
//! Every file and directory is one contiguous extent (start sector, sector count, byte size).
//! Writing a file allocates a new extent, writes it, then frees the old one. Directories are
//! files made of 64-byte entries. The superblock also records where `/system/kernel.sys` is, so
//! stage2 can load the kernel without understanding directories.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

pub const SECTOR: usize = 512;
/// Stage1 (1 sector) + stage2 (16 sectors). Must match `STAGE2_SECTORS` in config.inc.
pub const BOOT_SECTORS: u32 = 17;
pub const SUPERBLOCK_LBA: u32 = BOOT_SECTORS;
pub const BITMAP_LBA: u32 = SUPERBLOCK_LBA + 1;

pub const MAGIC: u32 = u32::from_le_bytes(*b"KTFS");
pub const VERSION: u32 = 1;

/// Superblock field offsets. `SB_MAGIC`, `SB_KERNEL_START` and `SB_KERNEL_SECTORS` are also used
/// by stage2 (config.inc); keep them in sync.
pub const SB_MAGIC: usize = 0;
pub const SB_VERSION: usize = 4;
pub const SB_TOTAL_SECTORS: usize = 8;
pub const SB_BITMAP_SECTORS: usize = 12;
pub const SB_DATA_LBA: usize = 16;
pub const SB_ROOT_START: usize = 20;
pub const SB_ROOT_SECTORS: usize = 24;
pub const SB_KERNEL_START: usize = 28;
pub const SB_KERNEL_SECTORS: usize = 32;
pub const SB_KERNEL_SIZE: usize = 36;

pub const BOOT_PATH: &str = "/system/boot.boot";
pub const KERNEL_PATH: &str = "/system/kernel.sys";

pub const ENTRY_SIZE: usize = 64;
pub const MAX_NAME: usize = 47;
const NAME_FIELD: usize = 48;
const KIND_FREE: u8 = 0;
const KIND_FILE: u8 = 1;
const KIND_DIR: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Io,
    NotFormatted,
    NotFound,
    NotADirectory,
    IsADirectory,
    AlreadyExists,
    DirectoryNotEmpty,
    NoSpace,
    InvalidName,
    /// The root directory cannot be removed, moved or written as a file.
    Root,
    /// The boot area holds at most `BOOT_SECTORS` sectors.
    BootTooLarge,
    /// A directory cannot be moved into itself.
    MoveIntoSelf,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Error::Io => "disk I/O error",
            Error::NotFormatted => "no KFS file system on this disk",
            Error::NotFound => "no such file or directory",
            Error::NotADirectory => "not a directory",
            Error::IsADirectory => "is a directory",
            Error::AlreadyExists => "already exists",
            Error::DirectoryNotEmpty => "directory not empty",
            Error::NoSpace => "not enough free space on the disk",
            Error::InvalidName => "invalid name (1-47 printable characters, no / or \")",
            Error::Root => "not allowed on the root directory",
            Error::BootTooLarge => "the bootloader must fit in 17 sectors (8704 bytes)",
            Error::MoveIntoSelf => "cannot move a directory into itself",
        })
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// A disk, addressed in 512-byte sectors. Buffers are always whole sectors.
pub trait BlockDevice {
    fn sector_count(&self) -> u64;
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<()>;
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<()>;
}

impl<T: BlockDevice + ?Sized> BlockDevice for alloc::boxed::Box<T> {
    fn sector_count(&self) -> u64 {
        (**self).sector_count()
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read(lba, buf)
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<()> {
        (**self).write(lba, buf)
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for &mut T {
    fn sector_count(&self) -> u64 {
        (**self).sector_count()
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read(lba, buf)
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<()> {
        (**self).write(lba, buf)
    }
}

/// A disk in memory: used by the xtask to build images and by tests.
pub struct MemDisk {
    pub data: Vec<u8>,
}

impl MemDisk {
    pub fn new(sectors: usize) -> Self {
        Self { data: vec![0; sectors * SECTOR] }
    }
}

impl BlockDevice for MemDisk {
    fn sector_count(&self) -> u64 {
        (self.data.len() / SECTOR) as u64
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        let start = lba as usize * SECTOR;
        let src = self.data.get(start..start + buf.len()).ok_or(Error::Io)?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<()> {
        let start = lba as usize * SECTOR;
        let dst = self.data.get_mut(start..start + buf.len()).ok_or(Error::Io)?;
        dst.copy_from_slice(buf);
        Ok(())
    }
}

/// File types come from the extension only. Matching ignores case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    /// `.txt`, and any file without a known extension.
    Text,
    /// `.asm`: assembly source.
    Asm,
    /// `.vex`: executable.
    Exe,
    /// `.v`: script of shell commands.
    Script,
    /// `.sys`: kernel image.
    Kernel,
    /// `.boot`: bootloader image.
    Boot,
    /// `.raw`: raw data.
    Raw,
    /// `.bmp`: a picture (opened by `view`, `gview` and `paint`).
    Image,
    Dir,
}

impl FileType {
    pub fn from_name(name: &str) -> FileType {
        let ext = match name.rfind('.') {
            Some(dot) if dot > 0 => &name[dot + 1..],
            _ => "",
        };
        let is = |e: &str| ext.eq_ignore_ascii_case(e);
        if is("asm") {
            FileType::Asm
        } else if is("vex") {
            FileType::Exe
        } else if is("v") {
            FileType::Script
        } else if is("sys") {
            FileType::Kernel
        } else if is("boot") {
            FileType::Boot
        } else if is("raw") {
            FileType::Raw
        } else if is("bmp") {
            FileType::Image
        } else {
            FileType::Text
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FileType::Text => "text",
            FileType::Asm => "asm",
            FileType::Exe => "vex",
            FileType::Script => "script",
            FileType::Kernel => "kernel",
            FileType::Boot => "boot",
            FileType::Raw => "raw",
            FileType::Image => "image",
            FileType::Dir => "dir",
        }
    }

    /// Shown as text by `rd`; everything else is shown as a hex dump.
    pub fn is_text(self) -> bool {
        matches!(self, FileType::Text | FileType::Asm | FileType::Script)
    }

    pub fn is_runnable(self) -> bool {
        matches!(self, FileType::Asm | FileType::Exe | FileType::Script)
    }

    /// Needed to boot the disk.
    pub fn is_system(self) -> bool {
        matches!(self, FileType::Kernel | FileType::Boot)
    }
}

/// Checks a single file or directory name.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name != "."
        && name != ".."
        && name.bytes().all(|b| (0x20..0x7F).contains(&b) && b != b'/' && b != b'"')
}

/// Turns `path` (absolute, or relative to `cwd`) into a normalized absolute path such as `/a/b`.
/// `.` and `..` are resolved by name; `..` at the root stays at the root.
pub fn resolve(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let full = if path.starts_with('/') { [path, ""] } else { [cwd, path] };
    for piece in full.iter().flat_map(|p| p.split('/')) {
        match piece {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut out = String::new();
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|p| !p.is_empty()).collect()
}

/// Splits a normalized absolute path into (parent, name). The root has no parent.
fn split_parent(path: &str) -> Option<(&str, &str)> {
    let path = path.trim_end_matches('/');
    let slash = path.rfind('/')?;
    let name = &path[slash + 1..];
    if name.is_empty() {
        return None;
    }
    Some((if slash == 0 { "/" } else { &path[..slash] }, name))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub start: u32,
    pub sectors: u32,
    pub size: u32,
}

impl Extent {
    const EMPTY: Extent = Extent { start: 0, sectors: 0, size: 0 };

    /// The boot area is the only extent that starts at LBA 0 and is not empty.
    pub fn is_boot_area(&self) -> bool {
        self.start == 0 && self.sectors > 0
    }
}

/// Information about one file or directory.
#[derive(Clone, Debug)]
pub struct Info {
    pub name: String,
    pub is_dir: bool,
    pub extent: Extent,
}

impl Info {
    pub fn file_type(&self) -> FileType {
        if self.is_dir {
            FileType::Dir
        } else {
            FileType::from_name(&self.name)
        }
    }

    pub fn size(&self) -> u32 {
        self.extent.size
    }
}

#[derive(Clone, Copy)]
struct RawEntry {
    name: [u8; NAME_FIELD],
    kind: u8,
    extent: Extent,
}

impl RawEntry {
    fn new(name: &str, kind: u8, extent: Extent) -> Self {
        let mut field = [0u8; NAME_FIELD];
        field[..name.len()].copy_from_slice(name.as_bytes());
        Self { name: field, kind, extent }
    }

    fn decode(b: &[u8]) -> Self {
        let mut name = [0u8; NAME_FIELD];
        name.copy_from_slice(&b[..NAME_FIELD]);
        Self {
            name,
            kind: b[48],
            extent: Extent { start: le32(b, 52), sectors: le32(b, 56), size: le32(b, 60) },
        }
    }

    fn encode(&self, b: &mut [u8]) {
        b[..NAME_FIELD].copy_from_slice(&self.name);
        b[48] = self.kind;
        b[49..52].fill(0);
        put32(b, 52, self.extent.start);
        put32(b, 56, self.extent.sectors);
        put32(b, 60, self.extent.size);
    }

    fn name(&self) -> &str {
        let len = self.name.iter().position(|&c| c == 0).unwrap_or(NAME_FIELD);
        core::str::from_utf8(&self.name[..len]).unwrap_or("?")
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn sectors_for(bytes: usize) -> u32 {
    bytes.div_ceil(SECTOR) as u32
}

#[derive(Clone, Copy)]
struct Superblock {
    total_sectors: u32,
    bitmap_sectors: u32,
    data_lba: u32,
    root: Extent,
    kernel: Extent,
}

impl Superblock {
    fn encode(&self) -> [u8; SECTOR] {
        let mut b = [0u8; SECTOR];
        put32(&mut b, SB_MAGIC, MAGIC);
        put32(&mut b, SB_VERSION, VERSION);
        put32(&mut b, SB_TOTAL_SECTORS, self.total_sectors);
        put32(&mut b, SB_BITMAP_SECTORS, self.bitmap_sectors);
        put32(&mut b, SB_DATA_LBA, self.data_lba);
        put32(&mut b, SB_ROOT_START, self.root.start);
        put32(&mut b, SB_ROOT_SECTORS, self.root.sectors);
        put32(&mut b, SB_KERNEL_START, self.kernel.start);
        put32(&mut b, SB_KERNEL_SECTORS, self.kernel.sectors);
        put32(&mut b, SB_KERNEL_SIZE, self.kernel.size);
        b
    }

    fn decode(b: &[u8]) -> Result<Self> {
        if le32(b, SB_MAGIC) != MAGIC || le32(b, SB_VERSION) != VERSION {
            return Err(Error::NotFormatted);
        }
        let root_sectors = le32(b, SB_ROOT_SECTORS);
        Ok(Self {
            total_sectors: le32(b, SB_TOTAL_SECTORS),
            bitmap_sectors: le32(b, SB_BITMAP_SECTORS),
            data_lba: le32(b, SB_DATA_LBA),
            root: Extent {
                start: le32(b, SB_ROOT_START),
                sectors: root_sectors,
                size: root_sectors * SECTOR as u32,
            },
            kernel: Extent {
                start: le32(b, SB_KERNEL_START),
                sectors: le32(b, SB_KERNEL_SECTORS),
                size: le32(b, SB_KERNEL_SIZE),
            },
        })
    }
}

/// Where a node's metadata lives: in the superblock (root) or in a directory slot.
#[derive(Clone, Copy)]
enum Slot {
    Root,
    Entry { dir: Extent, index: usize },
}

#[derive(Clone)]
struct Node {
    slot: Slot,
    entry: RawEntry,
}

impl Node {
    fn is_dir(&self) -> bool {
        self.entry.kind == KIND_DIR
    }
}

/// A mounted file system.
pub struct Fs<D: BlockDevice> {
    dev: D,
    sb: Superblock,
    bitmap: Vec<u8>,
    dirty: Vec<bool>,
}

impl<D: BlockDevice> Fs<D> {
    /// Creates an empty file system on `dev`, leaving the boot area (LBA 0-16) untouched.
    pub fn format(mut dev: D) -> Result<Self> {
        let total = dev.sector_count().min(u32::MAX as u64) as u32;
        let bitmap_sectors = sectors_for((total as usize).div_ceil(8));
        let data_lba = BITMAP_LBA + bitmap_sectors;
        if total < data_lba + 8 {
            return Err(Error::NoSpace);
        }
        let mut bitmap = vec![0u8; bitmap_sectors as usize * SECTOR];
        // Sectors past the end of the disk count as used, so they are never allocated.
        for s in total..bitmap_sectors * SECTOR as u32 * 8 {
            bitmap[s as usize / 8] |= 1 << (s % 8);
        }
        let empty = Superblock {
            total_sectors: total,
            bitmap_sectors,
            data_lba,
            root: Extent::EMPTY,
            kernel: Extent::EMPTY,
        };
        dev.write(SUPERBLOCK_LBA as u64, &[0u8; SECTOR])?;
        let mut fs = Fs { dev, sb: empty, bitmap, dirty: vec![true; bitmap_sectors as usize] };
        fs.mark(0, data_lba, true);
        let root = fs.alloc(1)?;
        fs.dev.write(root as u64, &[0u8; SECTOR])?;
        fs.sb.root = Extent { start: root, sectors: 1, size: SECTOR as u32 };
        fs.flush()?;
        Ok(fs)
    }

    pub fn mount(mut dev: D) -> Result<Self> {
        let mut block = [0u8; SECTOR];
        dev.read(SUPERBLOCK_LBA as u64, &mut block)?;
        let sb = Superblock::decode(&block)?;
        if sb.total_sectors as u64 > dev.sector_count() || sb.data_lba != BITMAP_LBA + sb.bitmap_sectors {
            return Err(Error::NotFormatted);
        }
        let mut bitmap = vec![0u8; sb.bitmap_sectors as usize * SECTOR];
        dev.read(BITMAP_LBA as u64, &mut bitmap)?;
        let dirty = vec![false; sb.bitmap_sectors as usize];
        Ok(Fs { dev, sb, bitmap, dirty })
    }

    pub fn device(&mut self) -> &mut D {
        &mut self.dev
    }

    pub fn into_device(self) -> D {
        self.dev
    }

    /// (total sectors, used sectors).
    pub fn usage(&self) -> (u32, u32) {
        let used = (0..self.sb.total_sectors).filter(|&s| self.is_used(s)).count() as u32;
        (self.sb.total_sectors, used)
    }

    /// Where stage2 will load the kernel from (empty if `/system/kernel.sys` is missing).
    pub fn kernel_extent(&self) -> Extent {
        self.sb.kernel
    }

    // ---- bitmap ----

    fn is_used(&self, sector: u32) -> bool {
        self.bitmap[sector as usize / 8] & (1 << (sector % 8)) != 0
    }

    fn mark(&mut self, start: u32, count: u32, used: bool) {
        for s in start..start + count {
            let byte = s as usize / 8;
            if used {
                self.bitmap[byte] |= 1 << (s % 8);
            } else {
                self.bitmap[byte] &= !(1 << (s % 8));
            }
            self.dirty[byte / SECTOR] = true;
        }
    }

    /// First-fit allocation of `count` contiguous sectors. Returns the start (0 if count is 0).
    fn alloc(&mut self, count: u32) -> Result<u32> {
        if count == 0 {
            return Ok(0);
        }
        let mut run_start = self.sb.data_lba;
        let mut run_len = 0;
        for s in self.sb.data_lba..self.sb.total_sectors {
            if self.is_used(s) {
                run_len = 0;
                run_start = s + 1;
            } else {
                run_len += 1;
                if run_len == count {
                    self.mark(run_start, count, true);
                    return Ok(run_start);
                }
            }
        }
        Err(Error::NoSpace)
    }

    fn free(&mut self, extent: Extent) {
        if extent.sectors > 0 && !extent.is_boot_area() {
            self.mark(extent.start, extent.sectors, false);
        }
    }

    fn flush(&mut self) -> Result<()> {
        for i in 0..self.dirty.len() {
            if self.dirty[i] {
                let bytes = &self.bitmap[i * SECTOR..(i + 1) * SECTOR];
                self.dev.write((BITMAP_LBA as usize + i) as u64, bytes)?;
                self.dirty[i] = false;
            }
        }
        let block = self.sb.encode();
        self.dev.write(SUPERBLOCK_LBA as u64, &block)
    }

    // ---- low-level node access ----

    fn read_extent(&mut self, extent: Extent) -> Result<Vec<u8>> {
        let mut data = vec![0u8; extent.sectors as usize * SECTOR];
        if extent.sectors > 0 {
            self.dev.read(extent.start as u64, &mut data)?;
        }
        data.truncate(extent.size as usize);
        Ok(data)
    }

    fn write_extent(&mut self, start: u32, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let whole = data.len() / SECTOR * SECTOR;
        if whole > 0 {
            self.dev.write(start as u64, &data[..whole])?;
        }
        if whole < data.len() {
            let mut last = [0u8; SECTOR];
            last[..data.len() - whole].copy_from_slice(&data[whole..]);
            self.dev.write(start as u64 + (whole / SECTOR) as u64, &last)?;
        }
        Ok(())
    }

    fn root_node(&self) -> Node {
        Node { slot: Slot::Root, entry: RawEntry::new("", KIND_DIR, self.sb.root) }
    }

    fn dir_entries(&mut self, dir: Extent) -> Result<Vec<RawEntry>> {
        let data = self.read_extent(dir)?;
        Ok(data.chunks_exact(ENTRY_SIZE).map(RawEntry::decode).collect())
    }

    fn find(&mut self, path: &str) -> Result<Node> {
        let mut node = self.root_node();
        for name in components(path) {
            if !node.is_dir() {
                return Err(Error::NotADirectory);
            }
            let dir = node.entry.extent;
            let entries = self.dir_entries(dir)?;
            let index = entries
                .iter()
                .position(|e| e.kind != KIND_FREE && e.name() == name)
                .ok_or(Error::NotFound)?;
            node = Node { slot: Slot::Entry { dir, index }, entry: entries[index] };
        }
        Ok(node)
    }

    /// Rewrites a node's metadata in place (or frees the slot when `entry` is `None`).
    fn store(&mut self, slot: Slot, entry: Option<RawEntry>) -> Result<()> {
        match slot {
            Slot::Root => {
                let entry = entry.ok_or(Error::Root)?;
                self.sb.root = entry.extent;
                Ok(())
            }
            Slot::Entry { dir, index } => {
                let offset = index * ENTRY_SIZE;
                let lba = dir.start as u64 + (offset / SECTOR) as u64;
                let mut block = [0u8; SECTOR];
                self.dev.read(lba, &mut block)?;
                let at = offset % SECTOR;
                match entry {
                    Some(e) => e.encode(&mut block[at..at + ENTRY_SIZE]),
                    None => block[at..at + ENTRY_SIZE].fill(0),
                }
                self.dev.write(lba, &block)
            }
        }
    }

    /// Adds `entry` to the directory `dir`, growing the directory if it is full.
    fn insert(&mut self, dir: &Node, entry: RawEntry) -> Result<()> {
        let extent = dir.entry.extent;
        let entries = self.dir_entries(extent)?;
        if let Some(index) = entries.iter().position(|e| e.kind == KIND_FREE) {
            return self.store(Slot::Entry { dir: extent, index }, Some(entry));
        }
        // Full: move the directory to an extent twice the size.
        let mut data = self.read_extent(extent)?;
        let sectors = (extent.sectors * 2).max(1);
        let start = self.alloc(sectors)?;
        let index = data.len() / ENTRY_SIZE;
        data.resize(sectors as usize * SECTOR, 0);
        entry.encode(&mut data[index * ENTRY_SIZE..(index + 1) * ENTRY_SIZE]);
        self.write_extent(start, &data)?;
        let mut grown = dir.entry;
        grown.extent = Extent { start, sectors, size: sectors * SECTOR as u32 };
        self.store(dir.slot, Some(grown))?;
        self.free(extent);
        Ok(())
    }

    /// Looks up the parent directory of `path` and validates the final name.
    fn parent_of<'p>(&mut self, path: &'p str) -> Result<(Node, &'p str)> {
        let (parent, name) = split_parent(path).ok_or(Error::Root)?;
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        let node = self.find(parent)?;
        if !node.is_dir() {
            return Err(Error::NotADirectory);
        }
        Ok((node, name))
    }

    /// Points the superblock at `/system/kernel.sys`, or clears it if that file is gone.
    fn update_kernel_pointer(&mut self) -> Result<()> {
        self.sb.kernel = match self.find(KERNEL_PATH) {
            Ok(node) if !node.is_dir() && !node.entry.extent.is_boot_area() => node.entry.extent,
            _ => Extent::EMPTY,
        };
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.update_kernel_pointer()?;
        self.flush()
    }

    // ---- public operations; paths are absolute (see `resolve`) ----

    pub fn stat(&mut self, path: &str) -> Result<Info> {
        let node = self.find(path)?;
        let name = match node.slot {
            Slot::Root => String::from("/"),
            Slot::Entry { .. } => String::from(node.entry.name()),
        };
        Ok(Info { name, is_dir: node.is_dir(), extent: node.entry.extent })
    }

    pub fn read_dir(&mut self, path: &str) -> Result<Vec<Info>> {
        let node = self.find(path)?;
        if !node.is_dir() {
            return Err(Error::NotADirectory);
        }
        Ok(self
            .dir_entries(node.entry.extent)?
            .iter()
            .filter(|e| e.kind != KIND_FREE)
            .map(|e| Info { name: String::from(e.name()), is_dir: e.kind == KIND_DIR, extent: e.extent })
            .collect())
    }

    pub fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        let node = self.find(path)?;
        if node.is_dir() {
            return Err(Error::IsADirectory);
        }
        self.read_extent(node.entry.extent)
    }

    /// Creates or replaces a file. `/system/boot.boot` (or any file already in the boot area)
    /// is written in place at LBA 0.
    pub fn write(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let (parent, name) = self.parent_of(path)?;
        let existing = match self.find(path) {
            Ok(node) if node.is_dir() => return Err(Error::IsADirectory),
            Ok(node) => Some(node),
            Err(Error::NotFound) => None,
            Err(e) => return Err(e),
        };
        let size = u32::try_from(data.len()).map_err(|_| Error::NoSpace)?;
        let boot_area = match &existing {
            Some(node) => node.entry.extent.is_boot_area(),
            None => path == BOOT_PATH,
        };
        let extent = if boot_area {
            if data.len() > BOOT_SECTORS as usize * SECTOR {
                return Err(Error::BootTooLarge);
            }
            let mut area = vec![0u8; BOOT_SECTORS as usize * SECTOR];
            area[..data.len()].copy_from_slice(data);
            self.dev.write(0, &area)?;
            Extent { start: 0, sectors: BOOT_SECTORS, size }
        } else {
            let sectors = sectors_for(data.len());
            let start = self.alloc(sectors)?;
            if let Err(e) = self.write_extent(start, data) {
                self.free(Extent { start, sectors, size });
                return Err(e);
            }
            Extent { start, sectors, size }
        };
        let entry = RawEntry::new(name, KIND_FILE, extent);
        match existing {
            Some(node) => {
                self.store(node.slot, Some(entry))?;
                if !boot_area {
                    self.free(node.entry.extent);
                }
            }
            None => self.insert(&parent, entry)?,
        }
        self.finish()
    }

    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        let (parent, name) = self.parent_of(path)?;
        match self.find(path) {
            Ok(_) => return Err(Error::AlreadyExists),
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        let start = self.alloc(1)?;
        self.dev.write(start as u64, &[0u8; SECTOR])?;
        let extent = Extent { start, sectors: 1, size: SECTOR as u32 };
        self.insert(&parent, RawEntry::new(name, KIND_DIR, extent))?;
        self.finish()
    }

    /// Removes a file, or a directory (which must be empty unless `recursive`). Removing the
    /// boot area's file zeroes LBA 0-16, which makes the disk unbootable.
    pub fn remove(&mut self, path: &str, recursive: bool) -> Result<()> {
        self.remove_inner(path, recursive)?;
        self.finish()
    }

    fn remove_inner(&mut self, path: &str, recursive: bool) -> Result<()> {
        let node = self.find(path)?;
        if let Slot::Root = node.slot {
            return Err(Error::Root);
        }
        if node.is_dir() {
            let children = self.read_dir(path)?;
            if !children.is_empty() && !recursive {
                return Err(Error::DirectoryNotEmpty);
            }
            for child in children {
                let child_path = resolve(path, &child.name);
                self.remove_inner(&child_path, true)?;
            }
        }
        // Children removal does not move this directory's entry, but look it up again anyway.
        let node = self.find(path)?;
        let extent = node.entry.extent;
        if extent.is_boot_area() {
            self.dev.write(0, &vec![0u8; BOOT_SECTORS as usize * SECTOR])?;
        }
        self.store(node.slot, None)?;
        self.free(extent);
        Ok(())
    }

    /// Moves or renames. If `to` is an existing directory, `from` is moved into it. An existing
    /// file at the destination is replaced.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let source = self.find(from)?;
        if let Slot::Root = source.slot {
            return Err(Error::Root);
        }
        let mut dest = String::from(to);
        if let Ok(target) = self.find(to) {
            if target.is_dir() {
                dest = resolve(to, source.entry.name());
            }
        }
        if dest == from {
            return Ok(());
        }
        if source.is_dir() && dest.starts_with(from) && dest.as_bytes().get(from.len()) == Some(&b'/') {
            return Err(Error::MoveIntoSelf);
        }
        let (parent_path, name) = split_parent(&dest).ok_or(Error::Root)?;
        self.parent_of(&dest)?; // validates the name and that the parent is a directory
        match self.find(&dest) {
            Ok(existing) if existing.is_dir() => return Err(Error::AlreadyExists),
            Ok(_) => self.remove_inner(&dest, false)?,
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        let parent = self.find(parent_path)?;
        self.insert(&parent, RawEntry::new(name, source.entry.kind, source.entry.extent))?;
        // Inserting may have moved the source's directory; find the old entry by name again.
        let old = self.find(from)?;
        self.store(old.slot, None)?;
        self.finish()
    }

    pub fn copy(&mut self, from: &str, to: &str) -> Result<()> {
        let data = self.read(from)?;
        let mut dest = String::from(to);
        if let Ok(target) = self.find(to) {
            if target.is_dir() {
                let name = split_parent(from).map(|(_, n)| n).ok_or(Error::Root)?;
                dest = resolve(to, name);
            }
        }
        self.write(&dest, &data)
    }
}

#[cfg(test)]
mod tests;
