//! File commands. Paths are relative to the shell's current directory.

use super::{confirm, hexdump, Args, CmdError, CmdResult, Pager, Shell};
use crate::fs;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use kfs::{FileType, Info};

/// `rd` shows at most this much of a binary file unless `-a` or `-c` is given.
const HEX_PREVIEW: usize = 512;

/// Adds the path to a file system error.
fn at<T>(path: &str, result: fs::Result<T>) -> Result<T, CmdError> {
    result.map_err(|e| CmdError::Message(alloc::format!("{path}: {e}")))
}

/// The last part of a path: `/a/b.txt` -> `b.txt`.
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_dir(path: &str) -> bool {
    fs::with(|fs| fs.stat(path)).is_ok_and(|info| info.is_dir)
}

pub(super) fn cmd_ls(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let paths: Vec<String> = if a.pos.is_empty() {
        alloc::vec![shell.cwd.clone()]
    } else {
        a.pos.iter().map(|p| shell.path(p)).collect()
    };
    let mut out = Pager::new(out, a.has('p'));
    let headers = paths.len() > 1 || a.has('R');
    for (n, path) in paths.iter().enumerate() {
        let mut info = at(path, fs::with(|fs| fs.stat(path)))?;
        if info.is_dir && !a.has('d') {
            list_dir(&mut out, a, path, headers, n > 0)?;
        } else {
            if a.has('d') || paths.len() > 1 {
                info.name = path.clone();
            }
            show_entries(&mut out, a, &[info])?;
        }
    }
    Ok(())
}

fn list_dir(out: &mut dyn Write, a: &Args, path: &str, header: bool, gap: bool) -> CmdResult {
    let mut entries = at(path, fs::with(|fs| fs.read_dir(path)))?;
    if a.has('S') {
        entries.sort_by(|x, y| y.extent.size.cmp(&x.extent.size).then_with(|| x.name.cmp(&y.name)));
    } else {
        entries.sort_by(|x, y| y.is_dir.cmp(&x.is_dir).then_with(|| x.name.cmp(&y.name)));
    }
    if a.has('r') {
        entries.reverse();
    }
    if gap {
        writeln!(out)?;
    }
    if header {
        writeln!(out, "{path}:")?;
    }
    show_entries(out, a, &entries)?;
    if a.has('R') {
        for e in entries.iter().filter(|e| e.is_dir) {
            list_dir(out, a, &kfs::resolve(path, &e.name), true, true)?;
        }
    }
    Ok(())
}

fn show_entries(out: &mut dyn Write, a: &Args, entries: &[Info]) -> CmdResult {
    if entries.is_empty() {
        if !a.has('1') {
            writeln!(out, "  (empty)")?;
        }
        return Ok(());
    }
    if a.has('l') && !a.has('1') {
        writeln!(out, "  {:<7}{:>9}  {:>7}  {:>8}  name", "type", "bytes", "sectors", "LBA")?;
    }
    for e in entries {
        let slash = if e.is_dir { "/" } else { "" };
        if a.has('1') {
            writeln!(out, "{}{slash}", e.name)?;
            continue;
        }
        let ty = e.file_type().label();
        let size = if e.is_dir { String::new() } else { alloc::format!("{}", e.extent.size) };
        if a.has('l') {
            let lba = if e.extent.sectors > 0 { alloc::format!("{}", e.extent.start) } else { String::from("-") };
            writeln!(out, "  {ty:<7}{size:>9}  {:>7}  {lba:>8}  {}{slash}", e.extent.sectors, e.name)?;
        } else {
            writeln!(out, "  {ty:<7}{size:>9}  {}{slash}", e.name)?;
        }
    }
    Ok(())
}

pub(super) fn cmd_cd(shell: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    let path = match a.pos[..] {
        [] => String::from("/"),
        ["-"] => shell.prev_cwd.clone(),
        [p] => shell.path(p),
        _ => return Err(CmdError::Usage),
    };
    if !at(&path, fs::with(|fs| fs.stat(&path)))?.is_dir {
        return Err(CmdError::Message(alloc::format!("{path}: not a directory")));
    }
    shell.prev_cwd = core::mem::replace(&mut shell.cwd, path);
    Ok(())
}

pub(super) fn cmd_pwd(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    writeln!(out, "{}", shell.cwd)?;
    Ok(())
}

pub(super) fn cmd_mkdir(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    for arg in &a.pos {
        let path = shell.path(arg);
        if !a.has('p') {
            at(&path, fs::with(|fs| fs.mkdir(&path)))?;
            if a.has('v') {
                writeln!(out, "created {path}")?;
            }
            continue;
        }
        let mut current = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            current.push('/');
            current.push_str(part);
            match fs::with(|fs| fs.stat(&current)) {
                Ok(info) if info.is_dir => {}
                Ok(_) => return Err(CmdError::Message(alloc::format!("{current}: not a directory"))),
                Err(_) => {
                    at(&current, fs::with(|fs| fs.mkdir(&current)))?;
                    if a.has('v') {
                        writeln!(out, "created {current}")?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Writes file contents as text: tabs become spaces, other control bytes become dots.
pub fn write_text(out: &mut dyn Write, data: &[u8]) -> core::fmt::Result {
    for &b in data {
        match b {
            b'\n' | 0x20..=0x7E => out.write_char(b as char)?,
            b'\t' => out.write_str("    ")?,
            b'\r' => {}
            _ => out.write_char('.')?,
        }
    }
    if data.last().is_some_and(|&b| b != b'\n') {
        out.write_char('\n')?;
    }
    Ok(())
}

/// Like `write_text`, with a line number in front of every line.
fn write_numbered(out: &mut dyn Write, data: &[u8]) -> core::fmt::Result {
    let text = data.strip_suffix(b"\n").unwrap_or(data);
    if data.is_empty() {
        return Ok(());
    }
    for (i, line) in text.split(|&b| b == b'\n').enumerate() {
        write!(out, "{:5}  ", i + 1)?;
        write_text(out, line)?;
        if line.is_empty() {
            out.write_char('\n')?;
        }
    }
    Ok(())
}

pub(super) fn cmd_rd(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let [file] = a.pos[..] else { return Err(CmdError::Usage) };
    if a.has('x') && a.has('t') {
        return Err("-x and -t cannot be used together".into());
    }
    let path = shell.path(file);
    let data = at(&path, fs::with(|fs| fs.read(&path)))?;
    let ty = FileType::from_name(&path);
    let start = (a.num('o')?.unwrap_or(0) as usize).min(data.len());
    let end = match a.num('c')? {
        Some(count) => start.saturating_add(count as usize).min(data.len()),
        None => data.len(),
    };
    let part = &data[start..end];
    let mut out = Pager::new(out, a.has('p'));
    let as_text = if a.has('x') { false } else { a.has('t') || ty.is_text() };
    if as_text {
        if a.has('n') {
            write_numbered(&mut out, part)?;
        } else {
            write_text(&mut out, part)?;
        }
        return Ok(());
    }
    let shown = if a.has('a') || a.has('c') { part.len() } else { part.len().min(HEX_PREVIEW) };
    hexdump(&mut out, start as u64, &part[..shown])?;
    if shown < part.len() {
        writeln!(out, "... {} more bytes ({} file; 'rd -a' shows everything)", part.len() - shown, ty.label())?;
    }
    Ok(())
}

/// True if removing `path` would break booting from this disk.
fn needed_to_boot(path: &str, info: &Info) -> bool {
    let inside = |file: &str| file.len() > path.len() && file.starts_with(path) && file.as_bytes()[path.len()] == b'/';
    info.file_type().is_system() || inside(kfs::BOOT_PATH) || inside(kfs::KERNEL_PATH)
}

pub(super) fn cmd_rm(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    let recursive = a.has('r');
    let force = a.has('f');
    for arg in &a.pos {
        let path = shell.path(arg);
        let info = match fs::with(|fs| fs.stat(&path)) {
            Ok(info) => info,
            Err(_) if force => continue,
            Err(e) => return Err(CmdError::Message(alloc::format!("{path}: {e}"))),
        };
        if info.is_dir && !recursive && !at(&path, fs::with(|fs| fs.read_dir(&path)))?.is_empty() {
            return Err(CmdError::Message(alloc::format!("{path} is a directory that is not empty (use rm -r)")));
        }
        if needed_to_boot(&path, &info) && !force {
            let question = alloc::format!("{path} is needed to boot this disk. Delete it anyway?");
            if !confirm(out, &question) {
                continue;
            }
        }
        at(&path, fs::with(|fs| fs.remove(&path, recursive)))?;
        if a.has('v') {
            writeln!(out, "removed {path}")?;
        }
    }
    Ok(())
}

pub(super) fn cmd_mv(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    transfer(shell, a, out, false)
}

pub(super) fn cmd_cp(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    transfer(shell, a, out, true)
}

/// `mv` and `cp`: one source to a new name, or any number of sources into a directory.
fn transfer(shell: &mut Shell, a: &Args, out: &mut dyn Write, copy: bool) -> CmdResult {
    let [sources @ .., to] = &a.pos[..] else { return Err(CmdError::Usage) };
    if sources.is_empty() {
        return Err(CmdError::Usage);
    }
    let to = shell.path(to);
    let into_dir = is_dir(&to);
    if sources.len() > 1 && !into_dir {
        return Err(CmdError::Message(alloc::format!("{to} is not a directory")));
    }
    for source in sources {
        let from = shell.path(source);
        let info = at(&from, fs::with(|fs| fs.stat(&from)))?;
        let dest = if into_dir { kfs::resolve(&to, base_name(&from)) } else { to.clone() };
        if dest == from {
            continue;
        }
        if let Ok(existing) = fs::with(|fs| fs.stat(&dest)) {
            if !existing.is_dir {
                if a.has('n') {
                    continue;
                }
                if a.has('i') && !confirm(out, &alloc::format!("Replace {dest}?")) {
                    continue;
                }
            }
        }
        if !copy {
            at(&from, fs::with(|fs| fs.rename(&from, &to)))?;
        } else if info.is_dir {
            if !a.has('r') {
                return Err(CmdError::Message(alloc::format!("{from} is a directory (use cp -r)")));
            }
            if dest.starts_with(&from) && dest.as_bytes().get(from.len()) == Some(&b'/') {
                return Err(CmdError::Message(alloc::format!("cannot copy {from} into itself")));
            }
            copy_tree(&from, &dest, a, out)?;
        } else {
            at(&from, fs::with(|fs| fs.copy(&from, &to)))?;
        }
        if a.has('v') {
            writeln!(out, "{from} -> {dest}")?;
        }
    }
    Ok(())
}

/// Copies directory `from` to `to`, creating `to` if needed and merging into it otherwise.
fn copy_tree(from: &str, to: &str, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !is_dir(to) {
        at(to, fs::with(|fs| fs.mkdir(to)))?;
    }
    for entry in at(from, fs::with(|fs| fs.read_dir(from)))? {
        let src = kfs::resolve(from, &entry.name);
        let dst = kfs::resolve(to, &entry.name);
        if entry.is_dir {
            copy_tree(&src, &dst, a, out)?;
            continue;
        }
        if fs::with(|fs| fs.stat(&dst)).is_ok() {
            if a.has('n') {
                continue;
            }
            if a.has('i') && !confirm(out, &alloc::format!("Replace {dst}?")) {
                continue;
            }
        }
        let data = at(&src, fs::with(|fs| fs.read(&src)))?;
        at(&dst, fs::with(|fs| fs.write(&dst, &data)))?;
        if a.has('v') {
            writeln!(out, "{src} -> {dst}")?;
        }
    }
    Ok(())
}

pub(super) fn cmd_info(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    for (n, arg) in a.pos.iter().enumerate() {
        if n > 0 {
            writeln!(out)?;
        }
        let path = shell.path(arg);
        let info = at(&path, fs::with(|fs| fs.stat(&path)))?;
        let e = info.extent;
        writeln!(out, "Path:     {path}")?;
        writeln!(out, "Type:     {}", info.file_type().label())?;
        if info.is_dir {
            writeln!(out, "Entries:  {}", fs::with(|fs| fs.read_dir(&path))?.len())?;
        } else {
            writeln!(out, "Size:     {} bytes", e.size)?;
        }
        if e.sectors > 0 {
            writeln!(out, "Sectors:  {} - {} ({} sectors)", e.start, e.start + e.sectors - 1, e.sectors)?;
        }
        if e.is_boot_area() {
            writeln!(out, "This is the boot area: the BIOS starts the computer from sector 0.")?;
        }
        if !info.is_dir && fs::with(|fs| Ok(fs.kernel_extent()))? == e && e.sectors > 0 {
            writeln!(out, "This is the kernel the bootloader loads.")?;
        }
    }
    Ok(())
}

pub(super) fn cmd_df(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    let (total, used) = fs::with(|fs| Ok(fs.usage()))?;
    let kib = |sectors: u32| sectors / 2;
    writeln!(out, "Disk:  {}", fs::driver().unwrap_or_default())?;
    writeln!(
        out,
        "Size:  {} KiB, {} KiB used, {} KiB free",
        kib(total),
        kib(used),
        kib(total - used)
    )?;
    Ok(())
}

pub(super) fn cmd_format(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    if !a.has('f') && !confirm(out, "Erase every file except the bootloader and kernel?") {
        return Ok(());
    }
    fs::format()?;
    shell.cwd = String::from("/");
    shell.prev_cwd = String::from("/");
    writeln!(out, "Formatted.")?;
    Ok(())
}

/// Opens any file: text types in the text editor, everything else in the hex editor.
pub(super) fn cmd_edit(shell: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    let [file] = a.pos[..] else { return Err(CmdError::Usage) };
    if a.has('x') && a.has('t') {
        return Err("-x and -t cannot be used together".into());
    }
    let path = shell.path(file);
    if is_dir(&path) {
        return Err(CmdError::Message(alloc::format!("{path} is a directory")));
    }
    let hex = if a.has('x') { true } else { !a.has('t') && !FileType::from_name(&path).is_text() };
    if hex {
        let offset = a.num('o')?.unwrap_or(0) as usize;
        crate::hexedit::HexEditor::open(&path)?.go_to(offset).run();
    } else {
        let line = a.num('l')?.unwrap_or(1) as usize;
        crate::editor::Editor::open(&path)?.go_to_line(line).run();
    }
    Ok(())
}

pub(super) fn cmd_asm(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let (source, target) = match (&a.pos[..], a.value('o')) {
        ([s], t) => (shell.path(s), t.map(|t| shell.path(t))),
        ([s, t], None) => (shell.path(s), Some(shell.path(t))),
        _ => return Err(CmdError::Usage),
    };
    if FileType::from_name(&source) != FileType::Asm {
        return Err("the source must be an .asm file".into());
    }
    let target = target.unwrap_or_else(|| {
        alloc::format!("{}.vex", source.trim_end_matches(".asm").trim_end_matches(".ASM"))
    });
    if FileType::from_name(&target) != FileType::Exe {
        return Err("the output must be a .vex file".into());
    }
    let data = at(&source, fs::with(|fs| fs.read(&source)))?;
    let (program, sizes) = crate::program::assemble_with_sizes(&source, &data)?;
    let mut out = Pager::new(out, a.has('p'));
    if a.has('l') {
        listing(&mut out, &data, &program, &sizes)?;
    }
    if a.has('n') {
        writeln!(out, "{source}: OK, {} bytes of code and data (nothing written)", program.image.len())?;
        return Ok(());
    }
    let vex = vasm::to_vex(&program);
    at(&target, fs::with(|fs| fs.write(&target, &vex)))?;
    writeln!(out, "{target}: {} bytes of code and data", program.image.len())?;
    Ok(())
}

/// `asm -l`: line number, address and up to 10 bytes for every source line.
fn listing(out: &mut dyn Write, source: &[u8], program: &vasm::Program, sizes: &[usize]) -> CmdResult {
    const SHOWN: usize = 10;
    let text = String::from_utf8_lossy(source);
    let mut offset = 0;
    for (i, line) in text.lines().enumerate() {
        let size = sizes.get(i).copied().unwrap_or(0);
        let bytes = &program.image[offset..offset + size];
        let mut hex = String::new();
        for b in bytes.iter().take(SHOWN) {
            let _ = write!(hex, "{b:02x}");
        }
        if size > SHOWN {
            hex.push('+');
        }
        let addr = if size > 0 {
            alloc::format!("{:08x}", crate::program::PROGRAM_BASE + offset as u64)
        } else {
            String::new()
        };
        write!(out, "{:4} {addr:<8} {hex:<21} ", i + 1)?;
        write_text(out, line.as_bytes())?;
        if line.is_empty() {
            writeln!(out)?;
        }
        offset += size;
    }
    Ok(())
}

pub(super) fn cmd_run(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let Some((file, rest)) = a.pos.split_first() else { return Err(CmdError::Usage) };
    let path = shell.path(file);
    if at(&path, fs::with(|fs| fs.stat(&path)))?.is_dir {
        return Err(CmdError::Message(alloc::format!("{path} is a directory")));
    }
    let started = crate::pit::uptime_ms();
    let result = shell.run_file(&path, rest, out);
    if a.has('t') {
        writeln!(out, "[{} ms]", crate::pit::uptime_ms() - started)?;
    }
    result
}
