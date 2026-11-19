//! Interactive command shell.
//!
//! Commands write to a `&mut dyn fmt::Write` rather than straight to the console, so the
//! self-tests can run them and check their output.

use crate::console::{self, Stdout};
use crate::input::{self, Key};
use crate::vga::Color;
use crate::{boot_info, cpu, heap, pit, print, println};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

const PROMPT: &str = "> ";
const MAX_LINE: usize = 256;
const MAX_HISTORY: usize = 16;

pub enum CmdError {
    /// Wrong arguments: prints the command's usage line.
    Usage,
    Message(String),
}

impl From<fmt::Error> for CmdError {
    fn from(_: fmt::Error) -> Self {
        CmdError::Message(String::from("output error"))
    }
}

impl From<&str> for CmdError {
    fn from(message: &str) -> Self {
        CmdError::Message(String::from(message))
    }
}

type CmdResult = Result<(), CmdError>;
type CmdFn = fn(&mut Shell, &[&str], &mut dyn Write) -> CmdResult;

struct Command {
    name: &'static str,
    usage: &'static str,
    help: &'static str,
    run: CmdFn,
}

const COMMANDS: &[Command] = &[
    Command { name: "help", usage: "[command]", help: "list commands or describe one", run: cmd_help },
    Command { name: "clear", usage: "", help: "clear the screen", run: cmd_clear },
    Command { name: "echo", usage: "[text...]", help: "print the arguments", run: cmd_echo },
    Command { name: "mem", usage: "", help: "show heap usage and the memory map", run: cmd_mem },
    Command { name: "uptime", usage: "", help: "time since boot", run: cmd_uptime },
    Command { name: "sleep", usage: "<ms>", help: "wait for a number of milliseconds", run: cmd_sleep },
    Command { name: "cpuinfo", usage: "", help: "show CPU vendor, model and features", run: cmd_cpuinfo },
    Command { name: "color", usage: "<fg> [bg]", help: "set the screen text colors", run: cmd_color },
    Command { name: "history", usage: "", help: "list recent commands", run: cmd_history },
    Command { name: "reboot", usage: "", help: "restart the computer", run: cmd_reboot },
    Command { name: "halt", usage: "", help: "stop the CPU", run: cmd_halt },
];

pub struct Shell {
    history: Vec<String>,
}

impl Shell {
    pub const fn new() -> Self {
        Self { history: Vec::new() }
    }

    /// Runs one command line. An empty line does nothing.
    pub fn execute(&mut self, line: &str, out: &mut dyn Write) -> CmdResult {
        let args = tokenize(line)?;
        let Some((name, rest)) = args.split_first() else {
            return Ok(());
        };
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        let command = find_command(name).ok_or_else(|| {
            CmdError::Message(alloc::format!("unknown command: {name} (try 'help')"))
        })?;
        (command.run)(self, &rest, out)
    }

    fn remember(&mut self, line: &str) {
        if line.trim().is_empty() || self.history.last().is_some_and(|last| last == line) {
            return;
        }
        if self.history.len() == MAX_HISTORY {
            self.history.remove(0);
        }
        self.history.push(String::from(line));
    }

    /// Reads one line from the keyboard / serial with basic editing and history.
    fn read_line(&self) -> String {
        let mut line = String::new();
        // Position in history while browsing with Up/Down; `history.len()` = the line being typed.
        let mut browse = self.history.len();
        let mut draft = String::new();
        loop {
            match input::read_key() {
                Key::Enter => {
                    println!();
                    return line;
                }
                Key::Char(c) if c != b'\t' && line.len() < MAX_LINE => {
                    line.push(c as char);
                    print!("{}", c as char);
                }
                Key::Backspace if !line.is_empty() => {
                    line.pop();
                    print!("\x08 \x08");
                }
                Key::Ctrl(b'c') => {
                    println!("^C");
                    line.clear();
                    print!("{PROMPT}");
                }
                Key::Up if browse > 0 => {
                    if browse == self.history.len() {
                        draft = line.clone();
                    }
                    browse -= 1;
                    replace_line(&mut line, &self.history[browse]);
                }
                Key::Down if browse < self.history.len() => {
                    browse += 1;
                    let text = self.history.get(browse).map_or(draft.as_str(), String::as_str);
                    replace_line(&mut line, text);
                }
                _ => {}
            }
        }
    }
}

/// Erases the displayed line and shows `text` instead.
fn replace_line(line: &mut String, text: &str) {
    for _ in 0..line.len() {
        print!("\x08 \x08");
    }
    line.clear();
    line.push_str(text);
    print!("{line}");
}

fn find_command(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// Splits a line on whitespace. Double quotes group words, including spaces, into one argument.
pub fn tokenize(line: &str) -> Result<Vec<String>, CmdError> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut in_quotes = false;
    for c in line.chars() {
        if in_quotes {
            if c == '"' {
                in_quotes = false;
            } else {
                current.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
            in_word = true;
        } else if c.is_whitespace() {
            if in_word {
                args.push(core::mem::take(&mut current));
                in_word = false;
            }
        } else {
            current.push(c);
            in_word = true;
        }
    }
    if in_quotes {
        return Err("unterminated quote".into());
    }
    if in_word {
        args.push(current);
    }
    Ok(args)
}

/// Runs the shell forever. Needs interrupts enabled and the timer and keyboard set up.
pub fn run() -> ! {
    let mut shell = Shell::new();
    println!();
    println!("Type 'help' for a list of commands.");
    loop {
        print!("{PROMPT}");
        let line = shell.read_line();
        shell.remember(&line);
        if let Err(error) = shell.execute(&line, &mut Stdout) {
            report(&line, error);
        }
    }
}

fn report(line: &str, error: CmdError) {
    match error {
        CmdError::Usage => {
            let name = line.split_whitespace().next().unwrap_or("");
            if let Some(command) = find_command(name) {
                println!("usage: {} {}", command.name, command.usage);
            }
        }
        CmdError::Message(message) => println!("error: {message}"),
    }
}

fn cmd_help(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    match args {
        [] => {
            for c in COMMANDS {
                let usage = alloc::format!("{} {}", c.name, c.usage);
                writeln!(out, "  {usage:<20} {}", c.help)?;
            }
        }
        [name] => {
            let c = find_command(name).ok_or("no such command")?;
            writeln!(out, "{} {}\n  {}", c.name, c.usage, c.help)?;
        }
        _ => return Err(CmdError::Usage),
    }
    Ok(())
}

fn cmd_clear(_: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    console::clear();
    Ok(())
}

fn cmd_echo(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.write_char(' ')?;
        }
        out.write_str(arg)?;
    }
    out.write_char('\n')?;
    Ok(())
}

fn cmd_mem(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
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

fn cmd_uptime(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
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

fn cmd_sleep(_: &mut Shell, args: &[&str], _: &mut dyn Write) -> CmdResult {
    let [ms] = args else { return Err(CmdError::Usage) };
    let ms: u64 = ms.parse().map_err(|_| CmdError::Usage)?;
    pit::sleep_ms(ms);
    Ok(())
}

fn cmd_cpuinfo(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
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

fn cmd_color(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
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

fn cmd_history(shell: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    for (i, line) in shell.history.iter().enumerate() {
        writeln!(out, "{:3}  {line}", i + 1)?;
    }
    Ok(())
}

fn cmd_reboot(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    writeln!(out, "Rebooting...")?;
    cpu::reboot()
}

fn cmd_halt(_: &mut Shell, args: &[&str], out: &mut dyn Write) -> CmdResult {
    if !args.is_empty() {
        return Err(CmdError::Usage);
    }
    writeln!(out, "System halted. It is now safe to turn off the computer.")?;
    cpu::halt()
}
