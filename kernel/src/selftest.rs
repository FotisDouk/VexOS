//! Boot-time checks of the CPU setup. They run on every boot, in QEMU and on real hardware.
//! A failing check panics.

use crate::input::{self, Key, SerialDecoder};
use crate::interrupts::{self, EXPECT_RESUME};
use crate::keyboard::Decoder;
use crate::shell::{self, CmdError, Shell};
use crate::bios::BiosDisk;
use crate::rtc::{self, DateTime};
use crate::sync::SpinLock;
use crate::vga::Color;
use crate::{acpi, boot_info, clock, console, cpu, heap, paging, pit, println, program, task};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    check("real-time clock", rtc_read);
    check("calendar arithmetic", clock_math);
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
    check("tasks are preempted", tasks_preempted);
    check("each task has its own screen", task_screens);
    check("keys go to a task's own queue", task_input);
    check("programs run at the same time", programs_concurrent);
    check("closing a task stops its program", close_busy_program);
    check("desktop home screen keys", desktop_home);
    check("Alt+Tab switcher", switcher);
    check("desktop commands outside the desktop", desktop_commands);
    check("BIOS disk read", bios_disk_read);
    check("file system on a RAM disk", ram_disk_fs);
    #[cfg(feature = "qemu-test")]
    check("boot disk write and read back", boot_disk_write);
    #[cfg(feature = "qemu-test")]
    check("file commands", file_commands);
    #[cfg(feature = "qemu-test")]
    check("date command", date_command);
    // The tasks the checks started are gone: the first task the user opens is task 1.
    task::reap();
    task::restart_ids();
}

/// Sleeps in 10 ms steps until `done` is true; panics after about 2 seconds.
fn wait_for(what: &str, done: impl Fn() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        pit::sleep_ms(10);
    }
    panic!("timed out waiting: {what}");
}

fn tasks_preempted() {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);
    task::reap();
    let heap_before = heap::stats().used;
    COUNT.store(0, Ordering::Relaxed);
    STOP.store(false, Ordering::Relaxed);
    let slot = task::spawn("selftest counter", || {
        while !STOP.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
            core::hint::spin_loop();
        }
    })
    .expect("spawn");
    assert!(task::is_alive(slot));
    // Spin without yielding: the counter only moves if the timer switches tasks.
    let start = pit::ticks();
    while pit::ticks() < start + 5 {
        core::hint::spin_loop();
    }
    assert!(COUNT.load(Ordering::Relaxed) > 0, "the other task never ran: no preemption");
    STOP.store(true, Ordering::Relaxed);
    task::join(slot);
    task::reap();
    assert!(task::list().iter().all(|t| t.slot != slot), "the ended task was not cleaned up");
    assert_eq!(heap::stats().used, heap_before, "the task's memory was not all freed");
}

fn task_screens() {
    let slot = task::spawn("selftest screen", || {
        println!("hello from a task");
        console::set_color(Color::Yellow, Color::Blue);
        println!("second line");
    })
    .expect("spawn");
    task::join(slot);
    assert_eq!(console::row_text(slot, 0).as_deref(), Some("hello from a task"));
    assert_eq!(console::row_text(slot, 1).as_deref(), Some("second line"));
    let on_boot_screen = (0..25).any(|row| console::row_text(task::BOOT, row).is_some_and(|t| t.contains("hello from")));
    assert!(!on_boot_screen, "a hidden task's output reached the screen shown");
    assert_eq!(console::visible(), task::BOOT);
    task::reap();
    assert_eq!(console::row_text(slot, 0), None, "the screen was not freed");
}

fn task_input() {
    static GOT: SpinLock<Vec<Key>> = SpinLock::new(Vec::new());
    GOT.lock().clear();
    let slot = task::spawn("selftest input", || loop {
        let key = input::read_key();
        GOT.lock().push(key);
        if key == Key::Close {
            break;
        }
    })
    .expect("spawn");
    input::push(slot, Key::Char(b'x'));
    input::push(slot, Key::Up);
    wait_for("the task reading its keys", || GOT.lock().len() >= 2);
    task::close(slot);
    task::join(slot);
    assert_eq!(*GOT.lock(), [Key::Char(b'x'), Key::Up, Key::Close]);
    task::reap();
}

/// Stores its first argument character at `marker`, reads a file into its own memory (through
/// the disk driver, which may call the BIOS), waits 150 ms, then returns what is at `marker`.
const MARKER_PROGRAM: &str = "
_start:
    mov eax, 8          ; args(buf, 8)
    lea rdi, [buf]
    mov esi, 8
    int 0x80
    movzx ebx, byte [buf]
    mov [marker], rbx
    mov eax, 6          ; file_read(path, len, data, 16)
    lea rdi, [path]
    mov esi, path_len
    lea rdx, [data]
    mov r10d, 16
    int 0x80
    cmp rax, 16
    jne .bad
    cmp byte [data + 15], 0xFF
    je .bad
    mov eax, 4          ; uptime_ms
    int 0x80
    lea r12, [rax + 150]
.wait:
    mov eax, 4
    int 0x80
    cmp rax, r12
    jb .wait
    mov rax, [marker]
    ret
.bad:
    xor eax, eax
    ret
marker: dq 0
buf: resb 8
path: db '/system/kernel.sys'
path_len equ $ - path
data: times 16 db 0xFF
";

fn programs_concurrent() {
    if paging::usable_slots() < 2 {
        println!("  (skipped: RAM for only {} program at a time)", paging::usable_slots());
        return;
    }
    static RESULTS: SpinLock<Vec<(u8, String)>> = SpinLock::new(Vec::new());
    RESULTS.lock().clear();
    let start = pit::uptime_ms();
    let slots: Vec<usize> = [b'A', b'B']
        .into_iter()
        .map(|marker| {
            task::spawn("selftest program", move || {
                let arg = String::from(marker as char);
                let (result, out) = run_asm(MARKER_PROGRAM, &arg);
                assert_eq!(result, Ok(()), "program {arg}");
                RESULTS.lock().push((marker, out));
            })
            .expect("spawn")
        })
        .collect();
    for &slot in &slots {
        task::join(slot);
    }
    let elapsed = pit::uptime_ms() - start;
    let mut results = core::mem::take(&mut *RESULTS.lock());
    results.sort();
    assert_eq!(
        results,
        [(b'A', String::from("[exit code 65]\n")), (b'B', String::from("[exit code 66]\n"))],
        "each program must see its own memory"
    );
    assert!(elapsed < 280, "the two 150 ms programs took {elapsed} ms: they did not run at the same time");
    assert_eq!(cpu::read_cr3() & !0xFFF, paging::kernel_cr3(), "page tables not restored");
    task::reap();
    assert!(paging::owners().iter().all(Option::is_none), "program memory not given back");
}

fn close_busy_program() {
    static RESULT: SpinLock<Option<Result<(), String>>> = SpinLock::new(None);
    *RESULT.lock() = None;
    let slot = task::spawn("selftest loop", || {
        let (result, _) = run_asm("_start:\n    jmp _start", "");
        *RESULT.lock() = Some(result);
    })
    .expect("spawn");
    wait_for("the program to start", || program::is_running_in(slot));
    task::close(slot);
    task::join(slot);
    assert_eq!(*RESULT.lock(), Some(Err(String::from("program stopped (Ctrl-C)"))));
    task::reap();
    assert_eq!(paging::slot_of_task(slot), None);
}

fn desktop_home() {
    use crate::desktop::home::{icons_for, Action, Home, Kind, DESKTOP_DIR};
    use kfs::{Extent, FileType, Info};
    let entry = |name: &str, is_dir: bool| Info { name: String::from(name), is_dir, extent: Extent { start: 0, sectors: 0, size: 0 } };
    let entries = || vec![entry("notes.txt", false), entry("games", true), entry("hello.vex", false), entry("a.raw", false)];

    // /desktop: Terminal and Files first, then folders, then files by name.
    let icons = icons_for(DESKTOP_DIR, entries());
    let names: Vec<&str> = icons.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["Terminal", "Files", "games", "a.raw", "hello.vex", "notes.txt"]);
    assert_eq!(icons[0].kind, Kind::Terminal);
    assert_eq!(icons[2].kind, Kind::File(FileType::Dir));
    assert_eq!(icons_for("/x", Vec::new())[0].kind, Kind::Up, "other folders start with ..");
    assert!(icons_for("/", Vec::new()).is_empty(), "the root has no parent");

    let mut home = Home::new();
    home.set_folder(DESKTOP_DIR, icons);
    assert_eq!(home.handle(Key::Enter), Action::NewTerminal);
    assert_eq!(home.handle(Key::Right), Action::None);
    assert_eq!(home.handle(Key::Enter), Action::Enter(String::from("/")), "Files shows the disk");
    // 5 icons per row: Down from the 2nd icon reaches the 6th (notes.txt), Up goes back.
    home.handle(Key::Down);
    assert_eq!(home.selected().unwrap().name, "notes.txt");
    assert_eq!(home.handle(Key::Enter), Action::Open(String::from("/desktop/notes.txt")));
    assert_eq!(home.handle(Key::Char(b'e')), Action::Edit(String::from("/desktop/notes.txt")));
    home.handle(Key::Up);
    assert_eq!(home.selected().unwrap().name, "Terminal");
    home.handle(Key::Right);
    home.handle(Key::Right);
    assert_eq!(home.selected().unwrap().name, "games");
    assert_eq!(home.handle(Key::Enter), Action::Enter(String::from("/desktop/games")));
    assert_eq!(home.handle(Key::Char(b'e')), Action::None, "folders are not edited");
    home.handle(Key::End);
    assert_eq!(home.selected().unwrap().name, "notes.txt");
    home.handle(Key::Right);
    assert_eq!(home.selected().unwrap().name, "notes.txt", "stays on the last icon");
    assert_eq!(home.handle(Key::Char(b't')), Action::NewTerminal);
    assert_eq!(home.handle(Key::Backspace), Action::Enter(String::from("/")));
    assert_eq!(home.handle(Key::Escape), Action::Enter(String::from(DESKTOP_DIR)));

    // In a subfolder: Backspace and .. go up; from / Backspace returns to /desktop.
    home.set_folder("/desktop/games", icons_for("/desktop/games", vec![entry("x.asm", false)]));
    assert_eq!(home.selected().unwrap().kind, Kind::Up, "a new folder starts at the first icon");
    assert_eq!(home.handle(Key::Enter), Action::Enter(String::from(DESKTOP_DIR)));
    assert_eq!(home.handle(Key::Backspace), Action::Enter(String::from(DESKTOP_DIR)));
    home.set_folder("/", icons_for("/", entries()));
    assert_eq!(home.handle(Key::Backspace), Action::Enter(String::from(DESKTOP_DIR)));
    assert_eq!(home.icons().len(), 4);

    // Leaving: Ctrl-Q, then the answer to the question.
    assert_eq!(home.handle(Key::Ctrl(b'q')), Action::Quit);
    home.ask_quit(2);
    assert!(home.message.contains("2 open programs"), "{}", home.message);
    assert_eq!(home.handle(Key::Char(b'n')), Action::None);
    assert!(home.message.is_empty());
    home.ask_quit(1);
    assert_eq!(home.handle(Key::Char(b'y')), Action::QuitNow);
}

fn switcher() {
    use crate::desktop::Switcher;
    const HOME: usize = 0;
    let mut s = Switcher::new();
    s.mru = vec![HOME, 3, 5];
    // PS/2: Alt+Tab opens the switcher on the previous task; Tab moves; letting go switches.
    assert!(s.key(Key::Alt(b'\t'), true, HOME));
    assert_eq!(s.open, Some(1));
    assert!(s.key(Key::Alt(b'\t'), true, HOME));
    assert_eq!(s.open, Some(2));
    assert!(s.key(Key::Alt(b'\t'), true, HOME));
    assert_eq!(s.open, Some(0), "wraps around");
    assert!(s.key(Key::Char(b'x'), true, HOME), "other keys are swallowed while it is open");
    s.key(Key::Left, true, HOME);
    assert_eq!(s.open, Some(2));
    s.alt_up();
    assert_eq!((s.open, s.switch_to), (None, Some(5)));
    s.switch_to = None;
    s.shown(5);
    assert_eq!(s.mru, [5, HOME, 3]);
    // Esc cancels.
    s.key(Key::Alt(b'\t'), true, HOME);
    s.key(Key::Escape, true, HOME);
    s.alt_up();
    assert_eq!((s.open, s.switch_to), (None, None));
    // Serial: ESC TAB switches at once; Alt+H goes home.
    assert!(s.key(Key::Alt(b'\t'), false, HOME));
    assert_eq!((s.open, s.switch_to), (None, Some(HOME)));
    assert!(s.key(Key::Alt(b'h'), false, HOME));
    // Not a hotkey: goes to the task.
    assert!(!s.key(Key::Char(b'\t'), true, HOME));
    assert!(!s.key(Key::Alt(b'x'), true, HOME));
    // A task that ended leaves the list; alone, Alt+Tab does nothing.
    s.forget(5);
    s.forget(3);
    s.switch_to = None;
    assert!(s.key(Key::Alt(b'\t'), true, HOME));
    assert_eq!((s.open, s.switch_to), (None, None));
}

fn desktop_commands() {
    let mut sh = Shell::new();
    let mut run = |line: &str| -> Result<String, String> {
        let mut out = String::new();
        match sh.execute(line, &mut out) {
            Ok(()) => Ok(out),
            Err(CmdError::Usage) => Err(String::from("usage")),
            Err(CmdError::Message(m)) => Err(m),
        }
    };
    let tasks = run("tasks").expect("tasks");
    assert!(tasks.contains("running") && tasks.contains("shell"), "tasks: {tasks}");
    assert!(run("start").unwrap_err().contains("desktop is not running"));
    assert!(run("start /desktop/welcome.txt").unwrap_err().contains("desktop is not running"));
    assert!(run("exit").unwrap_err().contains("boot shell"));
    assert!(run("kill 0").unwrap_err().contains("cannot be closed"));
    assert!(run("kill 9999").unwrap_err().contains("no task 9999"));
    assert_eq!(run("kill").unwrap_err(), "usage: kill [-f] <id...>");
    let help = run("help").expect("help");
    assert!(help.contains("Desktop:") && help.contains("desktop start tasks kill exit"), "help: {help}");
}

fn rtc_read() {
    let now = rtc::read();
    assert!(now.is_valid(), "the RTC reports an impossible date: {now}");
    // QEMU starts the RTC at the host's time; real machines may be anywhere, but not before 2000.
    assert!(now.year >= 2000, "RTC year {}", now.year);
}

fn clock_math() {
    let dt = |year, month, day, hour, minute, second| DateTime { year, month, day, hour, minute, second };
    assert_eq!(dt(1970, 1, 1, 0, 0, 0).to_unix(), 0);
    assert_eq!(dt(2000, 3, 1, 0, 0, 0).to_unix(), 951_868_800);
    assert_eq!(dt(2038, 1, 19, 3, 14, 8).to_unix(), 1 << 31);
    for t in [0, 86_399, 951_782_400, 1_709_164_800, 4_102_444_799, 253_402_300_799] {
        assert_eq!(DateTime::from_unix(t).to_unix(), t, "round trip of {t}");
    }
    assert_eq!(DateTime::from_unix(1_709_164_800), dt(2024, 2, 29, 0, 0, 0), "leap day");
    assert_eq!(dt(1970, 1, 1, 0, 0, 0).weekday_name(), "Thu");
    assert_eq!(dt(2026, 11, 25, 12, 0, 0).weekday_name(), "Wed");
    assert_eq!(dt(2000, 2, 29, 0, 0, 0).weekday_name(), "Tue");
    assert_eq!(DateTime::parse_date("2024-02-29"), Some((2024, 2, 29)));
    assert_eq!(DateTime::parse_date("2023-02-29"), None, "not a leap year");
    assert_eq!(DateTime::parse_date("2024-13-01"), None);
    assert_eq!(DateTime::parse_time("23:59"), Some((23, 59, 0)));
    assert_eq!(DateTime::parse_time("7:05:09"), Some((7, 5, 9)));
    assert_eq!(DateTime::parse_time("24:00"), None);
    assert_eq!(alloc::format!("{}", dt(2026, 1, 2, 3, 4, 5)), "2026-01-02 03:04:05");
    assert_eq!(clock::parse_config(&clock::config_text(-3600)), Some(-3600));
    assert_eq!(clock::parse_config("# x\n\noffset   42\n"), Some(42));
    assert_eq!(clock::parse_config("offset x\n"), None);
}

/// Sets the clock through the shell, then puts the old setting back.
#[cfg(feature = "qemu-test")]
fn date_command() {
    let saved = clock::offset();
    let had_file = fs::with(|fs| fs.stat(clock::CONFIG_PATH)).is_ok();
    let mut sh = Shell::new();
    let mut out = String::new();
    assert!(sh.execute("date -s 2030-01-02 03:04:05", &mut out).is_ok(), "date -s failed");
    assert!(out.starts_with("Wed 2030-01-02 03:04:0"), "date -s output: {out}");
    let saved_text = fs::with(|fs| fs.read(clock::CONFIG_PATH)).expect("clock.cfg not written");
    assert_eq!(clock::parse_config(&String::from_utf8_lossy(&saved_text)), Some(clock::offset()));
    out.clear();
    assert!(sh.execute("date -s 12:00", &mut out).is_ok());
    assert!(out.starts_with("Wed 2030-01-02 12:00:0"), "time only keeps the date: {out}");
    assert!(sh.execute("date -s 2030-02-30", &mut String::new()).is_err(), "bad date accepted");
    assert!(sh.execute("date 12:00", &mut String::new()).is_err(), "setting without -s");
    out.clear();
    assert!(sh.execute("date -r", &mut out).is_ok() && out.contains("hardware clock"), "date -r: {out}");

    clock::set_offset(saved);
    if had_file {
        clock::save().unwrap();
    } else {
        fs::with(|fs| fs.remove(clock::CONFIG_PATH, false)).unwrap();
    }
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

fn scancodes(d: &mut Decoder, codes: &[u8]) -> Vec<Key> {
    codes.iter().filter_map(|&c| d.feed(c)).collect()
}

/// Feeds serial bytes, collecting every key (including a second key left pending).
fn serial_keys(d: &mut SerialDecoder, bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    for &b in bytes {
        keys.extend(d.feed(b));
        keys.extend(d.take_pending());
    }
    keys
}

fn keyboard_decoding() {
    let mut d = Decoder::new();
    assert_eq!(scancodes(&mut d, &[0x1E, 0x9E]), [Key::Char(b'a')], "plain key");
    assert_eq!(scancodes(&mut d, &[0x2A, 0x1E, 0x9E, 0xAA, 0x1E]), [Key::Char(b'A'), Key::Char(b'a')], "shift");
    assert_eq!(scancodes(&mut d, &[0x2A, 0x02, 0xAA]), [Key::Char(b'!')], "shifted digit");
    assert_eq!(scancodes(&mut d, &[0x3A, 0xBA, 0x1E, 0x02, 0x3A, 0xBA]), [Key::Char(b'A'), Key::Char(b'1')], "caps lock");
    assert_eq!(scancodes(&mut d, &[0xE0, 0x48, 0xE0, 0xC8, 0xE0, 0x50]), [Key::Up, Key::Down], "arrows");
    assert_eq!(scancodes(&mut d, &[0x1D, 0x2E, 0xAE, 0x9D, 0x2E]), [Key::Ctrl(b'c'), Key::Char(b'c')], "ctrl");
    assert_eq!(scancodes(&mut d, &[0x1C, 0x0E, 0x39]), [Key::Enter, Key::Backspace, Key::Char(b' ')], "specials");
    assert_eq!(scancodes(&mut d, &[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5, 0x1E]), [Key::Char(b'a')], "pause skipped");
    assert_eq!(scancodes(&mut d, &[0xE0, 0x52, 0xE0, 0xD2]), [Key::Insert], "insert");
    // Alt: held across several keys, left (38) or right (E0 38).
    assert_eq!(scancodes(&mut d, &[0x38, 0x0F, 0x8F, 0x0F, 0xB8, 0x0F]), [Key::Alt(b'\t'), Key::Alt(b'\t'), Key::Char(b'\t')], "alt+tab");
    assert_eq!(scancodes(&mut d, &[0xE0, 0x38, 0x23]), [Key::Alt(b'h')], "right alt");
    assert!(d.alt_held());
    assert_eq!(scancodes(&mut d, &[0xE0, 0xB8, 0x23]), [Key::Char(b'h')], "alt released");
    assert!(!d.alt_held());
}

fn serial_decoding() {
    let mut d = SerialDecoder::new();
    assert_eq!(serial_keys(&mut d, b"hi\r\n"), [Key::Char(b'h'), Key::Char(b'i'), Key::Enter], "CRLF is one Enter");
    assert_eq!(serial_keys(&mut d, b"\n"), [Key::Enter], "bare LF");
    assert_eq!(serial_keys(&mut d, b"\x1b[A\x1b[B\x1bOC"), [Key::Up, Key::Down, Key::Right], "arrows");
    assert_eq!(serial_keys(&mut d, b"\x1b[3~\x7f\x03"), [Key::Delete, Key::Backspace, Key::Ctrl(b'c')], "editing");
    assert_eq!(serial_keys(&mut d, b"\x1b[2~"), [Key::Insert], "insert");
    // ESC plus a key is Alt plus that key (what terminals send for Alt).
    assert_eq!(serial_keys(&mut d, b"\x1b\t\x1bh"), [Key::Alt(b'\t'), Key::Alt(b'h')], "alt");
    // Modifier parameters are skipped: Alt+Up, Ctrl+PgUp.
    assert_eq!(serial_keys(&mut d, b"\x1b[1;3A\x1b[5;5~"), [Key::Up, Key::PageUp], "modifiers");
    // A lone ESC is the Escape key once nothing follows it.
    let now = pit::ticks();
    assert_eq!(serial_keys(&mut d, b"\x1b"), [], "escape waits");
    assert_eq!(d.timeout(now), None, "escape too early");
    assert_eq!(d.timeout(now + input::ESC_TIMEOUT_TICKS + 1), Some(Key::Escape), "escape after the timeout");
    assert_eq!(serial_keys(&mut d, b"a"), [Key::Char(b'a')], "after escape");
    // ESC ESC: the first is Escape. ESC plus Enter: Escape, then Enter.
    assert_eq!(serial_keys(&mut d, b"\x1b\x1b["), [Key::Escape], "double escape");
    assert_eq!(serial_keys(&mut d, b"B"), [Key::Down], "sequence after double escape");
    assert_eq!(serial_keys(&mut d, b"\x1b\r"), [Key::Escape, Key::Enter], "escape then control key");
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
