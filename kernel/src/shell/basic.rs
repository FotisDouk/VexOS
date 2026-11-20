//! General commands: help, output, timing, CPU and power.

use super::{describe, find_command, Args, Category, CmdError, CmdResult, Pager, Shell, COMMANDS};
use crate::vga::Color;
use crate::{acpi, boot_info, console, cpu, heap, pit};
use alloc::string::String;
use core::fmt::Write;

pub(super) fn cmd_help(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let mut out = Pager::new(out, a.has('p'));
    match a.pos[..] {
        [] if a.has('a') => {
            for category in Category::ALL {
                writeln!(out, "{}", category.title())?;
                for c in COMMANDS.iter().filter(|c| c.category == category) {
                    writeln!(out, "  {:<9}{}", c.name, c.help)?;
                }
            }
        }
        [] => {
            for category in Category::ALL {
                write!(out, "{:<10}", category.title())?;
                let mut col = 10;
                for c in COMMANDS.iter().filter(|c| c.category == category) {
                    if col + c.name.len() + 1 > 79 {
                        write!(out, "\n{:9}", "")?;
                        col = 10;
                    }
                    write!(out, "{} ", c.name)?;
                    col += c.name.len() + 1;
                }
                writeln!(out)?;
            }
            writeln!(out, "'help <command>' or '<command> -h' describes a command and its options;")?;
            writeln!(out, "'help -a' describes them all. Programs and scripts run by name.")?;
            writeln!(out, "The manual is in /docs: rd -p /docs/index.txt")?;
        }
        [name] => describe(find_command(name).ok_or("no such command")?, &mut out)?,
        _ => return Err(CmdError::Usage),
    }
    Ok(())
}

pub(super) fn cmd_clear(_: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    console::clear();
    Ok(())
}

/// `print -e`: turns `\n`, `\t`, `\r`, `\e`, `\\` and `\xHH` into the characters they stand for.
fn unescape(text: &str) -> Result<String, CmdError> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('e') => out.push('\x1b'),
            Some('\\') => out.push('\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                let byte = u8::from_str_radix(&hex, 16)
                    .map_err(|_| CmdError::Message(alloc::format!("bad escape \\x{hex}")))?;
                out.push(byte as char);
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Ok(out)
}

pub(super) fn cmd_print(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let mut text = a.pos.join(" ");
    if a.has('e') {
        text = unescape(&text)?;
    }
    out.write_str(&text)?;
    if !a.has('n') {
        out.write_char('\n')?;
    }
    Ok(())
}

pub(super) fn cmd_mem(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    let stats = heap::stats();
    writeln!(
        out,
        "Heap: {:#x} - {:#x}, {} KiB used, {} KiB free of {} KiB",
        stats.start,
        stats.start + stats.size,
        stats.used / 1024,
        stats.free() / 1024,
        stats.size / 1024
    )?;
    let Some(info) = boot_info::get() else { return Ok(()) };
    let mut usable = 0;
    if !a.has('s') {
        writeln!(out, "Memory map:")?;
    }
    for region in info.memory_map() {
        if !a.has('s') {
            writeln!(
                out,
                "  {:#014x} - {:#014x}  {}",
                region.base,
                region.base + region.length,
                region.kind_name()
            )?;
        }
        if region.is_usable() {
            usable += region.length;
        }
    }
    writeln!(out, "Usable RAM: {} MiB", usable / (1024 * 1024))?;
    Ok(())
}

pub(super) fn cmd_uptime(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    let ms = pit::uptime_ms();
    if a.has('m') {
        writeln!(out, "{ms}")?;
        return Ok(());
    }
    let secs = ms / 1000;
    writeln!(
        out,
        "up {}h {:02}m {:02}.{:02}s",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        ms % 1000 / 10
    )?;
    Ok(())
}

pub(super) fn cmd_sleep(_: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    let [time] = a.pos[..] else { return Err(CmdError::Usage) };
    let time: u64 = time.parse().map_err(|_| CmdError::Usage)?;
    pit::sleep_ms(if a.has('s') { time.saturating_mul(1000) } else { time });
    Ok(())
}

pub(super) fn cmd_cpuinfo(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    let leaf0 = cpu::cpuid(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&leaf0.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&leaf0.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&leaf0.ecx.to_le_bytes());
    writeln!(out, "Vendor:   {}", core::str::from_utf8(&vendor).unwrap_or("?"))?;

    let max_ext = cpu::cpuid(0x8000_0000, 0).eax;
    if max_ext >= 0x8000_0004 {
        let mut brand = [0u8; 48];
        for (i, leaf) in (0x8000_0002..=0x8000_0004).enumerate() {
            let r = cpu::cpuid(leaf, 0);
            for (j, reg) in [r.eax, r.ebx, r.ecx, r.edx].iter().enumerate() {
                let at = i * 16 + j * 4;
                brand[at..at + 4].copy_from_slice(&reg.to_le_bytes());
            }
        }
        let brand = core::str::from_utf8(&brand).unwrap_or("?");
        writeln!(out, "Model:    {}", brand.trim_matches(|c: char| c == '\0' || c == ' '))?;
    }

    let leaf1 = cpu::cpuid(1, 0);
    let base_family = (leaf1.eax >> 8) & 0xF;
    let family = if base_family == 0xF { base_family + ((leaf1.eax >> 20) & 0xFF) } else { base_family };
    let model = if base_family == 0x6 || base_family == 0xF {
        ((leaf1.eax >> 12) & 0xF0) | ((leaf1.eax >> 4) & 0xF)
    } else {
        (leaf1.eax >> 4) & 0xF
    };
    writeln!(out, "Family {family}, model {model}, stepping {}", leaf1.eax & 0xF)?;

    let ext1 = if max_ext >= 0x8000_0001 { cpu::cpuid(0x8000_0001, 0).edx } else { 0 };
    let features: [(&str, bool); 13] = [
        ("fpu", leaf1.edx & 1 != 0),
        ("tsc", leaf1.edx & (1 << 4) != 0),
        ("apic", leaf1.edx & (1 << 9) != 0),
        ("sse", leaf1.edx & (1 << 25) != 0),
        ("sse2", leaf1.edx & (1 << 26) != 0),
        ("sse3", leaf1.ecx & 1 != 0),
        ("ssse3", leaf1.ecx & (1 << 9) != 0),
        ("sse4.2", leaf1.ecx & (1 << 20) != 0),
        ("x2apic", leaf1.ecx & (1 << 21) != 0),
        ("avx", leaf1.ecx & (1 << 28) != 0),
        ("hypervisor", leaf1.ecx & (1 << 31) != 0),
        ("nx", ext1 & (1 << 20) != 0),
        ("1g-pages", ext1 & (1 << 26) != 0),
    ];
    write!(out, "Features:")?;
    for (name, _) in features.iter().filter(|(_, present)| *present) {
        write!(out, " {name}")?;
    }
    writeln!(out)?;
    Ok(())
}

fn list_colors(out: &mut dyn Write) -> CmdResult {
    write!(out, "colors:")?;
    for (name, _) in Color::ALL {
        write!(out, " {name}")?;
    }
    writeln!(out)?;
    Ok(())
}

pub(super) fn cmd_color(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if a.has('l') {
        return list_colors(out);
    }
    let (fg, bg) = match a.pos[..] {
        [] if a.has('r') => ("lightgray", "black"),
        [fg] => (fg, "black"),
        [fg, bg] => (fg, bg),
        _ => {
            list_colors(out)?;
            return Err(CmdError::Usage);
        }
    };
    let parse = |name: &str| {
        Color::from_name(name)
            .ok_or_else(|| CmdError::Message(alloc::format!("unknown color: {name}")))
    };
    console::set_color(parse(fg)?, parse(bg)?);
    Ok(())
}

pub(super) fn cmd_history(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    if a.has('c') {
        shell.history.clear();
        return Ok(());
    }
    for (i, line) in shell.history.iter().enumerate() {
        writeln!(out, "{:3}  {line}", i + 1)?;
    }
    Ok(())
}

pub(super) fn cmd_shutdown(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    if let Some(seconds) = a.num('t')? {
        writeln!(out, "{} in {seconds} s...", if a.has('r') { "Restarting" } else { "Shutting down" })?;
        pit::sleep_ms(seconds.saturating_mul(1000));
    }
    if a.has('r') {
        writeln!(out, "Restarting...")?;
        cpu::reboot()
    }
    writeln!(out, "Shutting down...")?;
    acpi::power_off();
    writeln!(out, "Power off failed. It is now safe to turn off the computer.")?;
    cpu::halt()
}

pub(super) fn cmd_halt(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    writeln!(out, "System halted. It is now safe to turn off the computer.")?;
    cpu::halt()
}
