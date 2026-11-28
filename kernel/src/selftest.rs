//! Boot-time checks of the CPU setup. They run on every boot, in QEMU and on real hardware.
//! A failing check panics.

use crate::input::{self, Key, SerialDecoder};
use crate::paint;
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
    check("mouse packets and events", mouse_events);
    check("pictures", pictures);
    check("picture viewers", viewers);
    check("shell tokenizer", shell_tokenizer);
    check("shell commands", shell_commands);
    check("shell options", shell_options);
    check("ACPI power-off info", acpi_power_info);
    check("editor keys", editor_keys);
    check("hex editor keys", hex_editor_keys);
    check("paint tools and keys", paint::selftest::tools);
    check("paint panel layout", paint::selftest::panel_layout);
    check("paint mouse", paint::selftest::mouse_use);
    check("paint redraws only what changed", paint::selftest::redraw_spans);
    check("paint zoom levels", paint::selftest::zoom_levels);
    check("paint zoom mapping", paint::selftest::zoom_mapping);
    check("paint reduced views average", paint::selftest::reduced_view_average);
    check("paint zoom keys and panning", paint::selftest::zoom_keys_and_pan);
    #[cfg(feature = "qemu-test")]
    check("paint files", paint::selftest::files);
    #[cfg(feature = "qemu-test")]
    check("paint a big canvas", paint::selftest::big_canvas);
    #[cfg(feature = "qemu-test")]
    check("paint a 4096x4096 canvas", paint::selftest::zoom_4096);
    check("assembly programs", programs);
    #[cfg(feature = "qemu-test")]
    check("crashing program", program_crash);
    check("tasks are preempted", tasks_preempted);
    check("each task has its own screen", task_screens);
    check("keys go to a task's own queue", task_input);
    check("programs run at the same time", programs_concurrent);
    check("closing a task stops its program", close_busy_program);
    check("desktop home screen keys", desktop_home);
    check("moving desktop icons", icon_layout);
    check("desktop mouse clicks and drags", desktop_mouse);
    check("taskbar click zones", taskbar_zones);
    check("Alt+Tab switcher", switcher);
    check("desktop commands outside the desktop", desktop_commands);
    check("screens change size", screen_resize);
    check("text drawn as pixels", glyph_drawing);
    check("display mode names", display_names);
    check("themes", theme_settings);
    #[cfg(feature = "qemu-test")]
    check("graphics modes", display_modes);
    #[cfg(feature = "qemu-test")]
    check("pixels and the mouse pointer", pixel_surface);
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
    if !program_ram() {
        return;
    }
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

    // /desktop: Terminal, Files and Paint first, then folders, then files by name.
    let icons = icons_for(DESKTOP_DIR, entries());
    let names: Vec<&str> = icons.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["Terminal", "Files", "Paint", "games", "a.raw", "hello.vex", "notes.txt"]);
    assert_eq!(icons[0].kind, Kind::Terminal);
    assert_eq!(icons[2].kind, Kind::Paint);
    assert_eq!(icons[3].kind, Kind::File(FileType::Dir));
    assert_eq!(icons_for("/x", Vec::new())[0].kind, Kind::Up, "other folders start with ..");
    assert!(icons_for("/", Vec::new()).is_empty(), "the root has no parent");

    let mut home = Home::new();
    home.set_folder(DESKTOP_DIR, icons);
    assert_eq!(home.handle(Key::Enter), Action::NewTerminal);
    assert_eq!(home.handle(Key::Right), Action::None);
    assert_eq!(home.handle(Key::Enter), Action::Enter(String::from("/")), "Files shows the disk");
    // Enter on Paint opens a blank canvas, with no message; P on it does the same.
    home.handle(Key::Right);
    assert_eq!(home.selected().unwrap().name, "Paint");
    assert_eq!(home.handle(Key::Enter), Action::Paint(None));
    assert_eq!(home.handle(Key::Char(b'p')), Action::Paint(None));
    assert!(home.message.is_empty(), "{}", home.message);
    assert_eq!(home.handle(Key::Char(b'e')), Action::None, "Paint is not edited");
    // 5 icons per row: Down from the 2nd icon reaches the 7th (notes.txt), Up goes back.
    home.handle(Key::Left);
    home.handle(Key::Down);
    assert_eq!(home.selected().unwrap().name, "notes.txt");
    assert_eq!(home.handle(Key::Enter), Action::Open(String::from("/desktop/notes.txt")));
    assert_eq!(home.handle(Key::Char(b'e')), Action::Edit(String::from("/desktop/notes.txt")));
    // P on anything that is not a picture opens a blank canvas and says so.
    assert_eq!(home.handle(Key::Char(b'p')), Action::Paint(None));
    assert_eq!(home.message, "Not an image: opening a blank canvas");
    home.handle(Key::Up);
    assert_eq!(home.selected().unwrap().name, "Files");
    home.handle(Key::Left);
    assert_eq!(home.selected().unwrap().name, "Terminal");
    home.handle(Key::Right);
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

    // P on a picture opens paint on it; the .bmp icon is an IMG.
    home.set_folder("/desktop/pics", icons_for("/desktop/pics", vec![entry("x.asm", false), entry("art.bmp", false)]));
    home.handle(Key::End);
    assert_eq!(home.selected().unwrap().name, "x.asm");
    home.handle(Key::Left);
    assert_eq!(home.selected().unwrap().kind, Kind::File(FileType::Image));
    assert_eq!(home.handle(Key::Char(b'P')), Action::Paint(Some(String::from("/desktop/pics/art.bmp"))));
    assert!(home.message.is_empty(), "{}", home.message);

    // Leaving: Ctrl-Q, then the answer to the question.
    assert_eq!(home.handle(Key::Ctrl(b'q')), Action::Quit);
    home.ask_quit(2);
    assert!(home.message.contains("2 open programs"), "{}", home.message);
    assert_eq!(home.handle(Key::Char(b'n')), Action::None);
    assert!(home.message.is_empty());
    home.ask_quit(1);
    assert_eq!(home.handle(Key::Char(b'y')), Action::QuitNow);
}

fn icon_layout() {
    use crate::desktop::home::{icons_for, Action, Home, DESKTOP_DIR};
    use crate::desktop::layout::{self, Place};
    use kfs::{Extent, Info};
    let place = |id: &str, col, row| Place { id: String::from(id), col, row };
    // The layout file: folder, icon (quoted name or built-in), column, row.
    let text = "# comment\n\"/desktop\" \"my notes.txt\" 3 0\n\"/desktop\" :terminal 0 1\n\"/desktop\" :paint 2 2\nnonsense\n\"/x\" \"a\" 1\n";
    let entries = layout::parse(text);
    assert_eq!(
        entries,
        [
            (String::from("/desktop"), place("\"my notes.txt\"", 3, 0)),
            (String::from("/desktop"), place(":terminal", 0, 1)),
            (String::from("/desktop"), place(":paint", 2, 2)),
        ]
    );
    assert_eq!(layout::parse(&layout::format(&entries)), entries, "layout file round trip");

    let file = |name: &str| Info { name: String::from(name), is_dir: false, extent: Extent { start: 0, sectors: 0, size: 0 } };
    let mut home = Home::new();
    home.set_folder(DESKTOP_DIR, icons_for(DESKTOP_DIR, vec![file("a.txt"), file("b.txt"), file("c.txt")]));
    assert_eq!(home.place_of("Paint"), Some((2, 0)), "Paint comes after Terminal and Files");
    assert_eq!(home.place_of("c.txt"), Some((0, 1)), "in order at first");
    // Saved places win; the other icons (Paint too, as in a folder saved before it existed) take
    // the free cells in order.
    home.set_places(vec![place("\"c.txt\"", 0, 2), place(":terminal", 2, 0)]);
    let at = |home: &Home, name: &str| home.place_of(name).unwrap();
    assert_eq!(
        [at(&home, "Files"), at(&home, "Paint"), at(&home, "a.txt"), at(&home, "Terminal"), at(&home, "b.txt"), at(&home, "c.txt")],
        [(0, 0), (1, 0), (3, 0), (2, 0), (4, 0), (0, 2)]
    );
    // Home/End go by reading order; Up finds the nearest row with icons.
    home.handle(Key::Home);
    assert_eq!(home.selected().unwrap().name, "Files");
    home.handle(Key::End);
    assert_eq!(home.selected().unwrap().name, "c.txt");
    home.handle(Key::Up);
    assert_eq!(home.selected().unwrap().name, "Files");

    // M moves the selected icon; one in the way takes its old place; Enter saves every place.
    home.handle(Key::End);
    assert_eq!(home.handle(Key::Char(b'm')), Action::None);
    for key in [Key::Right, Key::Up, Key::Up] {
        assert_eq!(home.handle(key), Action::None);
    }
    match home.handle(Key::Enter) {
        Action::SaveLayout(places) => {
            assert_eq!(places.len(), 6);
            assert!(places.contains(&place("\"c.txt\"", 1, 0)) && places.contains(&place(":paint", 1, 1)), "{places:?}");
        }
        other => panic!("Enter while moving gave {other:?}"),
    }
    // Esc puts the icons back; the edge of the grid stops a move.
    home.handle(Key::Char(b'm'));
    home.handle(Key::Left);
    assert_eq!(at(&home, "Files"), (1, 0), "swapped while moving");
    home.handle(Key::Escape);
    assert_eq!((at(&home, "c.txt"), at(&home, "Files")), ((1, 0), (0, 0)));
    home.handle(Key::Char(b'm'));
    for _ in 0..10 {
        home.handle(Key::Right);
    }
    assert_eq!(at(&home, "c.txt"), (4, 0));
    home.handle(Key::Escape);
    // A: back in order, with nothing saved for the folder.
    assert_eq!(home.handle(Key::Char(b'a')), Action::SaveLayout(Vec::new()));
    assert_eq!((at(&home, "Terminal"), at(&home, "c.txt")), ((0, 0), (0, 1)));
    // A place beyond the screen's width waits for a wider screen.
    home.set_places(vec![place("\"c.txt\"", 9, 0)]);
    assert_eq!(at(&home, "c.txt"), (0, 1));
    home.set_screen(160, 24);
    assert_eq!(at(&home, "c.txt"), (9, 0));
    home.set_screen(80, 24);
    assert_eq!(at(&home, "c.txt"), (0, 1));
}

fn desktop_mouse() {
    use crate::desktop::home::{icons_for, Action, Home, DESKTOP_DIR};
    use crate::desktop::layout::Place;
    use crate::mouse::{self, Button, Event};
    use kfs::{Extent, Info};
    let file = |name: &str| Info { name: String::from(name), is_dir: false, extent: Extent { start: 0, sectors: 0, size: 0 } };
    let mut home = Home::new();
    home.set_folder(DESKTOP_DIR, icons_for(DESKTOP_DIR, vec![file("a.txt"), file("b.txt"), file("c.txt")]));
    // 80x24: a 5x4 grid of 16x5 cells from row 2. Terminal, Files, Paint, a.txt, b.txt in grid
    // row 0 and c.txt at the start of grid row 1.
    let name = |home: &Home, row, col| home.icon_at(row, col).map(|i| home.icons()[i].name.clone());
    assert_eq!(name(&home, 2, 0).as_deref(), Some("Terminal"), "top left of the first cell");
    assert_eq!(name(&home, 6, 15).as_deref(), Some("Terminal"), "bottom right of the first cell");
    assert_eq!(name(&home, 4, 16).as_deref(), Some("Files"));
    assert_eq!(name(&home, 3, 79).as_deref(), Some("b.txt"), "the last column of the last cell");
    assert_eq!(name(&home, 7, 0).as_deref(), Some("c.txt"), "the second grid row");
    assert_eq!(name(&home, 7, 16), None, "an empty grid cell");
    assert_eq!(name(&home, 12, 0), None, "an empty grid row");
    assert_eq!(name(&home, 21, 79), None, "the last grid cell is empty");
    for row in [0, 1, 22, 23, 24] {
        assert_eq!(name(&home, row, 5), None, "header, message and hint rows (row {row})");
    }
    assert_eq!(name(&home, 2, 80), None, "right of the grid");
    assert_eq!(home.grid_cell_at(8, 33), Some((2, 1)));

    // Events in text mode: 8x16 pixels per cell; a point inside the cell (row, col).
    let cell_px = (8, 16);
    let point = |row: usize, col: usize| ((col * 8 + 3) as u16, (row * 16 + 5) as u16);
    let event = |action, (x, y): (u16, u16), buttons| Event { action, x, y, dx: 0, dy: 0, buttons };
    let down = |at| event(mouse::Action::Down(Button::Left), at, 1);
    let up = |at| event(mouse::Action::Up(Button::Left), at, 0);
    let drag = |at| event(mouse::Action::Move, at, 1);
    let selected = |home: &Home| home.selected().unwrap().name.clone();
    let place = |id: &str, col, row| Place { id: String::from(id), col, row };

    // A click chooses the icon (nothing else happens, so nothing is saved or opened) and shows
    // the hint; a click on empty space, the header or the hint rows changes nothing.
    assert_eq!(home.mouse(down(point(3, 20)), cell_px, 1000), Action::None);
    assert_eq!(home.mouse(up(point(3, 20)), cell_px, 1001), Action::None);
    assert_eq!(selected(&home), "Files");
    assert_eq!(home.message, "Double-click opens it, drag moves it");
    home.handle(Key::Right);
    assert_eq!(selected(&home), "Paint");
    assert!(home.message.is_empty(), "the next key clears the hint");
    for (row, col) in [(7, 20), (0, 5), (1, 5), (22, 5), (23, 5), (12, 40)] {
        assert_eq!(home.mouse(down(point(row, col)), cell_px, 1100), Action::None);
        assert_eq!(home.mouse(up(point(row, col)), cell_px, 1101), Action::None);
        assert_eq!((selected(&home), home.message.as_str()), (String::from("Paint"), ""), "click at {row},{col}");
    }
    // The other buttons do nothing.
    for button in [Button::Right, Button::Middle] {
        let at = point(3, 3);
        assert_eq!(home.mouse(event(mouse::Action::Down(button), at, button.bit()), cell_px, 1200), Action::None);
        assert_eq!(home.mouse(event(mouse::Action::Up(button), at, 0), cell_px, 1201), Action::None);
        assert_eq!(selected(&home), "Paint");
    }
    // A mouse event given to `handle` is not a key: nothing changes.
    home.message = String::from("x");
    assert_eq!(home.handle(Key::Mouse(down(point(3, 3)))), Action::None);
    assert_eq!(home.message, "x");

    // Double-click: a second press on the same icon within 40 ticks opens it like Enter.
    assert_eq!(home.mouse(down(point(3, 3)), cell_px, 2000), Action::None);
    home.mouse(up(point(3, 3)), cell_px, 2005);
    assert_eq!(home.mouse(down(point(5, 9)), cell_px, 2040), Action::NewTerminal, "within 40 ticks, anywhere in the cell");
    assert!(home.message.is_empty());
    home.mouse(up(point(5, 9)), cell_px, 2041);
    // A third quick click is a single click again.
    assert_eq!(home.mouse(down(point(5, 9)), cell_px, 2042), Action::None);
    assert_eq!(home.message, "Double-click opens it, drag moves it");
    home.mouse(up(point(5, 9)), cell_px, 2043);
    // 1 s apart, or on another icon, it only chooses.
    assert_eq!(home.mouse(down(point(3, 35)), cell_px, 3000), Action::None);
    home.mouse(up(point(3, 35)), cell_px, 3001);
    assert_eq!(home.mouse(down(point(3, 35)), cell_px, 3100), Action::None);
    home.mouse(up(point(3, 35)), cell_px, 3101);
    assert_eq!(home.mouse(down(point(3, 51)), cell_px, 3102), Action::None);
    home.mouse(up(point(3, 51)), cell_px, 3103);
    assert_eq!(selected(&home), "a.txt");
    // Files, Paint and a file open like Enter does.
    home.mouse(down(point(3, 20)), cell_px, 4000);
    home.mouse(up(point(3, 20)), cell_px, 4001);
    assert_eq!(home.mouse(down(point(3, 20)), cell_px, 4010), Action::Enter(String::from("/")));
    home.mouse(up(point(3, 20)), cell_px, 4011);
    home.mouse(down(point(3, 36)), cell_px, 5000);
    home.mouse(up(point(3, 36)), cell_px, 5001);
    assert_eq!(home.mouse(down(point(3, 36)), cell_px, 5010), Action::Paint(None));
    home.mouse(up(point(3, 36)), cell_px, 5011);
    home.mouse(down(point(3, 68)), cell_px, 6000);
    home.mouse(up(point(3, 68)), cell_px, 6001);
    assert_eq!(home.mouse(down(point(3, 68)), cell_px, 6010), Action::Open(String::from("/desktop/b.txt")));
    home.mouse(up(point(3, 68)), cell_px, 6011);

    // Drag: less than 4 pixels does not pick the icon up (and letting go saves nothing).
    let files = point(3, 20);
    let moved = |dx: u16, dy: u16| (files.0 + dx, files.1 + dy);
    assert_eq!(home.mouse(down(files), cell_px, 7000), Action::None);
    assert_eq!(home.mouse(drag(moved(3, 0)), cell_px, 7001), Action::None);
    assert_eq!(home.mouse(drag(moved(2, 2)), cell_px, 7001), Action::None, "2,2 is under 4 pixels");
    assert_eq!(home.message, "Double-click opens it, drag moves it");
    assert_eq!(home.mouse(up(moved(3, 0)), cell_px, 7002), Action::None);
    assert_eq!(home.place_of("Files"), Some((1, 0)));
    // 4 pixels and more picks it up, like M; it follows the pointer cell by cell...
    assert_eq!(home.mouse(down(files), cell_px, 8000), Action::None);
    home.mouse(drag(moved(4, 0)), cell_px, 8001);
    assert_eq!(home.message, "Moving Files");
    assert_eq!(home.place_of("Files"), Some((1, 0)), "still over its own cell");
    home.mouse(drag(point(8, 50)), cell_px, 8002);
    assert_eq!(home.place_of("Files"), Some((3, 1)), "an empty cell: the icon is shown there");
    assert_eq!(home.place_of("a.txt"), Some((3, 0)));
    // ...over an occupied cell the other icon is shown at the origin...
    home.mouse(drag(point(3, 68)), cell_px, 8003);
    assert_eq!((home.place_of("Files"), home.place_of("b.txt")), (Some((4, 0)), Some((1, 0))));
    // ...off the grid it is back at its origin...
    home.mouse(drag(point(23, 52)), cell_px, 8004);
    assert_eq!((home.place_of("Files"), home.place_of("b.txt")), (Some((1, 0)), Some((4, 0))));
    // ...and letting go puts it down and saves every place, like M.
    home.mouse(drag(point(8, 50)), cell_px, 8005);
    match home.mouse(up(point(8, 50)), cell_px, 8006) {
        Action::SaveLayout(saved) => {
            assert_eq!(saved.len(), 6);
            assert!(saved.contains(&place(":files", 3, 1)), "{saved:?}");
            assert!(saved.contains(&place(":terminal", 0, 0)), "{saved:?}");
        }
        other => panic!("letting go of a dragged icon gave {other:?}"),
    }
    assert!(home.message.is_empty(), "{}", home.message);
    assert_eq!(selected(&home), "Files");
    // Moves after the release do nothing.
    home.mouse(drag(point(3, 3)), cell_px, 8007);
    assert_eq!(home.place_of("Files"), Some((3, 1)));
    // Dropped on an occupied cell: the two swap.
    home.mouse(down(point(3, 36)), cell_px, 9000);
    home.mouse(drag(point(3, 68)), cell_px, 9001);
    let swapped = home.mouse(up(point(3, 68)), cell_px, 9002);
    assert!(matches!(swapped, Action::SaveLayout(_)), "{swapped:?}");
    assert_eq!((home.place_of("Paint"), home.place_of("b.txt")), (Some((4, 0)), Some((2, 0))));
    assert_eq!(home.places().len(), 6);
    // Let go off the grid: everything is put back and nothing is saved.
    home.mouse(down(point(3, 3)), cell_px, 10000);
    home.mouse(drag(point(12, 20)), cell_px, 10001);
    assert_eq!(home.place_of("Terminal"), Some((1, 2)));
    home.mouse(drag(point(22, 20)), cell_px, 10002);
    assert_eq!(home.mouse(up(point(22, 20)), cell_px, 10003), Action::None);
    assert_eq!(home.place_of("Terminal"), Some((0, 0)));
    assert_eq!(home.mouse(down(point(3, 3)), cell_px, 11000), Action::None, "not a double-click after a drag");
    home.mouse(up(point(3, 3)), cell_px, 11001);
    // Esc while dragging puts it back too, and the rest of the press does nothing.
    home.mouse(down(point(3, 3)), cell_px, 12000);
    home.mouse(drag(point(8, 20)), cell_px, 12001);
    assert_eq!(home.place_of("Terminal"), Some((1, 1)));
    assert_eq!(home.handle(Key::Right), Action::None, "an icon being dragged takes no other key");
    assert_eq!(home.place_of("Terminal"), Some((1, 1)));
    assert_eq!(home.handle(Key::Escape), Action::None);
    assert_eq!(home.place_of("Terminal"), Some((0, 0)));
    home.mouse(drag(point(8, 40)), cell_px, 12002);
    assert_eq!(home.place_of("Terminal"), Some((0, 0)));
    assert_eq!(home.mouse(up(point(8, 40)), cell_px, 12003), Action::None);
    // A right press during a drag is ignored.
    home.mouse(down(point(3, 3)), cell_px, 13000);
    home.mouse(drag(point(8, 20)), cell_px, 13001);
    assert_eq!(home.mouse(event(mouse::Action::Down(Button::Right), point(8, 20), 3), cell_px, 13002), Action::None);
    assert_eq!(home.place_of("Terminal"), Some((1, 1)));
    assert!(matches!(home.mouse(up(point(8, 20)), cell_px, 13003), Action::SaveLayout(_)));
    // Only the cells that are shown take part: the last grid row ends at row 21.
    assert_eq!(home.grid_cell_at(21, 0), Some((0, 3)));
    assert_eq!(home.grid_cell_at(22, 0), None);
}

fn taskbar_zones() {
    use crate::desktop::{bar_layout, bar_slot_at, BarZone};
    // Home (slot 0), and tasks 3 and 5, in a 80-column bar with a 17-column clock.
    let entries = [(0, 6), (3, 9), (5, 10)];
    let layout = bar_layout(&entries, 80 - 17);
    let zone = |start, end, slot| BarZone { start, end, slot };
    assert_eq!(layout.zones, [zone(0, 6, 0), zone(6, 15, 3), zone(15, 25, 5)]);
    assert_eq!((layout.overflow, layout.used), (None, 25));
    // Every column of an entry, its padding spaces included, is its zone; the gap and the clock
    // are nobody's.
    assert_eq!(bar_slot_at(&layout.zones, 0), Some(0), "the space before Home");
    assert_eq!(bar_slot_at(&layout.zones, 5), Some(0), "the space after Home");
    assert_eq!(bar_slot_at(&layout.zones, 6), Some(3));
    assert_eq!(bar_slot_at(&layout.zones, 14), Some(3));
    assert_eq!(bar_slot_at(&layout.zones, 15), Some(5));
    assert_eq!(bar_slot_at(&layout.zones, 24), Some(5));
    assert_eq!(bar_slot_at(&layout.zones, 25), None, "the gap");
    assert_eq!(bar_slot_at(&layout.zones, 62), None);
    assert_eq!(bar_slot_at(&layout.zones, 70), None, "the clock");
    // Too many tasks: what does not fit is replaced by " ...", which is nobody's either.
    let entries = [(0, 6), (3, 9), (5, 10), (7, 10)];
    let layout = bar_layout(&entries, 20);
    assert_eq!(layout.zones, [zone(0, 6, 0), zone(6, 15, 3)]);
    assert_eq!((layout.overflow, layout.used), (Some(15), 19));
    for col in 15..19 {
        assert_eq!(bar_slot_at(&layout.zones, col), None, "overflow column {col}");
    }
    // The last entry is always shown.
    let layout = bar_layout(&entries[..3], 20);
    assert_eq!((layout.zones.len(), layout.overflow), (3, None));
    assert_eq!(bar_slot_at(&layout.zones, 20), Some(5));
    assert_eq!(bar_layout(&[], 20).zones, []);

    // What a click does (Home is slot 0, task 3 is in front or not). Home always ends on
    // /desktop: from a task it goes there and shows it; with the home screen in front on a
    // folder it goes up to /desktop; on /desktop itself there is nothing to do. A task's entry
    // switches to it unless it is in front already.
    use crate::desktop::{bar_action, BarAction};
    let zones = bar_layout(&[(0, 6), (3, 9), (5, 10)], 80 - 17).zones;
    assert_eq!(bar_action(&zones, 2, 3, 0, "/"), BarAction::Home, "Home from a task, a folder grid left open");
    assert_eq!(bar_action(&zones, 2, 3, 0, "/pictures"), BarAction::Home);
    assert_eq!(bar_action(&zones, 2, 0, 0, "/"), BarAction::Home, "Home in front on a folder");
    assert_eq!(bar_action(&zones, 5, 0, 0, "/pictures"), BarAction::Home, "the space after Home");
    assert_eq!(bar_action(&zones, 2, 3, 0, "/desktop"), BarAction::Switch(0), "Home from a task, already /desktop");
    assert_eq!(bar_action(&zones, 2, 0, 0, "/desktop"), BarAction::Nothing, "Home in front on /desktop");
    assert_eq!(bar_action(&zones, 8, 0, 0, "/"), BarAction::Switch(3));
    assert_eq!(bar_action(&zones, 8, 3, 0, "/"), BarAction::Nothing, "the task in front");
    assert_eq!(bar_action(&zones, 30, 0, 0, "/"), BarAction::Nothing, "the gap");
    assert_eq!(bar_action(&zones, 70, 3, 0, "/"), BarAction::Nothing, "the clock");
}

fn screen_resize() {
    use crate::vga::{Screen, DEFAULT_ATTR};
    use core::fmt::Write;
    let text = |s: &Screen, row: usize| String::from(String::from_utf8_lossy(&s.row_bytes(row)).trim_end());
    let mut s = Screen::new(80, 25, DEFAULT_ATTR);
    for i in 0..30 {
        writeln!(s, "line {i}").unwrap();
    }
    assert_eq!((text(&s, 0), text(&s, 23), s.position()), (String::from("line 6"), String::from("line 29"), (24, 0)));
    // Larger: the text stays at the top left.
    s.resize(120, 40, 40);
    assert_eq!((s.width(), s.rows(), text(&s, 0), s.position()), (120, 40, String::from("line 6"), (24, 0)));
    // Smaller: the top rows go, so the output position stays on the screen.
    s.resize(40, 10, 10);
    assert_eq!((text(&s, 0), text(&s, 8), s.position()), (String::from("line 21"), String::from("line 29"), (9, 0)));
    // Under the desktop: one row less of the same height.
    s.resize(40, 10, 9);
    assert_eq!((s.rows(), text(&s, 0)), (9, String::from("line 21")));
    let mut wide = Screen::new(80, 5, DEFAULT_ATTR);
    write!(wide, "{}", "x".repeat(70)).unwrap();
    wide.resize(50, 5, 5);
    assert_eq!(text(&wide, 0).len(), 50, "a narrower screen cuts rows off");
    assert_eq!(crate::hexedit::bytes_per_row(80), 16);
    assert_eq!(crate::hexedit::bytes_per_row(60), 8);
    assert_eq!(crate::hexedit::bytes_per_row(240), 32);
    assert_eq!(crate::hexedit::bytes_per_row(300), 64);
}

fn glyph_drawing() {
    use crate::display::{self, CursorShape, Fb, DEFAULT_PALETTE};
    use crate::vbe::PixelFormat;
    use crate::vga::{attr, cell};
    let rgb565 = PixelFormat { red: (5, 11), green: (6, 5), blue: (5, 0) };
    assert_eq!(PixelFormat::RGB32.pixel([0x12, 0x34, 0x56]), 0x123456);
    assert_eq!((rgb565.pixel([0xFF, 0xFF, 0xFF]), rgb565.pixel([0xFF, 0, 0])), (0xFFFF, 0xF800));
    assert_eq!(display::grid(1920, 1080, 100), Ok((240, 67)));
    assert_eq!(display::grid(1920, 1080, 200), Ok((120, 33)));
    assert_eq!(display::grid(1366, 768, 125), Ok((136, 38)));
    assert!(display::grid(640, 480, 400).is_err(), "too few characters");
    assert!(display::grid(7680, 4320, 100).is_err(), "too many characters");
    assert!(display::grid(1920, 1080, 90).is_err(), "scale below 100%");

    let Some(font) = display::font() else {
        println!("  (skipped drawing: the video BIOS gave no font)");
        return;
    };
    let (white, blue) = (0xFFFFFF, 0x0000AA);
    let a = cell(b'A', attr(Color::White, Color::Blue));
    for scale in [100, 150, 200, 275] {
        let (cw, ch) = display::cell_size(scale);
        let (w, h) = (cw * 3, ch * 2);
        let mut buffer = vec![0u32; w * h];
        let fb = Fb::new(buffer.as_mut_ptr() as usize, w * 4, 32, w as u32, h as u32, PixelFormat::RGB32, (w as u32, h as u32), scale, &DEFAULT_PALETTE);
        fb.draw_cell(&font, 1, 2, a, None);
        let (x0, y0) = fb.cell_origin(1, 2);
        for y in 0..ch {
            for x in 0..cw {
                let on = font[b'A' as usize * 16 + y * 16 / ch] & (0x80 >> (x * 8 / cw)) != 0;
                assert_eq!(fb.pixel(x0 + x, y0 + y), if on { white } else { blue }, "scale {scale}%, pixel {x},{y}");
            }
        }
        assert_eq!(fb.pixel(0, 0), 0, "other cells are left alone");
        fb.draw_cell(&font, 0, 0, a, Some(CursorShape::Underline));
        assert_eq!(fb.pixel(cw / 2, ch - 1), white, "underline cursor");
        fb.draw_cell(&font, 0, 1, cell(b' ', attr(Color::White, Color::Blue)), Some(CursorShape::Block));
        assert_eq!(fb.pixel(cw + 1, 1), white, "block cursor");
        drop(buffer);
    }
}

fn display_names() {
    use crate::display::{config_text, parse_config, parse_mode, Request};
    let graphics = |width, height, scale| Request::Graphics { width, height, scale };
    assert_eq!(parse_mode("text", 150), Ok(Request::Text { rows50: false }));
    assert_eq!(parse_mode("TEXT50", 150), Ok(Request::Text { rows50: true }));
    assert_eq!(parse_mode("1920x1080", 150), Ok(graphics(1920, 1080, 150)));
    assert_eq!(parse_mode("1080p", 100), Ok(graphics(1920, 1080, 100)));
    assert_eq!(parse_mode("uw1440", 100), Ok(graphics(3440, 1440, 100)));
    assert_eq!(parse_mode("2560X1080", 125), Ok(graphics(2560, 1080, 125)));
    for bad in ["100x100", "big", "1920x", "x1080", "20000x1000"] {
        assert!(parse_mode(bad, 100).is_err(), "{bad} accepted");
    }
    for request in [Request::Text { rows50: false }, Request::Text { rows50: true }, graphics(1366, 768, 175)] {
        assert_eq!(parse_config(&config_text(request)), Ok(request), "display.cfg round trip");
    }
    assert_eq!(parse_config("# x\nmode 1366x768\nscale 125%\n"), Ok(graphics(1366, 768, 125)));
    assert!(parse_config("# no mode\n").is_err());
    assert!(parse_config("mode text\ncolor red\n").is_err());
}

fn theme_settings() {
    use crate::theme::{self, Item, Theme, PRESETS};
    use crate::vga::{attr, DEFAULT_ATTR};
    use Color::*;
    let mut t = Theme::CLASSIC;
    assert_eq!(t.attr(Item::Shell), DEFAULT_ATTR);
    assert_eq!(t.attr(Item::IconDir), attr(Yellow, Blue), "icons are on the desktop's background");
    t.set("desktop", &["black", "on", "white"]).unwrap();
    assert_eq!(t.attr(Item::IconDir), attr(Yellow, White));
    t.set("icon.dir", &["brown"]).unwrap();
    assert_eq!(t.attr(Item::IconDir), attr(Brown, White));
    t.set("desktop.header", &["red"]).unwrap();
    assert_eq!(t.attr(Item::DesktopHeader), attr(Red, Cyan), "one color keeps the background");
    t.set("palette.blue", &["#1e3a8a"]).unwrap();
    assert_eq!(t.palette[1], [0x1e, 0x3a, 0x8a]);
    for (name, value) in [("clock", "12h"), ("clock.seconds", "on"), ("cursor", "block"), ("Taskbar", "white")] {
        t.set(name, &[value]).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    for (name, value) in [
        ("nothing", &["x"][..]),
        ("shell", &["purple"]),
        ("icon.dir", &["red", "on", "blue"]),
        ("palette.red", &["#12345"]),
        ("palette.mauve", &["#123456"]),
        ("clock", &["13h"]),
        ("cursor", &[]),
    ] {
        assert!(t.set(name, value).is_err(), "{name} {value:?} accepted");
    }
    let (back, errors) = Theme::parse(&t.to_config());
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(back, t, "theme.cfg round trip");
    let (partial, errors) = Theme::parse("# comment\nshell white on black\nbogus 1\n");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(partial.attr(Item::Shell), attr(White, Black));
    assert_eq!(partial.attr(Item::Desktop), Theme::CLASSIC.attr(Item::Desktop), "the rest stays classic");
    for (name, _) in PRESETS {
        assert!(theme::preset(name).is_some(), "preset {name}");
    }
    assert!(theme::preset("dark") != Some(Theme::CLASSIC));
    // The picture and Paint icons: every preset sets them; Paint starts as Terminal's color.
    for (name, image) in [("classic", "pink"), ("dark", "pink"), ("light", "magenta"), ("amber", "yellow")] {
        let preset = theme::preset(name).unwrap();
        assert_eq!(preset.value("icon.image").as_deref(), Some(image), "{name} icon.image");
        assert_eq!(preset.value("icon.paint"), preset.value("icon.terminal"), "{name} icon.paint");
        assert_eq!(preset.attr(Item::IconImage) >> 4, preset.attr(Item::Desktop) >> 4, "{name}: on the desktop's background");
    }

    let now = DateTime { year: 2026, month: 11, day: 25, hour: 14, minute: 5, second: 9 };
    assert_eq!(crate::desktop::clock_text(&Theme::CLASSIC, now), " Wed 25 Nov 2026  14:05 ");
    assert_eq!(crate::desktop::clock_text(&t, now), " Wed 25 Nov 2026  2:05:09 PM ");

    // The command, without saving: the shell's colors change on the screen at once.
    let saved = theme::current();
    let mut sh = Shell::new();
    let mut run = |line: &str| -> Result<String, String> {
        let mut out = String::new();
        match sh.execute(line, &mut out) {
            Ok(()) => Ok(out),
            Err(CmdError::Usage) => Err(String::from("usage")),
            Err(CmdError::Message(m)) => Err(m),
        }
    };
    let before = console::attr();
    let out = run("theme -n shell yellow on blue").expect("theme -n");
    assert!(out.contains("not saved"), "{out}");
    assert_eq!(theme::attr(Item::Shell), attr(Yellow, Blue));
    if before == saved.attr(Item::Shell) {
        assert_eq!(console::attr(), attr(Yellow, Blue), "the shell took the new colors");
    }
    assert_eq!(run("theme shell").unwrap(), "shell yellow on blue\n");
    assert!(run("theme -n preset nosuch").unwrap_err().contains("no preset"));
    assert!(run("theme -n desktop.frame red on blue").is_err());
    let list = run("theme").unwrap();
    assert!(list.contains("icon.vex") && list.contains("Ready-made looks") && list.contains("lightgreen"), "{list}");
    theme::apply(saved);
    assert_eq!(console::attr(), before, "colors back");
}

/// Switches the display through the `display` command, then back to text mode.
#[cfg(feature = "qemu-test")]
fn display_modes() {
    use crate::display::{self, How};
    use crate::vga::attr;
    let (modes, version, memory) = display::vbe_modes();
    assert!(version >= 0x200, "VBE version {version:#x}");
    assert!(modes.iter().any(|m| (m.width, m.height, m.bpp) == (1024, 768, 32)), "no 1024x768x32 mode");
    assert!(memory >= 16 << 20, "video memory {memory}");
    let mut sh = Shell::new();
    let mut try_run = |line: &str| -> Result<String, String> {
        let mut out = String::new();
        match sh.execute(line, &mut out) {
            Ok(()) => Ok(out),
            Err(CmdError::Usage) => Err(String::from("usage")),
            Err(CmdError::Message(m)) => Err(m),
        }
    };
    let saved = display::saved();
    let boot_row = console::row_text(task::BOOT, 0);

    // A mode the video BIOS has.
    let out = try_run("display -f -n 1024x768 -s 100").unwrap();
    assert!(out.contains("128x48 characters") && out.contains("VESA mode"), "{out}");
    assert_eq!((display::size(), console::cols(), console::rows()), ((128, 48), 128, 48));
    assert_eq!(console::row_text(task::BOOT, 0), boot_row, "the screen kept its text");
    display::put(0, 0, crate::vga::cell(0xDB, attr(Color::White, Color::Red)));
    assert_eq!(display::pixel_in_cell(0, 0, 3, 8), display::color_pixel(Color::White as usize), "a full block is drawn");
    // A size it does not have: set directly (QEMU's Bochs display interface).
    let out = try_run("display -f -n 1000x700 -s 150").unwrap();
    assert!(out.contains("83x29 characters") && out.contains("set directly"), "{out}");
    assert_eq!(display::mode().how, How::Direct);
    // A width that is not a multiple of 8: framed in a mode a little wider; the scale is kept.
    let out = try_run("display -f -n 1001x700").unwrap();
    assert!(out.contains("framed in 1008x700") && out.contains("83x29"), "{out}");
    try_run("display -f -n -s 200").unwrap();
    assert_eq!(display::size(), (62, 21));
    // Errors leave the mode as it is.
    for bad in ["display -f -n 640x480 -s 400", "display -f -n 20000x100", "display -f -n -s 50"] {
        assert!(try_run(bad).is_err(), "{bad} worked");
    }
    assert_eq!(display::size(), (62, 21));
    // New tasks get screens of the new size.
    let slot = task::spawn("selftest size", || crate::println!("{}x{}", console::cols(), console::rows())).expect("spawn");
    task::join(slot);
    assert_eq!(console::row_text(slot, 0).as_deref(), Some("62x21"));
    task::reap();
    // Back to text: 80x50, then 80x25, which shows the boot screen in VGA memory again.
    try_run("display -f -n text50").unwrap();
    assert_eq!(display::size(), (80, 50));
    try_run("display -f -n text").unwrap();
    assert_eq!(display::size(), (80, 25));
    let vga_row: String = (0..80).map(|col| unsafe { core::ptr::read_volatile((0xB8000 as *const u16).add(col)) } as u8 as char).collect();
    assert_eq!(Some(String::from(vga_row.trim_end())), console::row_text(task::BOOT, 0), "VGA text memory");
    assert_eq!(display::saved(), saved, "-n saved the mode");
}

/// Loading pictures (read only, so it runs on real hardware too; skipped on a disk without the
/// samples) and the memory checks.
fn pictures() {
    use crate::picture::{self, Error};
    assert!(!picture::fits(usize::MAX) && picture::fits(4096));
    assert_eq!(picture::blank(5000, 2000, 0), Err(Error::Bmp(bmp::Error::TooLarge { width: 5000, height: 2000 })));
    assert_eq!(picture::blank(4097, 1, 0), Err(Error::Bmp(bmp::Error::TooLarge { width: 4097, height: 1 })));
    // The largest picture is refused for memory, never panics, wherever the heap is short.
    match picture::blank(4096, 4096, 0) {
        Ok(img) => assert_eq!(img.pixels.len(), 4096 * 4096),
        Err(e) => assert_eq!(e, Error::Memory(4096 * 4096 * 4)),
    }
    // Saving needs the file's sectors in one free run (a 4096x4096 picture: 48 MiB).
    let big = bmp::encoded_size(4096, 4096);
    assert_eq!(big, 54 + 4096 * 3 * 4096);
    assert_eq!(picture::room(big, big.div_ceil(512), false), Ok(()));
    assert_eq!(picture::room(big, big.div_ceil(512) - 1, false), Err(Error::Disk { needed: big, free: (big.div_ceil(512) - 1) * 512 }));
    assert_eq!(picture::room(513, 1, false), Err(Error::Disk { needed: 513, free: 512 }));
    assert_eq!(picture::room(512, 1, false), Ok(()));
    assert_eq!(picture::room(0, 0, false), Ok(()));
    assert_eq!(picture::room(4096, 0, true), Ok(()), "a file in the boot area is written in place");
    assert_eq!(
        alloc::format!("{}", Error::Disk { needed: 50 * 1024 * 1024, free: 10 * 1024 * 1024 }),
        "not enough free disk space (needs 51200 KiB in one piece; the largest free piece is 10240 KiB)"
    );
    assert_eq!(picture::blank(0, 5, 0).map(drop), Err(Error::Bmp(bmp::Error::TooLarge { width: 0, height: 5 })));
    assert_eq!(picture::blank(3, 2, 0xFFFFFF).map(|i| i.pixels.len()), Ok(6));
    assert_eq!(picture::load("/no such picture.bmp"), Err(Error::NotFound));
    assert_eq!(picture::load("/docs"), Err(Error::Folder));
    assert_eq!(picture::load("/docs/index.txt"), Err(Error::Bmp(bmp::Error::NotBmp)));
    match picture::load("/pictures/colors.bmp") {
        Err(Error::NotFound) => println!("  (no /pictures/colors.bmp on this disk: skipped)"),
        result => {
            let image = result.expect("colors.bmp");
            assert_eq!((image.width, image.height), (64, 40));
            assert_eq!(image.get(0, 0), Some(0x000000));
            assert_eq!(image.get(16, 0), Some(0x0000AA));
            assert_eq!(image.get(0, 10), Some(0xAA0000));
            assert_eq!(image.get(63, 39), Some(0xFFFFFF));
        }
    }
}

fn viewers() {
    use crate::display::DEFAULT_PALETTE;
    use crate::mouse::{Action, Event};
    use crate::vga::cell;
    use crate::viewer::{self, BarInfo, CellStyle, Placement, Scale, Viewer};

    // The half-block cells of a 2x2 picture: 0xDF with the upper pixel as foreground and the
    // lower as background; equal halves are a space on that color (blue 1, red 4, green 2).
    let mut img = bmp::Image::new(2, 2, 0);
    img.set(0, 0, 0x0000AA);
    img.set(1, 0, 0xAA0000);
    img.set(0, 1, 0x0000AA);
    img.set(1, 1, 0x00AA00);
    let style = CellStyle { palette: DEFAULT_PALETTE, bg: 0, blank: 0x07, dither: true };
    let (mut scratch, mut row) = (Vec::new(), Vec::new());
    let p = viewer::place((2, 2), (2, 2), Scale::Fit, false, (0, 0));
    assert_eq!(p, Placement { w: 2, h: 2, x: 0, y: 0, pan_x: 0, pan_y: 0 });
    viewer::cell_row(&img, &p, (2, 2), 0, &style, &mut scratch, &mut row);
    assert_eq!(row, [cell(b' ', 0x11), cell(0xDF, 0x24)], "same halves: a space on blue; red over green");
    // Not an exact palette color, without dither: the nearest one.
    let mut two = bmp::Image::new(1, 2, 0x000001);
    two.set(0, 0, 0xFFFFFF);
    let p = viewer::place((1, 2), (1, 2), Scale::Fit, false, (0, 0));
    let plain = CellStyle { palette: DEFAULT_PALETTE, bg: 0, blank: 0x07, dither: false };
    viewer::cell_row(&two, &p, (1, 2), 0, &plain, &mut scratch, &mut row);
    assert_eq!(row, [cell(0xDF, 0x0F)], "white over black");
    // Centered in a larger area: blank around it, and the blank color in the halves outside.
    let p = viewer::place((2, 2), (4, 4), Scale::Fit, false, (0, 0));
    assert_eq!((p.x, p.y), (1, 1));
    viewer::cell_row(&img, &p, (4, 4), 0, &style, &mut scratch, &mut row);
    assert_eq!(row, [cell(b' ', 0x07), cell(0xDF, 0x10), cell(0xDF, 0x40), cell(b' ', 0x07)]);
    viewer::cell_row(&img, &p, (4, 4), 1, &style, &mut scratch, &mut row);
    assert_eq!(row, [cell(b' ', 0x07), cell(0xDF, 0x01), cell(0xDF, 0x02), cell(b' ', 0x07)]);

    // Fit at 80x24 (80 cells by 2 * 23 pixel rows): shrunk with the ratio kept, never enlarged.
    let p = viewer::place((256, 128), (80, 46), Scale::Fit, false, (0, 0));
    assert_eq!((p.w, p.h, p.x, p.y), (80, 40, 0, 3));
    let p = viewer::place((64, 40), (80, 46), Scale::Fit, false, (0, 0));
    assert_eq!((p.w, p.h, p.x, p.y), (64, 40, 8, 3), "not enlarged");
    // gview: enlarged by a whole factor, 25% shrinks, a pan stays inside the picture.
    let p = viewer::place((64, 40), (1024, 752), Scale::Fit, true, (0, 0));
    assert_eq!((p.w, p.h, p.x, p.y), (1024, 640, 0, 56));
    let p = viewer::place((256, 128), (1024, 752), Scale::Percent(25), true, (0, 0));
    assert_eq!((p.w, p.h), (64, 32));
    let p = viewer::place((256, 128), (80, 46), Scale::Percent(100), false, (1000, 1000));
    assert_eq!((p.x, p.y, p.pan_x, p.pan_y), (0, 0, 176, 82));
    for (from, up, to) in [(100, true, 200), (800, true, 800), (100, false, 50), (25, false, 25), (30, true, 50), (30, false, 25), (150, false, 100)] {
        assert_eq!(viewer::zoom_step(from, up), to, "zoom {from} {up}");
    }

    // The drawing area is the task screen's rows above the status row in both views. 1024x768
    // (48 display rows): standalone the screen has 48 rows (47 above the bar), under the desktop
    // 47 (46 above the bar), and colors.bmp, 64x40 enlarged 16 times, is centered in it.
    assert_eq!(viewer::picture_area(47, 128, Some((8, 16))), (1024, 752));
    assert_eq!(viewer::picture_area(46, 128, Some((8, 16))), (1024, 736));
    let p = viewer::place((64, 40), viewer::picture_area(46, 128, Some((8, 16))), Scale::Fit, true, (0, 0));
    assert_eq!((p.w, p.h, p.x, p.y), (1024, 640, 0, 48), "under the desktop");
    let p = viewer::place((64, 40), viewer::picture_area(47, 128, Some((8, 16))), Scale::Fit, true, (0, 0));
    assert_eq!((p.w, p.h, p.x, p.y), (1024, 640, 0, 56), "standalone");
    // The character view: 80 cells by two picture pixel rows per text row (80x25: 24 rows alone,
    // 23 under the desktop's 24-row screens).
    assert_eq!(viewer::picture_area(24, 80, None), (80, 48));
    assert_eq!(viewer::picture_area(23, 80, None), (80, 46));
    assert_eq!(viewer::picture_area(0, 80, None), (80, 0));

    // "shrunk to N%" rounds to the nearest percent (1920 wide shown 76 wide is 3.96%), never
    // below 1 nor, as it is shrunk, above 99.
    for (shown, full, percent) in [(76, 1920, 4), (80, 256, 31), (1, 1920, 1), (0, 1920, 1), (1919, 1920, 99), (9, 10, 90), (3, 8, 38), (1, 3, 33), (2, 3, 67)] {
        assert_eq!(viewer::shrunk_percent(shown, full), percent, "{shown} of {full}");
    }
    // Through the status bar: 480x300 in 80x25's 80 by 48 is shown 76 wide, 15.8%.
    let wide = bmp::Image::new(480, 300, 0x336699);
    let mut v = Viewer::with_image("/", "wide.bmp", wide, vec![String::from("wide.bmp")], false, true);
    v.set_view(viewer::picture_area(24, 80, None), false);
    assert!(v.bar(80).starts_with(" wide.bmp  480x300  shrunk to 16%"), "{}", v.bar(80));

    // The picture after and before, in byte order, wrapping.
    let names = viewer::sorted_images(["b.bmp", "A.BMP", "a.bmp", "c.txt", "Z.Bmp"].map(String::from));
    assert_eq!(names, ["A.BMP", "Z.Bmp", "a.bmp", "b.bmp"]);
    assert_eq!(viewer::neighbour(&names, "A.BMP", true), Some(1));
    assert_eq!(viewer::neighbour(&names, "b.bmp", true), Some(0), "wraps forward");
    assert_eq!(viewer::neighbour(&names, "A.BMP", false), Some(3), "wraps back");
    assert_eq!(viewer::neighbour(&names, "a.bmp", false), Some(1));
    assert_eq!(viewer::neighbour(&names, "M.bmp", true), Some(1), "not in the list");
    assert_eq!(viewer::neighbour(&names, "M.bmp", false), Some(0));
    assert_eq!(viewer::neighbour(&[], "a.bmp", true), None);
    assert_eq!(viewer::neighbour(&names[..1], "A.BMP", true), Some(0));
    assert_eq!(viewer::neighbour(&names[..1], "A.BMP", false), Some(0));

    // The status bar: the longest text that fits.
    assert_eq!(viewer::short_name("a_very_long_file_name.bmp"), "a_very_long~");
    assert_eq!(viewer::short_name("twelve_chars"), "twelve_chars");
    let mut info = BarInfo {
        name: "sunset.bmp",
        size: (640, 427),
        pos: Some((3, 12)),
        shrunk: None,
        fitted: true,
        zoom: 100,
        pixel: false,
        fallback: false,
    };
    assert_eq!(viewer::bar_text(&info, 80), " sunset.bmp  640x427  3/12  <-/-> image  F actual  Q quit");
    info.fitted = false;
    assert_eq!(viewer::bar_text(&info, 80), " sunset.bmp  640x427  3/12  arrows pan  P/N image  F fit  Q quit");
    info.fitted = true;
    info.shrunk = Some(31);
    info.name = "gradient.bmp";
    info.size = (256, 128);
    assert_eq!(viewer::bar_text(&info, 80), " gradient.bmp  256x128  shrunk to 31%  3/12  <-/-> image  F actual  Q quit");
    info.pixel = true;
    info.name = "colors.bmp";
    info.size = (64, 40);
    assert_eq!(viewer::bar_text(&info, 80), " colors.bmp  64x40  3/12  <-/-> image  F actual  +/- zoom  Q quit");
    info.fitted = false;
    info.zoom = 800;
    assert_eq!(viewer::bar_text(&info, 80), " colors.bmp  3/12  800% +/-  arrows pan  P/N image  F fit  Q quit");
    info.pixel = false;
    info.fitted = true;
    info.fallback = true;
    info.shrunk = None;
    assert_eq!(viewer::bar_text(&info, 80), " colors.bmp  64x40  3/12  text mode  <-/-> image  F actual  Q quit");
    info.name = "a_very_long_file_name.bmp";
    info.shrunk = Some(7);
    info.size = (1920, 1200);
    for width in 0..=142 {
        let bar = viewer::bar_text(&info, width);
        assert!(bar.len() <= width, "{width}: {bar:?}");
        assert!(viewer::bar_candidates(&info).iter().all(|c| c.len() > width || c.len() <= bar.len()), "{width}: not the longest: {bar:?}");
        assert!(width < 7 || bar.starts_with(' '), "{width}: {bar:?}");
    }
    assert!(viewer::bar_text(&info, 12).ends_with("Q quit"));
    assert_eq!(
        viewer::bad_bar_text("x.bmp", "not a BMP picture this viewer can show", 80),
        " x.bmp: not a BMP picture this viewer can show. Q quits."
    );
    assert_eq!(viewer::bad_bar_text("x.bmp", "not a BMP picture this viewer can show", 20), " Q quit");

    // The keys, without a screen: fit and actual size, panning, zoom, leaving.
    let big = bmp::Image::new(256, 128, 0x336699);
    let mut v = Viewer::with_image("/", "t.bmp", big, vec![String::from("t.bmp")], false, true);
    v.set_view((80, 46), false);
    assert_eq!(v.scale(), Scale::Fit);
    assert!(v.handle(Key::Up) && v.handle(Key::Char(b'+')), "nothing to pan or zoom in the character view");
    assert_eq!(v.scale(), Scale::Fit);
    assert!(v.handle(Key::Char(b'f')));
    assert_eq!(v.scale(), Scale::Percent(100));
    let at = |v: &Viewer| v.placement().map(|p| (p.pan_x, p.pan_y));
    assert_eq!(at(&v), Some((88, 41)), "the middle stays in the middle");
    assert!(v.handle(Key::Right) && v.handle(Key::Down));
    assert_eq!(at(&v), Some((98, 45)), "pans by 1/8 of the area, whole cells vertically");
    for _ in 0..40 {
        v.handle(Key::Right);
        v.handle(Key::Down);
    }
    assert_eq!(at(&v), Some((176, 82)), "stops at the picture's edge");
    let event = Event { action: Action::Move, x: 1, y: 1, dx: 0, dy: 0, buttons: 0 };
    assert!(v.handle(Key::Mouse(event)), "mouse events are ignored");
    assert_eq!(at(&v), Some((176, 82)));
    assert!(v.handle(Key::Char(b'F')));
    assert_eq!(v.scale(), Scale::Fit);
    v.set_view((1024, 752), true);
    assert!(v.is_pixel());
    assert_eq!(v.placement().map(|p| (p.w, p.h)), Some((1024, 512)), "enlarged by 4");
    assert!(v.handle(Key::Char(b'+')));
    assert_eq!(v.scale(), Scale::Percent(800));
    assert!(v.handle(Key::Char(b'-')) && v.handle(Key::Char(b'_')));
    assert_eq!(v.scale(), Scale::Percent(200));
    assert!(v.handle(Key::Char(b'=')));
    assert_eq!(v.scale(), Scale::Percent(400));
    v.set_view((80, 46), false);
    assert!(!v.is_pixel());
    assert_eq!(v.scale(), Scale::Fit, "no zoom steps in the character view");
    for key in [Key::Escape, Key::Char(b'q'), Key::Char(b'Q'), Key::Ctrl(b'q'), Key::Close] {
        assert!(!v.handle(key), "{key:?} leaves");
    }

    // A real folder: N goes round the pictures and comes back to where it started.
    if let Ok(mut v) = Viewer::open("/pictures/colors.bmp", false, true) {
        v.set_view((80, 46), false);
        assert_eq!(v.size(), Some((64, 40)));
        assert!(v.handle(Key::Char(b'n')) && v.handle(Key::Char(b'p')));
        assert_eq!(v.name(), "colors.bmp");
        let mut steps = 0;
        loop {
            v.handle(Key::Right);
            steps += 1;
            assert!(steps < 64, "N never wrapped");
            if v.name() == "colors.bmp" {
                break;
            }
        }
        assert_eq!(v.size(), Some((64, 40)));
    }

    // The commands report a problem before they touch the screen.
    let mut sh = Shell::new();
    let mut run = |line: &str| -> Result<String, String> {
        let mut out = String::new();
        match sh.execute(line, &mut out) {
            Ok(()) => Ok(out),
            Err(CmdError::Usage) => Err(String::from("usage")),
            Err(CmdError::Message(m)) => Err(m),
        }
    };
    assert_eq!(run("view /no/such.bmp").unwrap_err(), "view: /no/such.bmp: not found");
    assert_eq!(run("gview /no/such.bmp").unwrap_err(), "gview: /no/such.bmp: not found");
    assert_eq!(run("view /docs/index.txt").unwrap_err(), "view: /docs/index.txt: not a BMP picture");
    assert_eq!(run("gview /docs/index.txt").unwrap_err(), "gview: /docs/index.txt: not a BMP picture");
    assert_eq!(run("view -n /docs").unwrap_err(), "view: /docs: is a folder");
    assert_eq!(run("view").unwrap_err(), "usage: view [-n] <file>");
    assert_eq!(run("gview").unwrap_err(), "usage: gview <file>");
    assert!(run("gview -n x.bmp").is_err(), "gview has no -n");
    assert!(run("help view").unwrap().contains("no dithering"));
}

fn mouse_events() {
    use crate::mouse::{self, Action, Button, DoubleClick, Event, Packet, PacketDecoder, Tracker};
    // Packets: three bytes; a first byte without bit 3 is out of step and skipped; a gap
    // between bytes starts the packet again.
    let mut d = PacketDecoder::new();
    assert_eq!(d.feed(0x09, 10), None);
    assert_eq!(d.feed(0x05, 10), None);
    assert_eq!(d.feed(0x03, 10), Some([0x09, 0x05, 0x03, 0]));
    assert_eq!(d.feed(0x05, 11), None, "no bit 3: not a first byte");
    assert_eq!(d.feed(0x08, 11), None);
    assert_eq!(d.feed(0x01, 11), None);
    assert_eq!(d.feed(0x02, 11), Some([0x08, 0x01, 0x02, 0]), "back in step");
    assert_eq!(d.feed(0x08, 20), None);
    assert_eq!(d.feed(0x01, 20), None);
    assert_eq!(d.feed(0x18, 25), None, "a byte was lost: this starts a new packet");
    assert_eq!(d.feed(0xFF, 25), None);
    assert_eq!(d.feed(0x00, 25), Some([0x18, 0xFF, 0x00, 0]));
    // Signs, overflow, buttons.
    assert_eq!(Packet::parse([0x08, 10, 5, 0]), Packet { buttons: 0, dx: 10, dy: 5, dz: 0 });
    assert_eq!(Packet::parse([0x38, 0xF6, 0xFB, 0]), Packet { buttons: 0, dx: -10, dy: -5, dz: 0 });
    assert_eq!(Packet::parse([0x09 | 0x40, 200, 3, 0]), Packet { buttons: 1, dx: 0, dy: 3, dz: 0 }, "x overflow");
    assert_eq!(Packet::parse([0x0E, 0, 0, 0]).buttons, 6);
    // Position: down on the screen is the hardware's negative dy; clamped to the area.
    let area = (640, 400);
    let mut t = Tracker::new();
    let mut out = [Event { action: Action::Move, x: 0, y: 0, dx: 0, dy: 0, buttons: 0 }; 5];
    assert_eq!(t.apply(Packet { buttons: 0, dx: 80, dy: -32, dz: 0 }, area, &mut out), 1);
    assert_eq!(out[0], Event { action: Action::Move, x: 80, y: 32, dx: 80, dy: 32, buttons: 0 });
    assert_eq!(out[0].cell((8, 16)), (2, 10));
    assert_eq!(t.apply(Packet { buttons: 1, dx: 0, dy: 0, dz: 0 }, area, &mut out), 1);
    assert_eq!((out[0].action, out[0].buttons), (Action::Down(Button::Left), 1));
    assert!(out[0].held(Button::Left) && !out[0].held(Button::Right));
    assert_eq!(t.apply(Packet { buttons: 2, dx: -5, dy: 0, dz: 0 }, area, &mut out), 3, "move, left up, right down");
    assert_eq!((out[0].action, out[0].x, out[0].buttons), (Action::Move, 75, 1));
    assert_eq!((out[1].action, out[2].action), (Action::Up(Button::Left), Action::Down(Button::Right)));
    assert_eq!(out[2].buttons, 2);
    t.apply(Packet { buttons: 0, dx: 255, dy: 255, dz: 0 }, area, &mut out);
    t.apply(Packet { buttons: 0, dx: 255, dy: 255, dz: 0 }, area, &mut out);
    t.apply(Packet { buttons: 0, dx: 255, dy: 0, dz: 0 }, area, &mut out);
    assert_eq!((t.x, t.y), (639, 0), "clamped at the edges");
    t.clamp((100, 50));
    assert_eq!((t.x, t.y), (99, 0), "clamped to a smaller area");
    // A fast move and many small ones end at the same place.
    let (mut fast, mut slow) = (Tracker::new(), Tracker::new());
    fast.apply(Packet { buttons: 0, dx: 200, dy: -100, dz: 0 }, area, &mut out);
    for _ in 0..50 {
        slow.apply(Packet { buttons: 0, dx: 4, dy: -2, dz: 0 }, area, &mut out);
    }
    assert_eq!((fast.x, fast.y), (slow.x, slow.y));
    // A wheel mouse: 4-byte packets, the same resync rules; the wheel byte is a signed notch
    // count (id 3: a byte; id 4: the low nibble, read the same for small counts).
    let mut d = PacketDecoder::with_wheel(true);
    assert_eq!(d.feed(0x08, 30), None);
    assert_eq!(d.feed(0x00, 30), None);
    assert_eq!(d.feed(0x00, 30), None);
    assert_eq!(d.feed(0xFF, 30), Some([0x08, 0x00, 0x00, 0xFF]));
    assert_eq!(d.feed(0x07, 31), None, "no bit 3: not a first byte");
    assert_eq!(d.feed(0x09, 31), None);
    assert_eq!(d.feed(0x00, 31), None);
    assert_eq!(d.feed(0x18, 40), None, "a byte was lost: this starts a new packet");
    assert_eq!(d.feed(0x01, 40), None);
    assert_eq!(d.feed(0x02, 40), None);
    assert_eq!(d.feed(0x01, 40), Some([0x18, 0x01, 0x02, 0x01]));
    assert_eq!(Packet::parse([0x08, 0, 0, 0xFF]).dz, -1);
    assert_eq!(Packet::parse([0x08, 0, 0, 0x01]).dz, 1);
    assert_eq!(Packet::parse([0x08, 0, 0, 0x3E]).dz, -2, "id 4: buttons 4 and 5 above the nibble");
    assert_eq!(Packet::parse([0x08, 0, 0, 0]).dz, 0);
    let mut t = Tracker::new();
    assert_eq!(t.apply(Packet { buttons: 0, dx: 0, dy: 0, dz: -1 }, area, &mut out), 1, "only the wheel");
    assert_eq!(out[0].action, Action::Wheel(-1));
    assert_eq!(t.apply(Packet { buttons: 4, dx: 3, dy: 0, dz: 2 }, area, &mut out), 3, "move, wheel, middle down");
    assert_eq!((out[0].action, out[1].action, out[2].action), (Action::Move, Action::Wheel(2), Action::Down(Button::Middle)));
    assert_eq!(out[1].buttons, 0, "the wheel comes before the button changes");
    // All three buttons change, with a move and the wheel: five events, the most there are.
    assert_eq!(t.apply(Packet { buttons: 3, dx: 1, dy: 1, dz: 1 }, area, &mut out), 5);
    // Double-click: same target within 400 ms; a third quick press starts again.
    let mut dc = DoubleClick::new();
    assert!(!dc.press(3, 100));
    assert!(dc.press(3, 130));
    assert!(!dc.press(3, 140), "a third press is a new first press");
    assert!(!dc.press(3, 200), "too slow");
    assert!(!dc.press(4, 210), "another target");
    dc.reset();
    assert!(!dc.press(4, 215));
    // mouse.cfg
    assert_eq!(mouse::parse_config("mouse off\n"), Some(false));
    assert_eq!(mouse::parse_config("# note\nmouse on\n"), Some(true));
    assert_eq!(mouse::parse_config("mouse sideways\n"), None);
    assert_eq!(mouse::parse_config(&mouse::config_text(false)), Some(false));
}

/// The pixel surface and the pointer in a graphics mode, and the pointer in text mode: what is
/// drawn under or over the pointer survives it, and it leaves nothing behind.
#[cfg(feature = "qemu-test")]
fn pixel_surface() {
    use crate::display;
    use crate::vga::{attr, cell};
    let mut sh = Shell::new();
    let mut run = |line: &str| {
        let mut out = String::new();
        sh.execute(line, &mut out).unwrap_or_else(|_| panic!("{line}"));
    };
    let fallback = attr(Color::Black, Color::LightGray);
    run("display -f -n 1024x768 -s 100");
    assert_eq!((display::cell_px(), display::area_px()), ((8, 16), (1024, 768)));
    assert_eq!(display::rows_rect(0, 47), Some((0, 0, 1024, 752)));
    // Pixels from the task whose screen is shown; a cell drawn over is drawn again next time.
    let row: Vec<u32> = (0..40).map(|i| 0x010203 * i).collect();
    assert!(console::draw_pixels(5, 7, &row));
    assert_eq!(display::area_pixel(5, 7), Some(0));
    assert_eq!(display::area_pixel(44, 7), Some(0x010203 * 39));
    display::put(0, 1, cell(0xDB, attr(Color::White, Color::Black)));
    assert!(console::fill_pixels(8, 0, 8, 16, 0x123456));
    assert_eq!(display::area_pixel(10, 3), Some(0x123456));
    display::put(0, 1, cell(0xDB, attr(Color::White, Color::Black)));
    assert_eq!(display::pixel_in_cell(0, 1, 3, 8), display::color_pixel(Color::White as usize), "the cell was drawn again");
    // A task whose screen is not shown cannot draw.
    let slot = task::spawn("selftest pixels", || assert!(!console::draw_pixels(0, 0, &[0xFFFFFF]))).expect("spawn");
    task::join(slot);
    task::reap();

    // The arrow: tip black, inside white; what is under it comes back exactly.
    let (x0, y0) = (300, 200);
    console::fill_pixels(x0 - 4, y0 - 4, 40, 40, 0x336699);
    let grab = |x0: usize, y0: usize| -> Vec<Option<u32>> {
        (0..19).flat_map(|y| (0..12).map(move |x| (x, y))).map(|(x, y)| display::area_pixel(x0 + x, y0 + y)).collect()
    };
    let before = grab(x0, y0);
    display::set_pointer(Some((x0, y0)), true, fallback);
    assert_eq!(display::pointer_box(), Some((x0, y0, 12, 19)));
    assert_eq!(display::area_pixel(x0, y0), Some(0), "the tip is the black outline");
    assert_eq!(display::area_pixel(x0 + 1, y0 + 2), Some(0xFFFFFF), "white inside");
    assert_eq!(display::area_pixel(x0 + 11, y0), Some(0x336699), "see-through");
    // Drawing under the pointer: the new pixels are kept, the arrow stays on top.
    console::fill_pixels(x0, y0 + 10, 12, 1, 0xABCDEF);
    assert_eq!(display::area_pixel(x0 + 1, y0 + 10), Some(0xFFFFFF), "the arrow is still on top");
    display::set_pointer(None, true, fallback);
    let after = grab(x0, y0);
    for (i, (b, a)) in before.iter().zip(&after).enumerate() {
        let expected = if i / 12 == 10 { Some(0xABCDEF) } else { *b };
        assert_eq!(*a, expected, "pixel {} of the arrow box", i);
    }
    // Moving: the old place is clean.
    display::set_pointer(Some((x0, y0)), true, fallback);
    display::set_pointer(Some((x0 + 100, y0)), true, fallback);
    assert_eq!(display::area_pixel(x0, y0), Some(0x336699));
    // Text over the pointer: the cell is drawn, the arrow comes back over it.
    let (cx, cy) = display::cell_px();
    let (col, row) = ((x0 + 100) / cx, y0 / cy);
    display::put(row, col, cell(b' ', attr(Color::Black, Color::Red)));
    assert_eq!(display::area_pixel(x0 + 100, y0), Some(0), "arrow over the new cell");
    display::set_pointer(None, true, fallback);
    assert_eq!(display::pixel_in_cell(row, col, (x0 + 100) % cx, y0 % cy), display::color_pixel(Color::Red as usize));
    // At 200% it is twice as large, and clipped at the edge of the text area.
    run("display -f -n 1024x768 -s 200");
    display::set_pointer(Some((10, 10)), true, fallback);
    assert_eq!(display::pointer_box(), Some((10, 10, 24, 38)));
    display::set_pointer(Some((1020, 750)), true, fallback);
    assert_eq!(display::pointer_box(), Some((1020, 750, 4, 18)));
    // A mode change keeps one pointer, inside the new area.
    run("display -f -n 800x600 -s 100");
    assert_eq!(display::pointer_box(), Some((799, 591, 1, 1)));
    display::set_pointer(None, true, fallback);
    assert_eq!(display::pointer_box(), None);

    // Text mode: the cell is highlighted, and shows what it really holds again afterwards.
    run("display -f -n text");
    let vga = |row: usize, col: usize| unsafe { core::ptr::read_volatile((0xB8000 as *const u16).add(row * 80 + col)) };
    display::put(2, 10, cell(b'A', attr(Color::Yellow, Color::Blue)));
    display::set_pointer(Some((80, 32)), true, fallback);
    assert_eq!(display::pointer_cell(), Some((2, 10)));
    assert_eq!(vga(2, 10), cell(b'A', attr(Color::Blue, Color::Yellow)), "colors swapped");
    display::put(2, 10, cell(b'B', attr(Color::Green, Color::Green)));
    assert_eq!(vga(2, 10), cell(b'B', fallback), "same colors: the fallback");
    assert_eq!(display::text_cell(2, 10), cell(b'B', attr(Color::Green, Color::Green)));
    display::set_pointer(Some((80, 32)), false, fallback);
    assert_eq!(vga(2, 10), cell(b'B', attr(Color::Green, Color::Green)), "no highlight here");
    display::set_pointer(Some((0, 0)), true, fallback);
    display::set_pointer(None, true, fallback);
    assert_eq!(display::pointer_cell(), None);
    // Draw the boot screen again over the cell written above (serial is left alone).
    run("display -f -n text");
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
    drop(big);
    // The regions: the low one below the program slots, the high one above them (if there is
    // RAM there), both in the identity-mapped 4 GiB and apart.
    let stats = heap::stats();
    let [(low, low_size), (high, high_size)] = stats.regions;
    assert!(low_size > 0 && (low + low_size) as u64 <= crate::program::PROGRAM_BASE, "low heap region");
    assert_eq!(stats.size, low_size + high_size);
    if high_size > 0 {
        let slots_end = crate::program::PROGRAM_BASE + crate::paging::MAX_SLOTS as u64 * crate::program::PROGRAM_SIZE;
        assert!(high as u64 >= slots_end && (high + high_size) as u64 <= 1 << 32, "high heap region");
        assert!(high_size <= 1 << 30);
        // A block bigger than the whole low region comes from the high one, and is usable.
        let size = low_size + SIZE;
        if crate::picture::fits(size) {
            let mut v = alloc::vec![0u8; size];
            v[size - 1] = 7;
            let at = v.as_ptr() as usize;
            assert!(at >= high && at + size <= high + high_size, "the big block is in the high region");
            assert_eq!(v[size - 1], 7);
        }
    }
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
    // `paint -s` refusals come before any screen is drawn (images.txt has these texts): a size
    // that is no size, with a side of 0 or not WxH, is not "too large".
    const SIZE: &str = "paint: size must be WxH, like 64x40 (up to 4096x4096)";
    for line in ["paint -s 0x5", "paint -s 5x0", "paint -s 0x0", "paint -s 5", "paint -s abc", "paint -s 64x", "paint -s 0x5 new.bmp"] {
        assert_eq!(run(line), Err(String::from(SIZE)), "{line}");
    }
    assert_eq!(run("paint -s 5000x2000"), Err(String::from("paint: 5000x2000 is too large (at most 4096x4096)")));
    assert_eq!(run("paint -s 4097x1"), Err(String::from("paint: 4097x1 is too large (at most 4096x4096)")));
    assert_eq!(
        run("paint -s 8x8 /docs/index.txt"),
        Err(String::from("paint: /docs/index.txt exists: -s only sets the size of a new picture"))
    );
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

/// True if this machine has RAM for programs; otherwise the check is skipped (and says so).
fn program_ram() -> bool {
    if paging::usable_slots() > 0 {
        return true;
    }
    println!("  (skipped: programs need at least {} MiB of RAM)", program::MIN_RAM_MIB);
    false
}

fn programs() {
    if !program_ram() {
        // Running one says why, and nothing is left running.
        let (result, _) = run_asm("ret", "");
        assert!(result.as_ref().is_err_and(|e| e.contains("too little RAM")), "{result:?}");
        assert!(!program::is_running());
        return;
    }
    let (result, out) = run_asm(PRINT_ARGS, "a b");
    assert_eq!(result, Ok(()));
    assert_eq!(out, "hi a b[exit code 42]\n");
    // The exit call ends the program right away.
    let (result, out) = run_asm("mov eax, 0\nmov edi, 7\nint 0x80\nud2", "");
    assert_eq!((result, out.as_str()), (Ok(()), "[exit code 7]\n"));
    assert!(!program::is_running());
    // screen_size: columns in bits 0-15, rows in bits 16-31.
    let (result, out) = run_asm("mov eax, 14\nint 0x80\nret", "");
    let size = console::cols() | console::rows() << 16;
    assert_eq!((result, out), (Ok(()), alloc::format!("[exit code {size}]\n")));
}

/// A crashing program prints a register dump and returns to the caller; the kernel survives.
#[cfg(feature = "qemu-test")]
fn program_crash() {
    if !program_ram() {
        return;
    }
    let (result, _) = run_asm("push rbx\nud2", "");
    assert_eq!(result, Err(String::from("program crashed: #UD invalid opcode")));
    let (result, _) = run_asm("mov rax, 0x800000000000\nmov rax, [rax]", "");
    assert_eq!(result, Err(String::from("program crashed: #GP general protection fault")));
    // Still works afterwards.
    let (result, _) = run_asm("xor eax, eax\nret", "");
    assert_eq!(result, Ok(()));
}
