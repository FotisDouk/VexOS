//! Raw access to the machine: memory, I/O ports and disk sectors. Nothing here is restricted.

use super::{confirm, hexdump, parse_num, Args, CmdError, CmdResult, Shell};
use crate::fs;
use crate::port::{inb, inl, inw, outb, outl, outw};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use kfs::SECTOR;

/// Stage2 identity-maps the first 4 GiB; anything above would page-fault.
const MAPPED_LIMIT: u64 = 1 << 32;
const MAX_PEEK: u64 = 4096;
const MAX_SECTOR_DUMP: u64 = 16;
/// `sector read -o` saves at most this many sectors (4 MiB).
const MAX_SECTOR_SAVE: u64 = 8192;

fn num(s: &str) -> Result<u64, CmdError> {
    parse_num(s).ok_or_else(|| CmdError::Message(alloc::format!("not a number: {s}")))
}

fn check_mapped(addr: u64, len: u64) -> Result<(), CmdError> {
    match addr.checked_add(len) {
        Some(end) if end <= MAPPED_LIMIT => Ok(()),
        _ => Err("address not mapped (only the first 4 GiB are)".into()),
    }
}

/// The value size picked with `-b` (1), `-w` (2), `-d` (4) or `-q` (8); `-b` is the default.
fn unit(a: &Args, allowed: &str) -> Result<usize, CmdError> {
    if a.count_of(allowed) > 1 {
        return Err("choose only one size option".into());
    }
    Ok(match allowed.chars().find(|&c| a.has(c)) {
        Some('w') => 2,
        Some('d') => 4,
        Some('q') => 8,
        _ => 1,
    })
}

/// A hex dump that shows little-endian values of `unit` bytes (1 = the classic byte dump).
fn dump(out: &mut dyn Write, base: u64, bytes: &[u8], unit: usize) -> fmt::Result {
    if unit == 1 {
        return hexdump(out, base, bytes);
    }
    for (i, line) in bytes.chunks(16).enumerate() {
        write!(out, "{:08x} ", base + i as u64 * 16)?;
        for value in line.chunks(unit) {
            let v = value.iter().rev().fold(0u64, |acc, &b| acc << 8 | b as u64);
            write!(out, " {v:0width$x}", width = unit * 2)?;
        }
        out.write_char('\n')?;
    }
    Ok(())
}

pub(super) fn cmd_peek(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let unit = unit(a, "wdq")?;
    let (addr, len) = match a.pos[..] {
        [addr] => (num(addr)?, 64),
        [addr, len] => (num(addr)?, num(len)?),
        _ => return Err(CmdError::Usage),
    };
    let len = len.min(MAX_PEEK).next_multiple_of(unit as u64);
    check_mapped(addr, len)?;
    let mut bytes = vec![0u8; len as usize];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_volatile((addr as usize + i) as *const u8) };
    }
    dump(out, addr, &bytes, unit)?;
    Ok(())
}

pub(super) fn cmd_poke(_: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    let unit = unit(a, "wdq")?;
    let [addr, ref values @ ..] = a.pos[..] else { return Err(CmdError::Usage) };
    if values.is_empty() {
        return Err(CmdError::Usage);
    }
    let addr = num(addr)?;
    let mut bytes = Vec::new();
    for v in values {
        let n = num(v)?;
        if unit < 8 && n >> (unit * 8) != 0 {
            return Err(CmdError::Message(alloc::format!("{v} does not fit in {} bits", unit * 8)));
        }
        bytes.extend_from_slice(&n.to_le_bytes()[..unit]);
    }
    check_mapped(addr, bytes.len() as u64)?;
    for (i, b) in bytes.iter().enumerate() {
        unsafe { core::ptr::write_volatile((addr as usize + i) as *mut u8, *b) };
    }
    Ok(())
}

/// Port access width: `-b`/`-w`/`-d`, or the older trailing `b`/`w`/`d` argument.
fn width(a: &Args, trailing: Option<&str>) -> Result<u8, CmdError> {
    let from_option = unit(a, "bwd")? as u8;
    match trailing {
        None => Ok(from_option),
        Some(_) if a.count_of("bwd") > 0 => Err(CmdError::Usage),
        Some("b") => Ok(1),
        Some("w") => Ok(2),
        Some("d") => Ok(4),
        Some(_) => Err(CmdError::Usage),
    }
}

fn port(s: &str) -> Result<u16, CmdError> {
    u16::try_from(num(s)?).map_err(|_| "ports are 0-0xffff".into())
}

pub(super) fn cmd_in(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let (p, w) = match a.pos[..] {
        [p] => (port(p)?, width(a, None)?),
        [p, w] => (port(p)?, width(a, Some(w))?),
        _ => return Err(CmdError::Usage),
    };
    let value = unsafe {
        match w {
            1 => inb(p) as u32,
            2 => inw(p) as u32,
            _ => inl(p),
        }
    };
    writeln!(out, "{value:#0width$x} ({value})", width = 2 + 2 * w as usize)?;
    Ok(())
}

pub(super) fn cmd_out(_: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    let (p, value, w) = match a.pos[..] {
        [p, v] => (port(p)?, num(v)?, width(a, None)?),
        [p, v, w] => (port(p)?, num(v)?, width(a, Some(w))?),
        _ => return Err(CmdError::Usage),
    };
    if value >> (8 * w as u32) != 0 {
        return Err("value too large for that width".into());
    }
    unsafe {
        match w {
            1 => outb(p, value as u8),
            2 => outw(p, value as u16),
            _ => outl(p, value as u32),
        }
    }
    Ok(())
}

pub(super) fn cmd_sector(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    match a.pos[..] {
        ["read", lba] | ["read", lba, _] => {
            let lba = num(lba)?;
            let count = match (a.pos.get(2), a.num('n')?) {
                (Some(_), Some(_)) => return Err(CmdError::Usage),
                (Some(c), None) => num(c)?,
                (None, Some(c)) => c,
                (None, None) => 1,
            };
            let save_to = a.value('o').map(|f| shell.path(f));
            let limit = if save_to.is_some() { MAX_SECTOR_SAVE } else { MAX_SECTOR_DUMP };
            let count = count.clamp(1, limit);
            let mut buf = vec![0u8; count as usize * SECTOR];
            fs::raw(false, |dev| dev.read(lba, &mut buf))?;
            match save_to {
                Some(path) => {
                    fs::with(|fs| fs.write(&path, &buf)).map_err(|e| CmdError::Message(alloc::format!("{path}: {e}")))?;
                    writeln!(out, "Saved {count} sector(s) to {path}.")?;
                }
                None => hexdump(out, lba * SECTOR as u64, &buf)?,
            }
            Ok(())
        }
        ["write", lba, file] => {
            let lba = num(lba)?;
            let path = shell.path(file);
            let mut data = fs::with(|fs| fs.read(&path))?;
            if data.is_empty() {
                return Err("the file is empty".into());
            }
            data.resize(data.len().next_multiple_of(SECTOR), 0);
            let sectors = data.len() / SECTOR;
            let question = alloc::format!("Write {sectors} sector(s) directly to the disk at LBA {lba}?");
            if !a.has('f') && !confirm(out, &question) {
                return Ok(());
            }
            fs::raw(true, |dev| dev.write(lba, &data))?;
            writeln!(out, "Wrote {sectors} sector(s).")?;
            Ok(())
        }
        _ => Err(CmdError::Usage),
    }
}

pub(super) fn cmd_disks(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    for disk in fs::disk_list() {
        writeln!(out, "  {disk}")?;
    }
    writeln!(out, "Boot disk driver: {}", fs::driver().unwrap_or_else(|| "none".into()))?;
    Ok(())
}
