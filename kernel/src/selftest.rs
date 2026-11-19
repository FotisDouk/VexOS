//! Boot-time checks of the CPU setup. They run on every boot, in QEMU and on real hardware.
//! A failing check panics.

use crate::input::{Key, SerialDecoder};
use crate::interrupts::{self, EXPECT_RESUME};
use crate::keyboard::Decoder;
use crate::shell::{self, CmdError, Shell};
use crate::{cpu, heap, pit, println};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;

/// Canonical, but above the 4 GiB that stage2 identity-maps.
const UNMAPPED_ADDR: u64 = 0x0000_1000_0000_0000;
/// Not canonical: bits 63..48 are not a sign extension of bit 47.
const NON_CANONICAL_ADDR: u64 = 0x8000_0000_0000_0000;

const VEC_GENERAL_PROTECTION: u64 = 13;
const VEC_PAGE_FAULT: u64 = 14;

pub fn run() {
    check("breakpoint", breakpoint);
    check("page fault is caught", page_fault);
    check("general protection fault is caught", general_protection);
    check("timer ticks", timer_ticks);
    check("heap alloc and free", heap_alloc_free);
    check("heap large allocation", heap_large);
    check("keyboard scancode decoding", keyboard_decoding);
    check("serial input decoding", serial_decoding);
    check("shell tokenizer", shell_tokenizer);
    check("shell commands", shell_commands);
}

fn timer_ticks() {
    assert!(cpu::interrupts_enabled(), "interrupts are disabled");
    let start = pit::ticks();
    // Spin instead of `hlt`, so that missing timer interrupts fail the check instead of hanging.
    // A tick is due within 10 ms; this allows well over a second.
    for _ in 0..100_000_000u64 {
        if pit::ticks() > start {
            return;
        }
        core::hint::spin_loop();
    }
    panic!("the timer did not tick: IRQ0 is not being delivered");
}

fn heap_alloc_free() {
    let before = heap::stats().used;
    let boxed = Box::new(0x1234_5678_u64);
    let mut v: Vec<u64> = Vec::new();
    for i in 0..1000 {
        v.push(i);
    }
    assert_eq!(*boxed, 0x1234_5678);
    assert_eq!(v.iter().sum::<u64>(), 999 * 1000 / 2);
    drop(v);
    drop(boxed);
    assert_eq!(heap::stats().used, before, "freed memory is not accounted for");

    // First fit with merged free blocks: the same request lands in the same place again.
    let first = Box::new([0u8; 256]);
    let first_addr = &*first as *const [u8; 256];
    drop(first);
    let second = Box::new([0u8; 256]);
    assert_eq!(&*second as *const [u8; 256], first_addr, "freed memory was not reused");
    drop(second);

    // Over-aligned allocation.
    let layout = core::alloc::Layout::from_size_align(64, 4096).unwrap();
    let p = unsafe { alloc::alloc::alloc(layout) };
    assert!(!p.is_null() && p as usize % 4096 == 0, "aligned allocation failed");
    unsafe { alloc::alloc::dealloc(p, layout) };
    assert_eq!(heap::stats().used, before);
}

fn heap_large() {
    const SIZE: usize = 4 * 1024 * 1024;
    let mut big = alloc::vec![0u8; SIZE];
    big[0] = 1;
    big[SIZE - 1] = 2;
    assert_eq!(big[0] + big[SIZE - 1], 3);
}

fn keyboard_decoding() {
    let mut d = Decoder::new();
    let mut feed = |codes: &[u8]| -> Vec<Key> { codes.iter().filter_map(|&c| d.feed(c)).collect() };
    assert_eq!(feed(&[0x1E, 0x9E]), [Key::Char(b'a')], "plain key");
    assert_eq!(feed(&[0x2A, 0x1E, 0x9E, 0xAA, 0x1E]), [Key::Char(b'A'), Key::Char(b'a')], "shift");
    assert_eq!(feed(&[0x2A, 0x02, 0xAA]), [Key::Char(b'!')], "shifted digit");
    assert_eq!(feed(&[0x3A, 0xBA, 0x1E, 0x02, 0x3A, 0xBA]), [Key::Char(b'A'), Key::Char(b'1')], "caps lock");
    assert_eq!(feed(&[0xE0, 0x48, 0xE0, 0xC8, 0xE0, 0x50]), [Key::Up, Key::Down], "arrows");
    assert_eq!(feed(&[0x1D, 0x2E, 0xAE, 0x9D, 0x2E]), [Key::Ctrl(b'c'), Key::Char(b'c')], "ctrl");
    assert_eq!(feed(&[0x1C, 0x0E, 0x39]), [Key::Enter, Key::Backspace, Key::Char(b' ')], "specials");
    assert_eq!(feed(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5, 0x1E]), [Key::Char(b'a')], "pause skipped");
}

fn serial_decoding() {
    let mut d = SerialDecoder::new();
    let mut feed = |bytes: &[u8]| -> Vec<Key> { bytes.iter().filter_map(|&b| d.feed(b)).collect() };
    assert_eq!(feed(b"hi\r\n"), [Key::Char(b'h'), Key::Char(b'i'), Key::Enter], "CRLF is one Enter");
    assert_eq!(feed(b"\n"), [Key::Enter], "bare LF");
    assert_eq!(feed(b"\x1b[A\x1b[B\x1bOC"), [Key::Up, Key::Down, Key::Right], "arrows");
    assert_eq!(feed(b"\x1b[3~\x7f\x03"), [Key::Delete, Key::Backspace, Key::Ctrl(b'c')], "editing");
}

fn shell_tokenizer() {
    let tokens = |line: &str| match shell::tokenize(line) {
        Ok(args) => args,
        Err(_) => panic!("tokenize({line:?}) failed"),
    };
    assert_eq!(tokens("  echo   a  b "), ["echo", "a", "b"]);
    assert_eq!(tokens(r#"echo "a  b" c"#), ["echo", "a  b", "c"]);
    assert_eq!(tokens(r#"x"y z"w """#), ["xy zw", ""]);
    assert!(tokens("   ").is_empty());
    assert!(shell::tokenize(r#"echo "open"#).is_err(), "unterminated quote accepted");
}

fn shell_commands() {
    let mut sh = Shell::new();
    let mut run = |line: &str| -> (bool, String) {
        let mut out = String::new();
        let ok = sh.execute(line, &mut out).is_ok();
        (ok, out)
    };
    assert_eq!(run(r#"echo a "b c""#), (true, String::from("a b c\n")));
    assert_eq!(run(""), (true, String::new()));
    let (ok, out) = run("help");
    assert!(ok && out.contains("echo") && out.contains("help"), "help output: {out}");
    let (ok, out) = run("uptime");
    assert!(ok && out.starts_with("up "), "uptime output: {out}");

    match sh.execute("nosuchcommand", &mut String::new()) {
        Err(CmdError::Message(m)) => assert!(m.starts_with("unknown command"), "message: {m}"),
        _ => panic!("unknown command was not reported"),
    }
    assert!(matches!(sh.execute("sleep", &mut String::new()), Err(CmdError::Usage)));
    assert!(matches!(sh.execute("sleep x", &mut String::new()), Err(CmdError::Usage)));
}

fn check(name: &str, test: fn()) {
    test();
    println!("selftest: {name} ... ok");
}

fn breakpoint() {
    unsafe { asm!("int3", options(nomem, nostack)) };
}

fn page_fault() {
    let fault = read_expecting_fault(UNMAPPED_ADDR, VEC_PAGE_FAULT);
    assert_eq!(fault.cr2, UNMAPPED_ADDR, "page fault reported the wrong address in CR2");
}

fn general_protection() {
    read_expecting_fault(NON_CANONICAL_ADDR, VEC_GENERAL_PROTECTION);
}

/// Reads `addr`, which must fault with `vector`. The fault handler resumes at label `2:`.
fn read_expecting_fault(addr: u64, vector: u64) -> interrupts::CaughtFault {
    interrupts::expect_fault(vector);
    unsafe {
        asm!(
            "lea {tmp}, [rip + 2f]",
            "mov qword ptr [rip + {resume}], {tmp}",
            "mov {tmp}, qword ptr [{addr}]",
            "2:",
            addr = in(reg) addr,
            resume = sym EXPECT_RESUME,
            tmp = out(reg) _,
            options(nostack),
        );
    }
    match interrupts::take_caught_fault() {
        Some(fault) => {
            assert_eq!(fault.vector, vector, "caught the wrong exception");
            fault
        }
        None => panic!("reading {addr:#x} did not fault (expected vector {vector})"),
    }
}
