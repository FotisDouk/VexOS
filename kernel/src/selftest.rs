//! Boot-time checks of the CPU setup. They run on every boot, in QEMU and on real hardware.
//! A failing check panics.

use crate::input::{Key, SerialDecoder};
use crate::interrupts::{self, EXPECT_RESUME};
use crate::keyboard::Decoder;
use crate::shell::{self, CmdError, Shell};
use crate::bios::BiosDisk;
use crate::{acpi, boot_info, cpu, heap, pit, println, program};
#[cfg(feature = "qemu-test")]
use crate::fs;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use kfs::{BlockDevice, Fs, MemDisk, SECTOR};
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
    check("shell options", shell_options);
    check("ACPI power-off info", acpi_power_info);
    check("editor keys", editor_keys);
    check("hex editor keys", hex_editor_keys);
    check("assembly programs", programs);
    #[cfg(feature = "qemu-test")]
    check("crashing program", program_crash);
    check("BIOS disk read", bios_disk_read);
    check("file system on a RAM disk", ram_disk_fs);
    #[cfg(feature = "qemu-test")]
    check("boot disk write and read back", boot_disk_write);
    #[cfg(feature = "qemu-test")]
    check("file commands", file_commands);
}

fn bios_disk_read() {
    let drive = boot_info::get().expect("no BootInfo").boot_drive as u8;
    let mut disk = BiosDisk::open(drive).expect("INT 13h extensions not available for the boot drive");
    let mut buf = vec![0u8; 2 * SECTOR];
    disk.read(0, &mut buf[..SECTOR]).expect("reading LBA 0 failed");
    assert_eq!(&buf[510..512], &[0x55, 0xAA], "LBA 0 is not a boot sector");
    disk.read(kfs::SUPERBLOCK_LBA as u64, &mut buf[..SECTOR]).expect("reading the superblock failed");
    assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()), kfs::MAGIC, "no KFS superblock");
}

fn ram_disk_fs() {
    let mut fs = Fs::format(MemDisk::new(2048)).expect("format");
    fs.mkdir("/d").unwrap();
    fs.write("/d/a.txt", b"hello").unwrap();
    fs.rename("/d/a.txt", "/b.txt").unwrap();
    assert_eq!(fs.read("/b.txt").unwrap(), b"hello");
    fs.remove("/d", false).unwrap();
    assert_eq!(fs.read_dir("/").unwrap().len(), 1);
}

/// Writes through whichever driver the boot disk uses (ATA or INT 13h). Only in the test build:
/// on real hardware the self-tests must not modify the disk.
#[cfg(feature = "qemu-test")]
fn boot_disk_write() {
    let pattern: Vec<u8> = (0..100_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
    fs::with(|fs| {
        fs.write("/selftest.raw", &pattern)?;
        let back = fs.read("/selftest.raw")?;
        assert!(back == pattern, "data read back differs");
        fs.remove("/selftest.raw", false)
    })
    .unwrap_or_else(|e| panic!("file system error: {e}"));
}

fn acpi_power_info() {
    let s5 = b"\x10\x08_S5_\x12\x08\x04\x0A\x05\x00\x0A\x07";
    assert_eq!(acpi::parse_s5(s5), Some((5, 0)), "AML _S5_ parsing");
    let rooted = b"\x08\\_S5_\x12\x06\x04\x00\x0A\x02\x00\x00";
    assert_eq!(acpi::parse_s5(rooted), Some((0, 2)), "AML \\_S5_ parsing");
    let info = acpi::power_info().expect("ACPI FADT or \\_S5 not found");
    assert!(info.pm1a_cnt != 0, "FADT has no PM1a control port");
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
    assert_eq!(feed(&[0xE0, 0x52, 0xE0, 0xD2]), [Key::Insert], "insert");
}

fn serial_decoding() {
    let mut d = SerialDecoder::new();
    let mut feed = |bytes: &[u8]| -> Vec<Key> { bytes.iter().filter_map(|&b| d.feed(b)).collect() };
    assert_eq!(feed(b"hi\r\n"), [Key::Char(b'h'), Key::Char(b'i'), Key::Enter], "CRLF is one Enter");
    assert_eq!(feed(b"\n"), [Key::Enter], "bare LF");
    assert_eq!(feed(b"\x1b[A\x1b[B\x1bOC"), [Key::Up, Key::Down, Key::Right], "arrows");
    assert_eq!(feed(b"\x1b[3~\x7f\x03"), [Key::Delete, Key::Backspace, Key::Ctrl(b'c')], "editing");
    assert_eq!(feed(b"\x1b[2~"), [Key::Insert], "insert");
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
    assert_eq!(run(r#"print a "b c""#), (true, String::from("a b c\n")));
    assert_eq!(run("echo alias"), (true, String::from("alias\n")), "echo alias");
    assert_eq!(run(""), (true, String::new()));
    let (ok, out) = run("help");
    assert!(ok && out.contains("print") && out.contains("help"), "help output: {out}");
    let (ok, out) = run("uptime");
    assert!(ok && out.starts_with("up "), "uptime output: {out}");

    match sh.execute("nosuchcommand", &mut String::new()) {
        Err(CmdError::Message(m)) => assert!(m.starts_with("unknown command"), "message: {m}"),
        _ => panic!("unknown command was not reported"),
    }
    for bad in ["sleep", "sleep x"] {
        match sh.execute(bad, &mut String::new()) {
            Err(CmdError::Message(m)) => assert!(m.starts_with("usage: sleep [-s] <time>"), "message: {m}"),
            _ => panic!("{bad}: usage error not reported"),
        }
    }
}

/// Option parsing, through commands whose output does not depend on the machine.
fn shell_options() {
    let mut sh = Shell::new();
    let mut run = |line: &str| -> Result<String, String> {
        let mut out = String::new();
        match sh.execute(line, &mut out) {
            Ok(()) => Ok(out),
            Err(CmdError::Usage) => Err(String::from("usage")),
            Err(CmdError::Message(m)) => Err(m),
        }
    };
    let ok = |r: Result<String, String>| r.unwrap_or_else(|e| panic!("failed: {e}"));
    // `print` takes options only in front, and prints unknown ones.
    assert_eq!(ok(run("print -n a b")), "a b");
    assert_eq!(ok(run(r#"print -e "a\tb\x41""#)), "a\tbA\n");
    assert_eq!(ok(run("print -ne x")), "x");
    assert_eq!(ok(run("print --no-newline y")), "y");
    assert_eq!(ok(run("print -x -n")), "-x -n\n", "unknown option ends the options");
    assert_eq!(ok(run("print a -n")), "a -n\n", "options after text are text");
    assert_eq!(ok(run("print -- -n")), "-n\n");
    assert_eq!(ok(run("print -5 -")), "-5 -\n", "negative numbers and - are arguments");
    // -h / --help on any command.
    let help = ok(run("rd --help"));
    assert!(help.starts_with("usage: rd [-axtnp] [-o N] [-c N] <file>"), "rd --help: {help}");
    assert!(help.contains("-o, --offset N") && help.contains("-h, --help"), "rd --help: {help}");
    assert_eq!(ok(run("rm -h")), ok(run("help rm")));
    assert!(ok(run("help -a")).contains("sector"));
    // Errors for options a command does not have, or a value that is missing.
    let err = run("ls -z").unwrap_err();
    assert!(err.contains("unknown option -z"), "{err}");
    assert!(run("ls --bogus").unwrap_err().contains("unknown option --bogus"));
    assert!(run("rd -o").unwrap_err().contains("needs a value"));
    assert!(run("uptime --ms=3").unwrap_err().contains("takes no value"));
    // Grouped flags, flags after arguments, and both value forms.
    assert!(ok(run("uptime -m")).trim().parse::<u64>().is_ok());
    assert!(ok(run("color -l")).contains("yellow"));
    ok(run("sleep 0 -s"));
    let peek = ok(run("peek -q 0x1000 16"));
    assert_eq!(peek.lines().count(), 1, "peek -q: {peek}");
    assert_eq!(peek.split_whitespace().count(), 3, "peek -q: {peek}");
    assert!(run("peek -w -d 0x1000").unwrap_err().contains("only one size"));
    ok(run("history -c"));
    assert_eq!(ok(run("history")), "");
}

fn hex_editor_keys() {
    use crate::hexedit::HexEditor;
    let mut ed = HexEditor::open("/selftest-does-not-exist.raw").expect("open new file");
    let keys = |ed: &mut HexEditor, keys: &[Key]| {
        for &k in keys {
            assert!(ed.handle(k), "hex editor quit early");
        }
    };
    // Typing at the end adds bytes; Tab switches to the character column.
    keys(&mut ed, &[Key::Char(b'4'), Key::Char(b'1'), Key::Char(b'\t'), Key::Char(b'B')]);
    // Overwrite the second byte in hex.
    keys(&mut ed, &[Key::Char(b'\t'), Key::Left, Key::Char(b'f'), Key::Char(b'F')]);
    // Insert mode: a new byte at the start.
    keys(&mut ed, &[Key::Insert, Key::Home, Key::Char(b'0'), Key::Char(b'7')]);
    assert_eq!(ed.data(), [0x07, 0x41, 0xFF]);
    keys(&mut ed, &[Key::Delete, Key::Backspace, Key::End]);
    assert_eq!(ed.data(), [0xFF]);
    // Ctrl-G goes to an offset, past the end means the end.
    keys(&mut ed, &[Key::Ctrl(b'g'), Key::Char(b'9'), Key::Enter, Key::Char(b'a'), Key::Char(b'b')]);
    assert_eq!(ed.data(), [0xFF, 0xAB]);
    assert!(ed.handle(Key::Ctrl(b'q')));
    assert!(!ed.handle(Key::Ctrl(b'q')));
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

/// Drives the file commands through the shell on the test disk.
#[cfg(feature = "qemu-test")]
fn file_commands() {
    let mut sh = Shell::new();
    let mut run = |line: &str| -> String {
        let mut out = String::new();
        if let Err(e) = sh.execute(line, &mut out) {
            let message = match e {
                CmdError::Usage => String::from("usage"),
                CmdError::Message(m) => m,
            };
            panic!("'{line}' failed: {message}");
        }
        out
    };
    fs::with(|fs| fs.write("/st-note.txt", b"line one\n\tline two\n")).unwrap();
    fs::with(|fs| fs.write("/st-data.raw", &[0x00, 0x41, 0xFF])).unwrap();
    fs::with(|fs| fs.write("/programs/st-hello.v", b"# comment\nprint hello from a script\n")).unwrap();

    run("mkdir /st-dir");
    run("cd /st-dir");
    assert_eq!(run("pwd"), "/st-dir\n");
    run("cp ../st-note.txt copy.txt");
    run("mv copy.txt renamed.txt");
    assert_eq!(run("rd renamed.txt"), "line one\n    line two\n");
    assert_eq!(run("cat /st-data.raw"), "00000000  00 41 ff                                         .A.\n");
    let listing = run("ls");
    assert!(listing.contains("text") && listing.contains("renamed.txt"), "ls output: {listing}");
    let info = run("info /system/kernel.sys");
    assert!(info.contains("kernel the bootloader loads"), "info output: {info}");
    // A script runs by name, found in /programs.
    assert_eq!(run("st-hello"), "hello from a script\n");
    assert_eq!(run("st-hello.v"), "hello from a script\n");

    // rd options.
    assert_eq!(run("rd -n renamed.txt"), "    1  line one\n    2      line two\n");
    assert_eq!(run("rd -t /st-data.raw"), ".A.\n");
    assert!(run("rd -x renamed.txt").starts_with("00000000  6c 69 6e 65"));
    let part = run("rd -o 1 -c 1 /st-data.raw");
    assert!(part.starts_with("00000001  41 ") && part.ends_with("A\n"), "rd -o -c: {part}");
    // mkdir -p, cd -, cp -r, cp -n, mv -v, ls -1 / -R.
    run("mkdir -p /st-dir/a/b");
    run("mkdir -p /st-dir/a");
    run("cd /");
    run("cd -");
    assert_eq!(run("pwd"), "/st-dir\n");
    run("cp -r /st-dir /st-copy");
    assert!(fs::with(|fs| fs.stat("/st-copy/a/b")).is_ok_and(|i| i.is_dir), "cp -r");
    assert_eq!(run("rd /st-copy/renamed.txt"), "line one\n    line two\n");
    assert!(sh.execute("cp -r /st-dir /st-dir/a", &mut String::new()).is_err(), "copied into itself");
    assert!(sh.execute("cp /st-dir /st-x", &mut String::new()).is_err(), "cp of a directory needs -r");
    fs::with(|fs| fs.write("/st-new.txt", b"new\n")).unwrap();
    let mut run = |line: &str| -> String {
        let mut out = String::new();
        if sh.execute(line, &mut out).is_err() {
            panic!("'{line}' failed");
        }
        out
    };
    run("cp -n /st-new.txt renamed.txt");
    assert_eq!(run("rd renamed.txt"), "line one\n    line two\n", "cp -n replaced a file");
    assert_eq!(run("mv -v /st-new.txt /st-dir"), "/st-new.txt -> /st-dir/st-new.txt\n");
    assert_eq!(run("ls -1"), "a/\nrenamed.txt\nst-new.txt\n");
    assert!(run("ls -R /st-copy").contains("/st-copy/a/b:"));
    // asm listing without writing, and saving raw sectors to a file.
    let listing = run("asm -l -n /programs/hello.asm");
    assert!(listing.contains("04000000 b801000000") && listing.contains("nothing written"), "asm -l: {listing}");
    run("sector read 0 -o /st-boot.raw");
    assert!(run("rd -o 510 /st-boot.raw").starts_with("000001fe  55 aa"));
    run("rm -f /st-nothing-here");
    assert!(run("rm -rv /st-copy").contains("removed /st-copy"));

    run("cd /");
    run("rm -r st-dir st-note.txt st-data.raw st-boot.raw programs/st-hello.v");
    assert!(fs::with(|fs| fs.stat("/st-dir")).is_err(), "rm -r left the directory");
    assert!(!run("ls /").contains("st-"), "files left over");
}

fn editor_keys() {
    use crate::editor::Editor;
    let mut ed = Editor::open("/selftest-does-not-exist.txt").expect("open new file");
    let mut keys = |keys: &[Key]| {
        for &k in keys {
            assert!(ed.handle(k), "editor quit early");
        }
    };
    keys(&[Key::Char(b'a'), Key::Char(b'c'), Key::Left, Key::Char(b'b'), Key::End, Key::Enter]);
    keys(&[Key::Char(b'\t'), Key::Char(b'x'), Key::Up, Key::Home, Key::Delete, Key::Down, Key::End]);
    keys(&[Key::Backspace, Key::Enter, Key::Char(b'z'), Key::Home, Key::Backspace]);
    assert_eq!(ed.text(), b"bc\n    z\n");
    // Unsaved changes: the first quit only warns.
    assert!(ed.handle(Key::Ctrl(b'q')));
    assert!(!ed.handle(Key::Ctrl(b'q')));
}

fn run_asm(source: &str, args: &str) -> (Result<(), String>, String) {
    let program = vasm::assemble(source, program::PROGRAM_BASE).unwrap_or_else(|e| panic!("assembling: {e}"));
    let mut out = String::new();
    let result = program::run_image(&program.image, program.entry, program.zero_fill, args, "/", &mut out)
        .map_err(|e| match e {
            CmdError::Usage => String::from("usage"),
            CmdError::Message(m) => m,
        });
    (result, out)
}

const PRINT_ARGS: &str = "
_start:
    mov eax, 1          ; print(msg, len)
    lea rdi, [msg]
    mov esi, msg_len
    int 0x80
    mov eax, 8          ; args(buf, 64) -> length
    lea rdi, [buf]
    mov esi, 64
    int 0x80
    mov rsi, rax
    mov eax, 1
    lea rdi, [buf]
    int 0x80
    mov eax, 42
    ret
msg: db 'hi '
msg_len equ $ - msg
buf: resb 64
";

fn programs() {
    let (result, out) = run_asm(PRINT_ARGS, "a b");
    assert_eq!(result, Ok(()));
    assert_eq!(out, "hi a b[exit code 42]\n");
    // The exit call ends the program right away.
    let (result, out) = run_asm("mov eax, 0\nmov edi, 7\nint 0x80\nud2", "");
    assert_eq!((result, out.as_str()), (Ok(()), "[exit code 7]\n"));
    assert!(!program::is_running());
}

/// A crashing program prints a register dump and returns to the caller; the kernel survives.
#[cfg(feature = "qemu-test")]
fn program_crash() {
    let (result, _) = run_asm("push rbx\nud2", "");
    assert_eq!(result, Err(String::from("program crashed: #UD invalid opcode")));
    let (result, _) = run_asm("mov rax, 0x800000000000\nmov rax, [rax]", "");
    assert_eq!(result, Err(String::from("program crashed: #GP general protection fault")));
    // Still works afterwards.
    let (result, _) = run_asm("xor eax, eax\nret", "");
    assert_eq!(result, Ok(()));
}
