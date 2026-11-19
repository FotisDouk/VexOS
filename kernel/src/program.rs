//! Running programs: `.vex` executables, and `.asm` files assembled in memory first.
//!
//! A program is loaded at `PROGRAM_BASE` and runs in ring 0 with full privileges, on its own
//! stack at the top of the program region. It ends by returning (`ret`, exit code in rax) or
//! with system call 0. If it faults, or the user presses Ctrl-C, the interrupt handler sends
//! the CPU to `program_return`, which restores the kernel stack saved at entry.
//!
//! System calls: `int 0x80` with the number in rax, arguments in rdi, rsi, rdx, r10; the result
//! comes back in rax (negative = error). See `syscall` for the list.

use crate::input::{self, Key};
use crate::interrupts::{self, InterruptFrame};
use crate::shell::{CmdError, CmdResult};
use crate::vga::Color;
use crate::{boot_info, console, fs, gdt, keyboard};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use kfs::FileType;

pub const PROGRAM_BASE: u64 = vasm::PROGRAM_BASE;
pub const PROGRAM_SIZE: u64 = 16 * 1024 * 1024;
/// Space kept free for the stack at the top of the region.
const MIN_STACK: u64 = 1024 * 1024;
const MAPPED_LIMIT: u64 = 1 << 32;

const RFLAGS_IF: u64 = 1 << 9;
const RFLAGS_DF: u64 = 1 << 10;

static RUNNING: AtomicBool = AtomicBool::new(false);
static BREAK: AtomicBool = AtomicBool::new(false);
/// Kernel RSP saved by `program_enter`, restored by `program_return`.
static KERNEL_RSP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Ending {
    Returned = 0,
    Exited = 1,
    Crashed = 2,
    Stopped = 3,
}

static ENDING: AtomicU8 = AtomicU8::new(Ending::Returned as u8);
static CRASH_VECTOR: AtomicU64 = AtomicU64::new(0);

/// State for system calls while a program runs. Only touched from the program's own thread of
/// execution (directly or from `int 0x80`), never concurrently.
struct Context {
    out: *mut (dyn Write + 'static),
    args: String,
    cwd: String,
}

static mut CONTEXT: Option<Context> = None;

core::arch::global_asm!(
    r#"
    .global program_enter
    .global program_return
program_enter:
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov [rip + {kernel_rsp}], rsp
    mov rsp, rsi
    xor ebp, ebp
    call rdi
program_return:
    mov rsp, [rip + {kernel_rsp}]
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret
    "#,
    kernel_rsp = sym KERNEL_RSP,
);

extern "sysv64" {
    /// Calls `entry` on the stack `stack_top`; returns the program's rax.
    fn program_enter(entry: u64, stack_top: u64) -> u64;
    static program_return: u8;
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

pub fn request_break() {
    BREAK.store(true, Ordering::Relaxed);
}

fn in_program(rip: u64) -> bool {
    (PROGRAM_BASE..PROGRAM_BASE + PROGRAM_SIZE).contains(&rip)
}

/// Makes the interrupted program end: the `iretq` lands in `program_return`.
fn end_program(frame: &mut InterruptFrame, ending: Ending, rax: u64) {
    ENDING.store(ending as u8, Ordering::Relaxed);
    frame.rip = &raw const program_return as u64;
    frame.cs = gdt::KERNEL_CODE as u64;
    frame.rflags = (frame.rflags | RFLAGS_IF) & !RFLAGS_DF;
    frame.rax = rax;
}

pub fn abort_crashed(frame: &mut InterruptFrame) {
    CRASH_VECTOR.store(frame.vector, Ordering::Relaxed);
    end_program(frame, Ending::Crashed, u64::MAX);
}

/// Called after every IRQ: stops the program on Ctrl-C, but only if the interrupt hit program
/// code (not the kernel in the middle of a system call, which may hold locks).
pub fn check_break(frame: &mut InterruptFrame) {
    if is_running() && BREAK.load(Ordering::Relaxed) && in_program(frame.rip) {
        BREAK.store(false, Ordering::Relaxed);
        end_program(frame, Ending::Stopped, u64::MAX);
    }
}

/// Checks that a program-supplied buffer is in mapped memory.
fn buffer(ptr: u64, len: u64) -> Option<(*mut u8, usize)> {
    let end = ptr.checked_add(len)?;
    (end <= MAPPED_LIMIT && len <= PROGRAM_SIZE).then_some((ptr as *mut u8, len as usize))
}

fn bytes<'a>(ptr: u64, len: u64) -> Option<&'a [u8]> {
    buffer(ptr, len).map(|(p, l)| unsafe { core::slice::from_raw_parts(p, l) })
}

fn bytes_mut<'a>(ptr: u64, len: u64) -> Option<&'a mut [u8]> {
    buffer(ptr, len).map(|(p, l)| unsafe { core::slice::from_raw_parts_mut(p, l) })
}

/// Key codes returned by system call 2 for keys that are not characters.
fn key_code(key: Key) -> u64 {
    match key {
        Key::Char(c) => c as u64,
        Key::Ctrl(c) => (c - b'a' + 1) as u64,
        Key::Enter => 10,
        Key::Backspace => 8,
        Key::Escape => 27,
        Key::Up => 0x100,
        Key::Down => 0x101,
        Key::Left => 0x102,
        Key::Right => 0x103,
        Key::Home => 0x104,
        Key::End => 0x105,
        Key::PageUp => 0x106,
        Key::PageDown => 0x107,
        Key::Delete => 0x108,
    }
}

const ERR: u64 = u64::MAX; // -1

/// Handles `int 0x80`. Runs with interrupts as the program had them (trap gate).
pub fn syscall(frame: &mut InterruptFrame) {
    #[allow(static_mut_refs)]
    let Some(ctx) = (unsafe { CONTEXT.as_mut() }) else {
        frame.rax = ERR;
        return;
    };
    if BREAK.swap(false, Ordering::Relaxed) {
        end_program(frame, Ending::Stopped, ERR);
        return;
    }
    let out = unsafe { &mut *ctx.out };
    let (a, b, c, d) = (frame.rdi, frame.rsi, frame.rdx, frame.r10);
    let path = |ptr: u64, len: u64| -> Option<String> {
        let raw = bytes(ptr, len)?;
        Some(kfs::resolve(&ctx.cwd, core::str::from_utf8(raw).ok()?))
    };
    frame.rax = match frame.rax {
        // exit(code)
        0 => return end_program(frame, Ending::Exited, a),
        // print(ptr, len)
        1 => match bytes(a, b) {
            Some(text) => {
                for &ch in text {
                    let _ = out.write_char(if ch == b'\n' || (0x20..0x7F).contains(&ch) { ch as char } else { '?' });
                }
                b
            }
            None => ERR,
        },
        // read_key() -> character or key code
        2 => match input::read_key() {
            Key::Ctrl(b'c') => return end_program(frame, Ending::Stopped, ERR),
            key => key_code(key),
        },
        // read_line(buf, max) -> length
        3 => match bytes_mut(a, b) {
            Some(buf) => match read_line(out, buf) {
                Some(len) => len as u64,
                None => return end_program(frame, Ending::Stopped, ERR),
            },
            None => ERR,
        },
        // uptime_ms()
        4 => crate::pit::uptime_ms(),
        // sleep(ms)
        5 => {
            crate::pit::sleep_ms(a);
            0
        }
        // file_read(path, path_len, buf, max) -> bytes read
        6 => match (path(a, b), bytes_mut(c, d)) {
            (Some(p), Some(buf)) => match fs::with(|fs| fs.read(&p)) {
                Ok(data) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    n as u64
                }
                Err(_) => ERR,
            },
            _ => ERR,
        },
        // file_write(path, path_len, buf, len) -> 0
        7 => match (path(a, b), bytes(c, d)) {
            (Some(p), Some(data)) => fs::with(|fs| fs.write(&p, data)).map_or(ERR, |_| 0),
            _ => ERR,
        },
        // args(buf, max) -> length of the argument string
        8 => match bytes_mut(a, b) {
            Some(buf) => {
                let n = ctx.args.len().min(buf.len());
                buf[..n].copy_from_slice(&ctx.args.as_bytes()[..n]);
                ctx.args.len() as u64
            }
            None => ERR,
        },
        // clear()
        9 => {
            console::clear();
            0
        }
        // set_color(fg, bg)
        10 if a < 16 && b < 16 => {
            console::set_color(Color::ALL[a as usize].1, Color::ALL[b as usize].1);
            0
        }
        // print_dec(value)
        11 => {
            let _ = write!(out, "{}", a as i64);
            0
        }
        // print_hex(value)
        12 => {
            let _ = write!(out, "{a:#x}");
            0
        }
        _ => ERR,
    };
}

/// Line input for system call 3. Returns `None` on Ctrl-C.
fn read_line(out: &mut dyn Write, buf: &mut [u8]) -> Option<usize> {
    let mut len = 0;
    loop {
        match input::read_key() {
            Key::Enter => {
                let _ = out.write_char('\n');
                return Some(len);
            }
            Key::Ctrl(b'c') => return None,
            Key::Backspace if len > 0 => {
                len -= 1;
                let _ = out.write_str("\x08 \x08");
            }
            Key::Char(c) if c != b'\t' && len < buf.len() => {
                buf[len] = c;
                len += 1;
                let _ = out.write_char(c as char);
            }
            _ => {}
        }
    }
}

/// Checks that the program region is ordinary RAM.
fn region_usable() -> bool {
    let end = PROGRAM_BASE + PROGRAM_SIZE;
    boot_info::get().is_some_and(|info| {
        info.memory_map()
            .iter()
            .any(|r| r.is_usable() && r.base <= PROGRAM_BASE && r.base + r.length >= end)
    })
}

/// Loads `image` at PROGRAM_BASE and runs it. Program output goes to `out`.
pub fn run_image(image: &[u8], entry: u32, zero_fill: u32, args: &str, cwd: &str, out: &mut dyn Write) -> CmdResult {
    if is_running() {
        return Err("a program is already running".into());
    }
    if !region_usable() {
        return Err(CmdError::Message(alloc::format!(
            "programs need RAM at {:#x}-{:#x}, which this machine does not have",
            PROGRAM_BASE,
            PROGRAM_BASE + PROGRAM_SIZE
        )));
    }
    if image.len() as u64 + zero_fill as u64 > PROGRAM_SIZE - MIN_STACK {
        return Err("program too large (15 MiB at most)".into());
    }
    unsafe {
        let base = PROGRAM_BASE as *mut u8;
        core::ptr::write_bytes(base, 0, PROGRAM_SIZE as usize);
        core::ptr::copy_nonoverlapping(image.as_ptr(), base, image.len());
        // The context outlives the call below, and is cleared before `out` goes away.
        let raw: *mut (dyn Write + '_) = &mut *out;
        CONTEXT = Some(Context {
            out: core::mem::transmute::<*mut (dyn Write + '_), *mut (dyn Write + 'static)>(raw),
            args: String::from(args),
            cwd: String::from(cwd),
        });
    }
    ENDING.store(Ending::Returned as u8, Ordering::Relaxed);
    BREAK.store(false, Ordering::Relaxed);
    RUNNING.store(true, Ordering::Relaxed);
    let rax = unsafe { program_enter(PROGRAM_BASE + entry as u64, PROGRAM_BASE + PROGRAM_SIZE) };
    RUNNING.store(false, Ordering::Relaxed);
    unsafe { CONTEXT = None };
    // Programs can do anything, including `cli`; the shell needs interrupts.
    crate::cpu::sti();

    match ENDING.load(Ordering::Relaxed) {
        x if x == Ending::Crashed as u8 => {
            let vector = CRASH_VECTOR.load(Ordering::Relaxed);
            Err(CmdError::Message(alloc::format!("program crashed: {}", interrupts::exception_name(vector))))
        }
        x if x == Ending::Stopped as u8 => {
            keyboard::discard_pending();
            let _ = writeln!(out, "^C");
            Err("program stopped (Ctrl-C)".into())
        }
        _ => {
            let code = rax as i64;
            if code != 0 {
                let _ = writeln!(out, "[exit code {code}]");
            }
            Ok(())
        }
    }
}

/// Assembles assembly source for running or saving, with errors that name the file and line.
pub fn assemble(path: &str, source: &[u8]) -> Result<vasm::Program, CmdError> {
    let text = core::str::from_utf8(source).map_err(|_| CmdError::from("the file is not valid text"))?;
    vasm::assemble(text, PROGRAM_BASE)
        .map_err(|e| CmdError::Message(alloc::format!("{path}, line {}: {}", e.line, e.message)))
}

/// Runs a `.vex` or `.asm` file.
pub fn run_path(path: &str, args: &[&str], cwd: &str, out: &mut dyn Write) -> CmdResult {
    let data = fs::with(|fs| fs.read(path))?;
    let args: Vec<&str> = args.to_vec();
    let args = args.join(" ");
    match FileType::from_name(path) {
        FileType::Exe => {
            let (header, image) = vasm::parse_vex(&data).map_err(|e| CmdError::Message(alloc::format!("{path}: {e}")))?;
            run_image(image, header.entry, header.zero_fill, &args, cwd, out)
        }
        FileType::Asm => {
            let program = assemble(path, &data)?;
            run_image(&program.image, program.entry, program.zero_fill, &args, cwd, out)
        }
        _ => Err(CmdError::Message(alloc::format!("{path} is not a program"))),
    }
}
