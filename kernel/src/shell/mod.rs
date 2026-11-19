//! Interactive command shell.
//!
//! Commands write to a `&mut dyn fmt::Write` rather than straight to the console, so the
//! self-tests can run them and check their output. A name that is not a command is looked up as
//! a runnable file (see `find_runnable`).

mod basic;
mod files;
mod system;

use crate::console::Stdout;
use crate::fs::{self, FsError};
use crate::input::{self, Key};
use crate::{print, println};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use basic::*;
use core::fmt::{self, Write};
use files::*;
use kfs::FileType;
use system::*;

const MAX_LINE: usize = 256;
const MAX_HISTORY: usize = 16;
const MAX_SCRIPT_DEPTH: usize = 8;
/// Where a bare program name is looked for after the current directory.
pub const PROGRAMS_DIR: &str = "/programs";
pub const STARTUP_SCRIPT: &str = "/system/startup.v";

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

impl From<String> for CmdError {
    fn from(message: String) -> Self {
        CmdError::Message(message)
    }
}

impl From<FsError> for CmdError {
    fn from(e: FsError) -> Self {
        CmdError::Message(e.to_string())
    }
}

impl From<kfs::Error> for CmdError {
    fn from(e: kfs::Error) -> Self {
        CmdError::Message(e.to_string())
    }
}

pub type CmdResult = Result<(), CmdError>;
type CmdFn = fn(&mut Shell, &[&str], &mut dyn Write) -> CmdResult;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Files,
    Programs,
    System,
    Shell,
}

impl Category {
    pub const ALL: [Category; 4] = [Category::Files, Category::Programs, Category::System, Category::Shell];

    pub fn title(self) -> &'static str {
        match self {
            Category::Files => "Files:",
            Category::Programs => "Programs:",
            Category::System => "System:",
            Category::Shell => "Shell:",
        }
    }
}

pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub category: Category,
    pub usage: &'static str,
    pub help: &'static str,
    pub run: CmdFn,
}

const fn cmd(
    category: Category,
    name: &'static str,
    aliases: &'static [&'static str],
    usage: &'static str,
    help: &'static str,
    run: CmdFn,
) -> Command {
    Command { name, aliases, category, usage, help, run }
}

use Category::{Files as F, Programs as P, Shell as S, System as Y};

const COMMANDS: &[Command] = &[
    cmd(F, "ls", &["dir"], "[path]", "list a directory", cmd_ls),
    cmd(F, "cd", &[], "[path]", "change the current directory (default /)", cmd_cd),
    cmd(F, "pwd", &[], "", "show the current directory", cmd_pwd),
    cmd(F, "mkdir", &[], "<path...>", "create directories", cmd_mkdir),
    cmd(F, "rd", &["cat"], "[-a] [-x] <file>", "show a file: text, or a hex dump for binary types (-a: whole file, -x: always hex)", cmd_rd),
    cmd(F, "edit", &[], "<file>", "full-screen text editor (Ctrl-S save, Ctrl-Q quit)", cmd_edit),
    cmd(F, "rm", &["del"], "[-r] [-f] <path...>", "delete files (-r: directories with their contents, -f: no questions)", cmd_rm),
    cmd(F, "mv", &["move"], "<from> <to>", "move or rename a file or directory", cmd_mv),
    cmd(F, "cp", &["copy"], "<from> <to>", "copy a file", cmd_cp),
    cmd(F, "info", &[], "<path>", "show a file's type, size and location on disk", cmd_info),
    cmd(F, "df", &[], "", "show disk usage", cmd_df),
    cmd(F, "format", &[], "[-f]", "erase all files except the bootloader and kernel", cmd_format),
    cmd(P, "asm", &[], "<file.asm> [out.vex]", "assemble a program into a .vex executable", cmd_asm),
    cmd(P, "run", &[], "<file> [args...]", "run a program (.vex), assembly file (.asm) or script (.v)", cmd_run),
    cmd(Y, "peek", &[], "<addr> [len]", "hex dump of memory", cmd_peek),
    cmd(Y, "poke", &[], "<addr> <byte...>", "write bytes to memory", cmd_poke),
    cmd(Y, "in", &[], "<port> [b|w|d]", "read an I/O port", cmd_in),
    cmd(Y, "out", &[], "<port> <value> [b|w|d]", "write an I/O port", cmd_out),
    cmd(Y, "sector", &[], "read <lba> [count] | write <lba> <file> [-f]", "raw disk sectors", cmd_sector),
    cmd(Y, "disks", &[], "", "list disks and the boot disk's driver", cmd_disks),
    cmd(Y, "mem", &[], "", "show heap usage and the memory map", cmd_mem),
    cmd(Y, "cpuinfo", &[], "", "show CPU vendor, model and features", cmd_cpuinfo),
    cmd(Y, "uptime", &[], "", "time since boot", cmd_uptime),
    cmd(Y, "shutdown", &[], "[-r]", "power off the computer (-r: restart instead)", cmd_shutdown),
    cmd(Y, "halt", &[], "", "stop the CPU", cmd_halt),
    cmd(S, "help", &[], "[command]", "list commands or describe one", cmd_help),
    cmd(S, "print", &["echo"], "[text...]", "print the arguments", cmd_print),
    cmd(S, "clear", &["cls"], "", "clear the screen", cmd_clear),
    cmd(S, "color", &[], "<fg> [bg]", "set the screen text colors", cmd_color),
    cmd(S, "history", &[], "", "list recent commands", cmd_history),
    cmd(S, "sleep", &[], "<ms>", "wait for a number of milliseconds", cmd_sleep),
];

pub struct Shell {
    history: Vec<String>,
    cwd: String,
    script_depth: usize,
}

impl Shell {
    pub fn new() -> Self {
        Self { history: Vec::new(), cwd: String::from("/"), script_depth: 0 }
    }

    /// Turns a path argument into an absolute path.
    pub fn path(&self, arg: &str) -> String {
        kfs::resolve(&self.cwd, arg)
    }

    /// Runs one command line. An empty line or a `#` comment does nothing.
    pub fn execute(&mut self, line: &str, out: &mut dyn Write) -> CmdResult {
        let args = tokenize(line)?;
        let Some((name, rest)) = args.split_first() else {
            return Ok(());
        };
        if name.starts_with('#') {
            return Ok(());
        }
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        if let Some(command) = find_command(name) {
            return match (command.run)(self, &rest, out) {
                Err(CmdError::Usage) => {
                    Err(CmdError::Message(alloc::format!("usage: {} {}", command.name, command.usage)))
                }
                other => other,
            };
        }
        match self.find_runnable(name) {
            Some(path) => self.run_file(&path, &rest, out),
            None => Err(CmdError::Message(alloc::format!("unknown command: {name} (try 'help')"))),
        }
    }

    /// Finds the file a command name refers to: `name` as a path, then `name` with `.vex`, `.v`
    /// and `.asm` added, first in the current directory and then in `/programs`.
    fn find_runnable(&self, name: &str) -> Option<String> {
        let runnable = |path: &str| {
            fs::with(|fs| fs.stat(path))
                .is_ok_and(|info| !info.is_dir && info.file_type().is_runnable())
        };
        let explicit = self.path(name);
        if runnable(&explicit) {
            return Some(explicit);
        }
        if name.contains('/') {
            return None;
        }
        for dir in [self.cwd.as_str(), PROGRAMS_DIR] {
            for ext in ["", ".vex", ".v", ".asm"] {
                let path = kfs::resolve(dir, &alloc::format!("{name}{ext}"));
                if runnable(&path) {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Runs a program, assembly file or script by absolute path.
    pub fn run_file(&mut self, path: &str, args: &[&str], out: &mut dyn Write) -> CmdResult {
        match FileType::from_name(path) {
            FileType::Script => self.run_script(path, out),
            FileType::Asm | FileType::Exe => {
                let cwd = self.cwd.clone();
                crate::program::run_path(path, args, &cwd, out)
            }
            other => Err(CmdError::Message(alloc::format!("{path}: {} files cannot be run", other.label()))),
        }
    }

    /// Runs each line of a script as a command, stopping at the first error.
    pub fn run_script(&mut self, path: &str, out: &mut dyn Write) -> CmdResult {
        if self.script_depth >= MAX_SCRIPT_DEPTH {
            return Err("scripts are nested too deeply".into());
        }
        let data = fs::with(|fs| fs.read(path))?;
        let text = String::from_utf8_lossy(&data).into_owned();
        self.script_depth += 1;
        let mut result = Ok(());
        for (number, line) in text.lines().enumerate() {
            if let Err(e) = self.execute(line.trim(), out) {
                let message = match e {
                    CmdError::Usage => String::from("usage error"),
                    CmdError::Message(m) => m,
                };
                result = Err(CmdError::Message(alloc::format!("{path} line {}: {message}", number + 1)));
                break;
            }
        }
        self.script_depth -= 1;
        result
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

    fn prompt(&self) {
        print!("{}> ", self.cwd);
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
                    self.prompt();
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

/// Asks a yes/no question on the console. Anything but `y` means no.
pub fn confirm(out: &mut dyn Write, question: &str) -> bool {
    let _ = write!(out, "{question} (y/n) ");
    loop {
        match input::read_key() {
            Key::Char(b'y' | b'Y') => {
                let _ = writeln!(out, "y");
                return true;
            }
            Key::Char(_) | Key::Enter | Key::Escape | Key::Ctrl(_) => {
                let _ = writeln!(out, "n");
                return false;
            }
            _ => {}
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
    COMMANDS.iter().find(|c| c.name == name || c.aliases.contains(&name))
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

/// Parses `0x`-prefixed hexadecimal or decimal.
pub fn parse_num(s: &str) -> Option<u64> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Classic 16-bytes-per-line hex dump, labelled starting at `base`.
pub fn hexdump(out: &mut dyn Write, base: u64, bytes: &[u8]) -> fmt::Result {
    for (i, line) in bytes.chunks(16).enumerate() {
        write!(out, "{:08x} ", base + i as u64 * 16)?;
        for j in 0..16 {
            match line.get(j) {
                Some(b) => write!(out, " {b:02x}")?,
                None => out.write_str("   ")?,
            }
        }
        out.write_str("  ")?;
        for &b in line {
            out.write_char(if (0x20..0x7F).contains(&b) { b as char } else { '.' })?;
        }
        out.write_char('\n')?;
    }
    Ok(())
}

/// Runs the shell forever. Needs interrupts enabled and the timer and keyboard set up.
pub fn run() -> ! {
    let mut shell = Shell::new();
    println!();
    if fs::with(|fs| fs.stat(STARTUP_SCRIPT)).is_ok() {
        println!("Running {STARTUP_SCRIPT}");
        if let Err(error) = shell.run_script(STARTUP_SCRIPT, &mut Stdout) {
            report(error);
        }
    }
    println!("Type 'help' for a list of commands.");
    loop {
        shell.prompt();
        let line = shell.read_line();
        shell.remember(&line);
        if let Err(error) = shell.execute(&line, &mut Stdout) {
            report(error);
        }
    }
}

fn report(error: CmdError) {
    match error {
        CmdError::Usage => println!("usage error"),
        CmdError::Message(message) => println!("error: {message}"),
    }
}
