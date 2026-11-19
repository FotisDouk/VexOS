//! General commands: help, output, timing, CPU and power.

use super::{find_command, Category, CmdError, CmdResult, Shell, COMMANDS};
use crate::vga::Color;
use crate::{acpi, boot_info, console, cpu, heap, pit};
use core::fmt::Write;

pub(super) fn cmd_help(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    match args {
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
            writeln!(out, "Type 'help <command>' for details. Programs and scripts run by name.")?;
        }
        [name] => {
            let c = find_command(name).ok_or("no such command")?;
            writeln!(out, "{} {}\n  {}", c.name, c.usage, c.help)?;
            if !c.aliases.is_empty() {
                writeln!(out, "  also: {}", c.aliases.join(", "))?;
            }
        }
        _ => return Err(CmdError::Usage),
    }
    Ok(())
}

pub(super) fn cmd_clear(_: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    console::clear();
    Ok(())
}

pub(super) fn cmd_print(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.write_char(' ')?;
        }
        out.write_str(arg)?;
    }
    out.write_char('\n')?;
    Ok(())
}

pub(super) fn cmd_mem(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
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
    writeln!(out, "Memory map:")?;
    for region in info.memory_map() {
        writeln!(
            out,
            "  {:#014x} - {:#014x}  {}",
            region.base,
            region.base + region.length,
            region.kind_name()
        )?;
        if region.is_usable() {
            usable += region.length;
        }
    }
    writeln!(out, "Usable RAM: {} MiB", usable / (1024 * 1024))?;
    Ok(())
}

pub(super) fn cmd_uptime(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    let ms = pit::uptime_ms();
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

pub(super) fn cmd_sleep(_: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    let [ms] = args else { return Err(CmdError::Usage) };
    let ms: u64 = ms.parse().map_err(|_| CmdError::Usage)?;
    pit::sleep_ms(ms);
    Ok(())
}

pub(super) fn cmd_cpuinfo(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
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

pub(super) fn cmd_color(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    let (fg, bg) = match args {
        [fg] => (*fg, "black"),
        [fg, bg] => (*fg, *bg),
        _ => {
            write!(out, "colors:")?;
            for (name, _) in Color::ALL {
                write!(out, " {name}")?;
            }
            writeln!(out)?;
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

pub(super) fn cmd_history(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    for (i, line) in shell.history.iter().enumerate() {
        writeln!(out, "{:3}  {line}", i + 1)?;
    }
    Ok(())
}

pub(super) fn cmd_shutdown(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    match args {
        [] => {
            writeln!(out, "Shutting down...")?;
            acpi::power_off();
            writeln!(out, "Power off failed. It is now safe to turn off the computer.")?;
            cpu::halt()
        }
        ["-r"] => {
            writeln!(out, "Restarting...")?;
            cpu::reboot()
        }
        _ => Err(CmdError::Usage),
    }
}

pub(super) fn cmd_halt(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    writeln!(out, "System halted. It is now safe to turn off the computer.")?;
    cpu::halt()
}
