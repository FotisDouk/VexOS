//! Raw access to the machine: memory, I/O ports and disk sectors. Nothing here is restricted.

use super::{confirm, hexdump, parse_num, CmdError, CmdResult, Shell};
use crate::fs;
use crate::port::{inb, inl, inw, outb, outl, outw};
use alloc::vec;
use core::fmt::Write;
use kfs::SECTOR;

/// Stage2 identity-maps the first 4 GiB; anything above would page-fault.
const MAPPED_LIMIT: u64 = 1 << 32;
const MAX_PEEK: u64 = 4096;
const MAX_SECTOR_DUMP: u64 = 16;

fn num(s: &str) -> Result<u64, CmdError> {
    parse_num(s).ok_or_else(|| CmdError::Message(alloc::format!("not a number: {s}")))
}

fn check_mapped(addr: u64, len: u64) -> Result<(), CmdError> {
    match addr.checked_add(len) {
        Some(end) if end <= MAPPED_LIMIT => Ok(()),
        _ => Err("address not mapped (only the first 4 GiB are)".into()),
    }
}

pub(super) fn cmd_peek(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let (addr, len) = match args {
        [a] => (num(a)?, 64),
        [a, l] => (num(a)?, num(l)?),
        _ => return Err(CmdError::Usage),
    };
    let len = len.min(MAX_PEEK);
    check_mapped(addr, len)?;
    let mut bytes = vec![0u8; len as usize];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_volatile((addr as usize + i) as *const u8) };
    }
    hexdump(out, addr, &bytes)?;
    Ok(())
}

pub(super) fn cmd_poke(_: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    let Some((addr, values)) = args.split_first() else { return Err(CmdError::Usage) };
    if values.is_empty() {
        return Err(CmdError::Usage);
    }
    let addr = num(addr)?;
    let bytes = values
        .iter()
        .map(|v| num(v).and_then(|n| u8::try_from(n).map_err(|_| CmdError::from("byte values must be 0-255"))))
        .collect::<Result<vec::Vec<u8>, _>>()?;
    check_mapped(addr, bytes.len() as u64)?;
    for (i, b) in bytes.iter().enumerate() {
        unsafe { core::ptr::write_volatile((addr as usize + i) as *mut u8, *b) };
    }
    Ok(())
}

fn width(arg: Option<&&str>) -> Result<u8, CmdError> {
    match arg.copied() {
        None | Some("b") => Ok(1),
        Some("w") => Ok(2),
        Some("d") => Ok(4),
        Some(_) => Err(CmdError::Usage),
    }
}

fn port(s: &str) -> Result<u16, CmdError> {
    u16::try_from(num(s)?).map_err(|_| "ports are 0-0xffff".into())
}

pub(super) fn cmd_in(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let (p, w) = match args {
        [p] | [p, _] => (port(p)?, width(args.get(1))?),
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

pub(super) fn cmd_out(_: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    let (p, value, w) = match args {
        [p, v] | [p, v, _] => (port(p)?, num(v)?, width(args.get(2))?),
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

pub(super) fn cmd_sector(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    match args {
        ["read", lba] | ["read", lba, _] => {
            let lba = num(lba)?;
            let count = match args.get(2) {
                Some(c) => num(c)?.clamp(1, MAX_SECTOR_DUMP),
                None => 1,
            };
            let mut buf = vec![0u8; count as usize * SECTOR];
            fs::raw(false, |dev| dev.read(lba, &mut buf))?;
            hexdump(out, lba * SECTOR as u64, &buf)?;
            Ok(())
        }
        ["write", lba, file] | ["write", lba, file, "-f"] => {
            let lba = num(lba)?;
            let path = shell.path(file);
            let mut data = fs::with(|fs| fs.read(&path))?;
            if data.is_empty() {
                return Err("the file is empty".into());
            }
            data.resize(data.len().next_multiple_of(SECTOR), 0);
            let sectors = data.len() / SECTOR;
            let force = args.len() == 4;
            let question = alloc::format!("Write {sectors} sector(s) directly to the disk at LBA {lba}?");
            if !force && !confirm(out, &question) {
                return Ok(());
            }
            fs::raw(true, |dev| dev.write(lba, &data))?;
            writeln!(out, "Wrote {sectors} sector(s).")?;
            Ok(())
        }
        _ => Err(CmdError::Usage),
    }
}

pub(super) fn cmd_disks(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    for disk in fs::disk_list() {
        writeln!(out, "  {disk}")?;
    }
    writeln!(out, "Boot disk driver: {}", fs::driver().unwrap_or_else(|| "none".into()))?;
    Ok(())
}
