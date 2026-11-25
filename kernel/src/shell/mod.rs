//! Interactive command shell.
//!
//! Commands write to a `&mut dyn fmt::Write` rather than straight to the console, so the
//! self-tests can run them and check their output. A name that is not a command is looked up as
//! a runnable file (see `find_runnable`).

mod basic;
mod files;
mod system;
mod tasks;

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
use tasks::*;

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
type CmdFn = fn(&mut Shell, &Args, &mut dyn Write) -> CmdResult;

/// One command-line option. Every option has a one-letter form (`-a`) and a long form (`--all`).
pub struct Opt {
    pub short: char,
    pub long: &'static str,
    /// What the option's value is called (`N`, `FILE`), or "" for an option without a value.
    pub value: &'static str,
    pub help: &'static str,
}

/// `opt!('a', "all", "help")` for a flag, `opt!('o', "offset" = "N", "help")` for an option
/// that takes a value.
macro_rules! opt {
    ($short:literal, $long:literal, $help:literal) => {
        Opt { short: $short, long: $long, value: "", help: $help }
    };
    ($short:literal, $long:literal = $value:literal, $help:literal) => {
        Opt { short: $short, long: $long, value: $value, help: $help }
    };
}

/// A command's arguments after option parsing.
pub struct Args<'a> {
    /// The arguments that are not options, in order.
    pub pos: Vec<&'a str>,
    /// Options given, by their one-letter name, with their values.
    opts: Vec<(char, Option<&'a str>)>,
}

impl<'a> Args<'a> {
    /// True if the option was given (in any form).
    pub fn has(&self, short: char) -> bool {
        self.opts.iter().any(|&(c, _)| c == short)
    }

    /// The value of an option that takes one. If it was given twice, the last one counts.
    pub fn value(&self, short: char) -> Option<&'a str> {
        self.opts.iter().rev().find(|&&(c, _)| c == short).and_then(|&(_, v)| v)
    }

    /// The value of an option, parsed as a number (decimal or `0x` hex).
    pub fn num(&self, short: char) -> Result<Option<u64>, CmdError> {
        match self.value(short) {
            None => Ok(None),
            Some(v) => parse_num(v)
                .map(Some)
                .ok_or_else(|| CmdError::Message(alloc::format!("-{short}: not a number: {v}"))),
        }
    }

    /// How many of the given options were used, e.g. to reject `-w -d` together.
    pub fn count_of(&self, shorts: &str) -> usize {
        shorts.chars().filter(|&c| self.has(c)).count()
    }
}

enum Parse<'a> {
    Run(Args<'a>),
    /// `-h` or `--help` was given.
    Help,
}

/// Splits options from the other arguments, the way Linux tools do:
/// - `-a -b` and `-ab` are the same; a value follows the letter (`-n5`) or comes next (`-n 5`)
/// - `--name`, `--name=value` and `--name value`
/// - `--` ends the options; a lone `-` and negative numbers like `-5` are ordinary arguments
/// - `-h` / `--help` shows the command's help
///
/// Options may come anywhere, unless the command is `leading`: then they stop at the first other
/// argument (so `run prog -x` passes `-x` to the program), and an unknown option is taken as an
/// ordinary argument instead of being an error (so `print -x` prints `-x`).
fn parse_args<'a>(command: &Command, raw: &[&'a str]) -> Result<Parse<'a>, CmdError> {
    let mut args = Args { pos: Vec::new(), opts: Vec::new() };
    let mut done = false;
    let mut i = 0;
    let unknown = |what: &str| {
        CmdError::Message(alloc::format!("{}: unknown option {what} (see 'help {}')", command.name, command.name))
    };
    let missing = |what: &str| CmdError::Message(alloc::format!("{}: option {what} needs a value", command.name));
    while i < raw.len() {
        let arg = raw[i];
        i += 1;
        let digit = arg.strip_prefix('-').and_then(|r| r.chars().next()).filter(char::is_ascii_digit);
        let negative_number = digit.is_some_and(|d| !command.options.iter().any(|o| o.short == d));
        if done || arg == "-" || !arg.starts_with('-') || negative_number {
            args.pos.push(arg);
            done |= command.leading;
            continue;
        }
        if arg == "--" {
            done = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            if name == "help" {
                return Ok(Parse::Help);
            }
            let Some(opt) = command.options.iter().find(|o| o.long == name) else {
                if command.leading {
                    args.pos.push(arg);
                    done = true;
                    continue;
                }
                return Err(unknown(arg));
            };
            let value = match (opt.value.is_empty(), inline) {
                (true, None) => None,
                (true, Some(_)) => {
                    return Err(CmdError::Message(alloc::format!("{}: --{name} takes no value", command.name)))
                }
                (false, Some(v)) => Some(v),
                (false, None) => {
                    let v = raw.get(i).ok_or_else(|| missing(arg))?;
                    i += 1;
                    Some(*v)
                }
            };
            args.opts.push((opt.short, value));
            continue;
        }
        // A group of one-letter options. A letter that takes a value ends the group.
        let letters = &arg[1..];
        let mut found = Vec::new();
        let mut help = false;
        let mut bad = None;
        for (at, c) in letters.char_indices() {
            match command.options.iter().find(|o| o.short == c) {
                Some(o) if !o.value.is_empty() => {
                    let rest = &letters[at + c.len_utf8()..];
                    let value = if !rest.is_empty() {
                        rest
                    } else {
                        let v = raw.get(i).ok_or_else(|| missing(&alloc::format!("-{c}")))?;
                        i += 1;
                        *v
                    };
                    found.push((c, Some(value)));
                    break;
                }
                Some(_) => found.push((c, None)),
                None if c == 'h' => help = true,
                None => {
                    bad = Some(c);
                    break;
                }
            }
        }
        if let Some(c) = bad {
            if command.leading {
                args.pos.push(arg);
                done = true;
                continue;
            }
            return Err(unknown(&alloc::format!("-{c}")));
        }
        if help {
            return Ok(Parse::Help);
        }
        args.opts.extend(found);
    }
    Ok(Parse::Run(args))
}

/// The usage line, e.g. `rd [-antx] [-o N] [-c N] <file>`, built from the option table.
pub fn usage_line(c: &Command) -> String {
    let mut line = String::from(c.name);
    let flags: String = c.options.iter().filter(|o| o.value.is_empty()).map(|o| o.short).collect();
    if !flags.is_empty() {
        line.push_str(" [-");
        line.push_str(&flags);
        line.push(']');
    }
    for o in c.options.iter().filter(|o| !o.value.is_empty()) {
        let _ = write!(line, " [-{} {}]", o.short, o.value);
    }
    if !c.usage.is_empty() {
        line.push(' ');
        line.push_str(c.usage);
    }
    line
}

/// Output for `-p`: pauses after every screenful until a key is pressed. Disabled, it passes
/// everything straight through (the self-tests never page).
pub struct Pager<'a> {
    out: &'a mut dyn Write,
    enabled: bool,
    /// Rows to fill before pausing: the screen's rows less one for the prompt.
    page_rows: usize,
    row: usize,
    col: usize,
    quit: bool,
}

impl<'a> Pager<'a> {
    pub fn new(out: &'a mut dyn Write, enabled: bool) -> Self {
        // The command line itself is still on the screen.
        let page_rows = crate::console::rows() - 1;
        Pager { out, enabled, page_rows, row: 1, col: 0, quit: false }
    }

    fn pause(&mut self) -> fmt::Result {
        const PROMPT: &str = "-- more -- Space: next page, Enter: next line, q: quit";
        self.out.write_str(PROMPT)?;
        let key = input::read_key();
        self.out.write_char('\r')?;
        for _ in 0..PROMPT.len() {
            self.out.write_char(' ')?;
        }
        self.out.write_char('\r')?;
        match key {
            Key::Char(b'q' | b'Q') | Key::Escape | Key::Ctrl(_) | Key::Close => self.quit = true,
            Key::Enter | Key::Down => self.row = self.page_rows - 1,
            _ => self.row = 0,
        }
        Ok(())
    }
}

impl Write for Pager<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if !self.enabled {
            return self.out.write_str(s);
        }
        for c in s.chars() {
            if self.quit {
                return Ok(());
            }
            // The screen wraps a full row when the next character comes.
            if c != '\n' && self.col == crate::vga::WIDTH {
                self.row += 1;
                self.col = 0;
            }
            if self.row >= self.page_rows {
                self.pause()?;
                if self.quit {
                    return Ok(());
                }
            }
            self.out.write_char(c)?;
            if c == '\n' {
                self.row += 1;
                self.col = 0;
            } else {
                self.col += 1;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Files,
    Programs,
    Desktop,
    System,
    Shell,
}

impl Category {
    pub const ALL: [Category; 5] =
        [Category::Files, Category::Programs, Category::Desktop, Category::System, Category::Shell];

    pub fn title(self) -> &'static str {
        match self {
            Category::Files => "Files:",
            Category::Programs => "Programs:",
            Category::Desktop => "Desktop:",
            Category::System => "System:",
            Category::Shell => "Shell:",
        }
    }
}

pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub category: Category,
    /// The arguments that are not options, e.g. `<file>`. Options are added from `options`.
    pub usage: &'static str,
    pub help: &'static str,
    pub options: &'static [Opt],
    /// Options only before the first other argument; unknown ones are ordinary arguments.
    pub leading: bool,
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
    Command { name, aliases, category, usage, help, options: &[], leading: false, run }
}

impl Command {
    const fn opts(self, options: &'static [Opt]) -> Command {
        Command { options, ..self }
    }

    const fn leading(self) -> Command {
        Command { leading: true, ..self }
    }
}

use Category::{Desktop as D, Files as F, Programs as P, Shell as S, System as Y};

const PAGE: Opt = opt!('p', "page", "pause after every screenful");

const COMMANDS: &[Command] = &[
    cmd(F, "ls", &["dir"], "[path...]", "list directories", cmd_ls).opts(&[
        opt!('l', "long", "also show sectors and the first sector (LBA)"),
        opt!('1', "names", "names only, one per line"),
        opt!('R', "recursive", "list subdirectories too"),
        opt!('d', "directory", "show a directory itself, not its contents"),
        opt!('S', "size", "sort by size, largest first"),
        opt!('r', "reverse", "reverse the order"),
        PAGE,
    ]),
    cmd(F, "cd", &[], "[path | -]", "change the current directory (default /, - = previous)", cmd_cd),
    cmd(F, "pwd", &[], "", "show the current directory", cmd_pwd),
    cmd(F, "mkdir", &[], "<path...>", "create directories", cmd_mkdir).opts(&[
        opt!('p', "parents", "create missing parent directories; no error if it exists"),
        opt!('v', "verbose", "name each directory created"),
    ]),
    cmd(F, "rd", &["cat"], "<file>", "show a file: text, or a hex dump for binary types", cmd_rd).opts(&[
        opt!('a', "all", "whole hex dump (default: first 512 bytes)"),
        opt!('x', "hex", "hex dump, whatever the type"),
        opt!('t', "text", "as text, whatever the type"),
        opt!('n', "number", "number the lines of text"),
        opt!('o', "offset" = "N", "start at byte N"),
        opt!('c', "count" = "N", "show N bytes at most"),
        PAGE,
    ]),
    cmd(F, "edit", &[], "<file>", "full-screen editor: text, or hex for binary types", cmd_edit).opts(&[
        opt!('x', "hex", "hex editor, whatever the type"),
        opt!('t', "text", "text editor, whatever the type"),
        opt!('l', "line" = "N", "start at line N (text)"),
        opt!('o', "offset" = "N", "start at byte N (hex)"),
    ]),
    cmd(F, "rm", &["del"], "<path...>", "delete files and directories", cmd_rm).opts(&[
        opt!('r', "recursive", "delete directories with their contents"),
        opt!('f', "force", "no questions; skip paths that do not exist"),
        opt!('v', "verbose", "name each path deleted"),
    ]),
    cmd(F, "mv", &["move"], "<from...> <to>", "move or rename files and directories", cmd_mv).opts(&[
        opt!('n', "no-clobber", "never replace an existing file"),
        opt!('i', "interactive", "ask before replacing a file"),
        opt!('v', "verbose", "name each move"),
    ]),
    cmd(F, "cp", &["copy"], "<from...> <to>", "copy files (and directories with -r)", cmd_cp).opts(&[
        opt!('r', "recursive", "copy directories with their contents"),
        opt!('n', "no-clobber", "never replace an existing file"),
        opt!('i', "interactive", "ask before replacing a file"),
        opt!('v', "verbose", "name each copy"),
    ]),
    cmd(F, "info", &[], "<path...>", "show type, size and location on disk", cmd_info),
    cmd(F, "df", &[], "", "show disk usage", cmd_df),
    cmd(F, "format", &[], "", "erase all files except the bootloader and kernel", cmd_format)
        .opts(&[opt!('f', "force", "do not ask")]),
    cmd(P, "asm", &[], "<file.asm> [out.vex]", "assemble a program into a .vex executable", cmd_asm).opts(&[
        opt!('o', "output" = "FILE", "the .vex file to write (default: the source name)"),
        opt!('l', "listing", "show the address and bytes of every line"),
        opt!('n', "dry-run", "only check the source; write nothing"),
        PAGE,
    ]),
    cmd(P, "run", &[], "<file> [args...]", "run a program (.vex), assembly file (.asm) or script (.v)", cmd_run)
        .opts(&[opt!('t', "time", "show how long it ran")])
        .leading(),
    cmd(D, "desktop", &[], "", "start the desktop: home screen, taskbar, Alt+Tab", cmd_desktop),
    cmd(D, "start", &[], "[file [args...]]", "open a file or a new terminal as a task of its own (desktop)", cmd_start)
        .opts(&[opt!('e', "edit", "open the file in the editor, even a program")])
        .leading(),
    cmd(D, "tasks", &["ps"], "", "list running tasks", cmd_tasks),
    cmd(D, "kill", &[], "<id...>", "close tasks (ids from 'tasks')", cmd_kill)
        .opts(&[opt!('f', "force", "end them at once, without letting them finish")]),
    cmd(D, "exit", &[], "", "close this terminal (desktop)", cmd_exit),
    cmd(Y, "peek", &[], "<addr> [len]", "hex dump of memory (default 64 bytes)", cmd_peek).opts(&[
        opt!('w', "words", "show 16-bit values"),
        opt!('d', "dwords", "show 32-bit values"),
        opt!('q', "qwords", "show 64-bit values"),
    ]),
    cmd(Y, "poke", &[], "<addr> <value...>", "write values to memory (bytes by default)", cmd_poke).opts(&[
        opt!('w', "words", "write 16-bit values"),
        opt!('d', "dwords", "write 32-bit values"),
        opt!('q', "qwords", "write 64-bit values"),
    ]),
    cmd(Y, "in", &[], "<port>", "read an I/O port", cmd_in).opts(&[
        opt!('b', "byte", "8 bits (the default)"),
        opt!('w', "word", "16 bits"),
        opt!('d', "dword", "32 bits"),
    ]),
    cmd(Y, "out", &[], "<port> <value>", "write an I/O port", cmd_out).opts(&[
        opt!('b', "byte", "8 bits (the default)"),
        opt!('w', "word", "16 bits"),
        opt!('d', "dword", "32 bits"),
    ]),
    cmd(Y, "sector", &[], "read <lba> [count] | write <lba> <file>", "raw disk sectors", cmd_sector).opts(&[
        opt!('f', "force", "write without asking"),
        opt!('n', "count" = "N", "sectors to read (default 1)"),
        opt!('o', "output" = "FILE", "save the sectors read to a file instead of showing them"),
    ]),
    cmd(Y, "disks", &[], "", "list disks and the boot disk's driver", cmd_disks),
    cmd(Y, "mem", &[], "", "show heap usage and the memory map", cmd_mem)
        .opts(&[opt!('s', "summary", "heap and total usable RAM only")]),
    cmd(Y, "cpuinfo", &[], "", "show CPU vendor, model and features", cmd_cpuinfo),
    cmd(Y, "uptime", &[], "", "time since boot", cmd_uptime)
        .opts(&[opt!('m', "ms", "milliseconds as a plain number")]),
    cmd(Y, "date", &["time"], "[YYYY-MM-DD] [HH:MM[:SS]]", "show the date and time, or set them", cmd_date).opts(&[
        opt!('s', "set", "set the clock to the date and/or time given, and save it"),
        opt!('r', "rtc", "also show the hardware clock and the saved offset"),
    ]),
    cmd(Y, "shutdown", &[], "", "power off the computer", cmd_shutdown).opts(&[
        opt!('r', "restart", "restart instead"),
        opt!('t', "time" = "SECONDS", "wait this long first"),
    ]),
    cmd(Y, "halt", &[], "", "stop the CPU", cmd_halt),
    cmd(S, "help", &[], "[command]", "list commands or describe one", cmd_help).opts(&[
        opt!('a', "all", "every command with its description"),
        PAGE,
    ]),
    cmd(S, "print", &["echo"], "[text...]", "print the arguments", cmd_print)
        .opts(&[
            opt!('n', "no-newline", "no line break at the end"),
            opt!('e', "escapes", "understand \\n \\t \\r \\e \\\\ and \\xHH"),
        ])
        .leading(),
    cmd(S, "clear", &["cls"], "", "clear the screen", cmd_clear),
    cmd(S, "color", &[], "<fg> [bg]", "set the screen text colors", cmd_color).opts(&[
        opt!('l', "list", "list the color names"),
        opt!('r', "reset", "back to light gray on black"),
    ]),
    cmd(S, "history", &[], "", "list recent commands", cmd_history)
        .opts(&[opt!('c', "clear", "forget them")]),
    cmd(S, "sleep", &[], "<time>", "wait (milliseconds by default)", cmd_sleep)
        .opts(&[opt!('s', "seconds", "the time is in seconds")]),
];

pub struct Shell {
    history: Vec<String>,
    cwd: String,
    /// The directory before the last `cd`, for `cd -`.
    prev_cwd: String,
    script_depth: usize,
    /// `exit` was given: the terminal closes after this command.
    exiting: bool,
}

impl Shell {
    pub fn new() -> Self {
        Self::with_cwd("/")
    }

    pub fn with_cwd(cwd: &str) -> Self {
        Self {
            history: Vec::new(),
            cwd: String::from(cwd),
            prev_cwd: String::from(cwd),
            script_depth: 0,
            exiting: false,
        }
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
            let args = match parse_args(command, &rest)? {
                Parse::Run(args) => args,
                Parse::Help => return describe(command, out),
            };
            return match (command.run)(self, &args, out) {
                Err(CmdError::Usage) => Err(CmdError::Message(alloc::format!("usage: {}", usage_line(command)))),
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
    pub(super) fn find_runnable(&self, name: &str) -> Option<String> {
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
            if self.exiting {
                break;
            }
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

    /// Reads one line from the keyboard / serial with basic editing and history. `None` if the
    /// task is being closed.
    fn read_line(&self) -> Option<String> {
        let mut line = String::new();
        // Position in history while browsing with Up/Down; `history.len()` = the line being typed.
        let mut browse = self.history.len();
        let mut draft = String::new();
        loop {
            match input::read_key() {
                Key::Enter => {
                    println!();
                    return Some(line);
                }
                Key::Close => return None,
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
            Key::Char(_) | Key::Enter | Key::Escape | Key::Ctrl(_) | Key::Close => {
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

/// `help <command>` and `<command> --help`: usage, description and options.
fn describe(c: &Command, out: &mut dyn Write) -> CmdResult {
    writeln!(out, "usage: {}", usage_line(c))?;
    writeln!(out, "  {}", c.help)?;
    if !c.aliases.is_empty() {
        writeln!(out, "  also: {}", c.aliases.join(", "))?;
    }
    writeln!(out, "options:")?;
    let line = |out: &mut dyn Write, short: char, long: &str, value: &str, help: &str| {
        let mut left = alloc::format!("  -{short}, --{long}");
        if !value.is_empty() {
            left.push(' ');
            left.push_str(value);
        }
        writeln!(out, "{left:<26}{help}")
    };
    for o in c.options {
        line(out, o.short, o.long, o.value, o.help)?;
    }
    line(out, 'h', "help", "", "show this help")?;
    if c.leading {
        writeln!(out, "Options must come before the other arguments.")?;
    }
    Ok(())
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
    println!("Type 'help' for a list of commands, or 'desktop' for the desktop.");
    loop {
        shell.prompt();
        let Some(line) = shell.read_line() else { continue };
        shell.remember(&line);
        if let Err(error) = shell.execute(&line, &mut Stdout) {
            report(error);
        }
    }
}

/// A terminal on the desktop: a shell in a task of its own, until `exit` or the task is closed.
pub fn terminal(cwd: &str) {
    let me = crate::task::current();
    let mut shell = Shell::with_cwd(cwd);
    println!("Terminal {}. 'exit' closes it; Alt+Tab switches tasks.", crate::task::id_of(me));
    while !shell.exiting {
        shell.prompt();
        let Some(line) = shell.read_line() else { break };
        shell.remember(&line);
        // The taskbar shows what the terminal is doing, e.g. "clock" while that program runs.
        let command = line.split_whitespace().next().unwrap_or("");
        let base = command.rsplit('/').next().unwrap_or(command);
        if !base.is_empty() {
            crate::task::rename(me, base);
        }
        if let Err(error) = shell.execute(&line, &mut Stdout) {
            report(error);
        }
        crate::task::rename(me, "Terminal");
    }
}

pub fn report(error: CmdError) {
    match error {
        CmdError::Usage => println!("usage error"),
        CmdError::Message(message) => println!("error: {message}"),
    }
}
