//! File commands. Paths are relative to the shell's current directory.

use super::{confirm, hexdump, CmdError, CmdResult, Shell};
use crate::fs;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use kfs::{FileType, Info};

/// `rd` shows at most this much of a binary file unless `-a` is given.
const HEX_PREVIEW: usize = 512;

/// Splits leading `-x` style flags from the other arguments.
fn flags<'a>(args: &[&'a str], allowed: &str) -> Result<(String, Vec<&'a str>), CmdError> {
    let mut set = String::new();
    let mut rest = Vec::new();
    for &arg in args {
        match arg.strip_prefix('-') {
            Some(letters) if !letters.is_empty() && rest.is_empty() => {
                for c in letters.chars() {
                    if !allowed.contains(c) {
                        return Err(CmdError::Message(alloc::format!("unknown option -{c}")));
                    }
                    set.push(c);
                }
            }
            _ => rest.push(arg),
        }
    }
    Ok((set, rest))
}

fn describe(info: &Info) -> (&'static str, String) {
    let ty = info.file_type();
    let size = if info.is_dir { String::new() } else { alloc::format!("{}", info.size()) };
    (ty.label(), size)
}

pub(super) fn cmd_ls(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let path = match args {
        [] => shell.cwd.clone(),
        [p] => shell.path(p),
        _ => return Err(CmdError::Usage),
    };
    let info = fs::with(|fs| fs.stat(&path))?;
    let mut entries = if info.is_dir { fs::with(|fs| fs.read_dir(&path))? } else { alloc::vec![info] };
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    for e in &entries {
        let (ty, size) = describe(e);
        let slash = if e.is_dir { "/" } else { "" };
        writeln!(out, "  {ty:<7}{size:>9}  {}{slash}", e.name)?;
    }
    if entries.is_empty() {
        writeln!(out, "  (empty)")?;
    }
    Ok(())
}

pub(super) fn cmd_cd(shell: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    let path = match args {
        [] => String::from("/"),
        [p] => shell.path(p),
        _ => return Err(CmdError::Usage),
    };
    if !fs::with(|fs| fs.stat(&path))?.is_dir {
        return Err("not a directory".into());
    }
    shell.cwd = path;
    Ok(())
}

pub(super) fn cmd_pwd(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    writeln!(out, "{}", shell.cwd)?;
    Ok(())
}

pub(super) fn cmd_mkdir(shell: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    if args.is_empty() {
        return Err(CmdError::Usage);
    }
    for arg in args {
        let path = shell.path(arg);
        fs::with(|fs| fs.mkdir(&path)).map_err(|e| CmdError::Message(alloc::format!("{path}: {e}")))?;
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

pub(super) fn cmd_rd(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let (opts, rest) = flags(args, "ax")?;
    let [file] = rest[..] else { return Err(CmdError::Usage) };
    let path = shell.path(file);
    let data = fs::with(|fs| fs.read(&path))?;
    let ty = FileType::from_name(&path);
    if ty.is_text() && !opts.contains('x') {
        write_text(out, &data)?;
        return Ok(());
    }
    let shown = if opts.contains('a') { data.len() } else { data.len().min(HEX_PREVIEW) };
    hexdump(out, 0, &data[..shown])?;
    if shown < data.len() {
        writeln!(out, "... {} more bytes ({} file; 'rd -a' shows everything)", data.len() - shown, ty.label())?;
    }
    Ok(())
}

/// True if removing `path` would break booting from this disk.
fn needed_to_boot(path: &str, info: &Info) -> bool {
    let inside = |file: &str| file.len() > path.len() && file.starts_with(path) && file.as_bytes()[path.len()] == b'/';
    info.file_type().is_system() || inside(kfs::BOOT_PATH) || inside(kfs::KERNEL_PATH)
}

pub(super) fn cmd_rm(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let (opts, rest) = flags(args, "rf")?;
    if rest.is_empty() {
        return Err(CmdError::Usage);
    }
    let recursive = opts.contains('r');
    let force = opts.contains('f');
    for arg in rest {
        let path = shell.path(arg);
        let info = fs::with(|fs| fs.stat(&path)).map_err(|e| CmdError::Message(alloc::format!("{path}: {e}")))?;
        if info.is_dir && !recursive && !fs::with(|fs| fs.read_dir(&path))?.is_empty() {
            return Err(CmdError::Message(alloc::format!("{path} is a directory that is not empty (use rm -r)")));
        }
        if needed_to_boot(&path, &info) && !force {
            let question = alloc::format!("{path} is needed to boot this disk. Delete it anyway?");
            if !confirm(out, &question) {
                continue;
            }
        }
        fs::with(|fs| fs.remove(&path, recursive)).map_err(|e| CmdError::Message(alloc::format!("{path}: {e}")))?;
    }
    Ok(())
}

pub(super) fn cmd_mv(shell: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    let [from, to] = args else { return Err(CmdError::Usage) };
    let (from, to) = (shell.path(from), shell.path(to));
    fs::with(|fs| fs.rename(&from, &to))?;
    Ok(())
}

pub(super) fn cmd_cp(shell: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    let [from, to] = args else { return Err(CmdError::Usage) };
    let (from, to) = (shell.path(from), shell.path(to));
    fs::with(|fs| fs.copy(&from, &to))?;
    Ok(())
}

pub(super) fn cmd_info(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let [arg] = args else { return Err(CmdError::Usage) };
    let path = shell.path(arg);
    let info = fs::with(|fs| fs.stat(&path))?;
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
    Ok(())
}

pub(super) fn cmd_df(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
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

pub(super) fn cmd_format(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let (opts, rest) = flags(args, "f")?;
    if !rest.is_empty() {
        return Err(CmdError::Usage);
    }
    if !opts.contains('f') && !confirm(out, "Erase every file except the bootloader and kernel?") {
        return Ok(());
    }
    fs::format()?;
    shell.cwd = String::from("/");
    writeln!(out, "Formatted.")?;
    Ok(())
}

pub(super) fn cmd_edit(shell: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    let [file] = args else { return Err(CmdError::Usage) };
    let path = shell.path(file);
    match fs::with(|fs| fs.stat(&path)) {
        Ok(info) if info.is_dir => return Err("is a directory".into()),
        Ok(info) if !info.file_type().is_text() => {
            return Err(CmdError::Message(alloc::format!(
                "{path} is a {} file; edit only works on text files (.txt, .asm, .v, no extension)",
                info.file_type().label()
            )))
        }
        _ => {}
    }
    crate::editor::Editor::open(&path)?.run();
    Ok(())
}

pub(super) fn cmd_asm(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let (source, target) = match args {
        [s] => (shell.path(s), None),
        [s, t] => (shell.path(s), Some(shell.path(t))),
        _ => return Err(CmdError::Usage),
    };
    if FileType::from_name(&source) != FileType::Asm {
        return Err("the source must be an .asm file".into());
    }
    let target = target.unwrap_or_else(|| alloc::format!("{}.vex", source.trim_end_matches(".asm").trim_end_matches(".ASM")));
    if FileType::from_name(&target) != FileType::Exe {
        return Err("the output must be a .vex file".into());
    }
    let data = fs::with(|fs| fs.read(&source))?;
    let program = crate::program::assemble(&source, &data)?;
    let vex = vasm::to_vex(&program);
    fs::with(|fs| fs.write(&target, &vex))?;
    writeln!(out, "{target}: {} bytes of code and data", program.image.len())?;
    Ok(())
}

pub(super) fn cmd_run(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let Some((file, rest)) = args.split_first() else { return Err(CmdError::Usage) };
    let path = shell.path(file);
    let info = fs::with(|fs| fs.stat(&path))?;
    if info.is_dir {
        return Err("is a directory".into());
    }
    shell.run_file(&path, rest, out)
}
