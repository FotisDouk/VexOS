#![no_std]
#![no_main]
// The test build exits after the self-tests and never starts the interactive shell.
#![cfg_attr(feature = "qemu-test", allow(dead_code))]

extern crate alloc;

mod acpi;
mod ata;
mod bios;
mod boot_info;
mod clock;
mod console;
mod cpu;
mod desktop;
mod display;
mod editor;
mod fs;
mod gdt;
mod heap;
mod hexedit;
mod input;
mod interrupts;
mod keyboard;
mod mouse;
mod paging;
mod paint;
mod pic;
mod picture;
mod pit;
mod port;
mod program;
#[cfg(feature = "qemu-test")]
mod qemu;
mod rtc;
mod selftest;
mod serial;
mod shell;
mod sync;
mod task;
mod theme;
mod vbe;
mod viewer;
mod vga;

use boot_info::BootInfo;
use core::panic::PanicInfo;

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax"
    .global _start
_start:
    cli
    cld
    lea rsp, [rip + __stack_top]
    mov r12, rdi
    lea rdi, [rip + __bss_start]
    lea rcx, [rip + __bss_end]
    sub rcx, rdi
    xor eax, eax
    rep stosb
    mov rdi, r12
    xor ebp, ebp
    call {main}
2:
    cli
    hlt
    jmp 2b
    "#,
    main = sym kernel_main,
);

extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

extern "C" fn kernel_main(boot_info: *const BootInfo) -> ! {
    display::note_boot_keys();
    console::init();
    gdt::init();
    interrupts::init();

    println!("kernel-test: running in 64-bit long mode");
    println!("CPU: kernel GDT, TSS and IDT loaded");

    let info = unsafe { BootInfo::from_ptr(boot_info) }
        .expect("bootloader passed an invalid BootInfo");
    boot_info::set(info);
    heap::init(info);
    console::init_heap();
    paging::init();
    task::init();

    pic::init();
    bios::init();
    display::init();
    pit::init();
    let keyboard = keyboard::init();
    cpu::sti();
    println!(
        "IRQs: PIC remapped, timer at {} Hz, PS/2 keyboard {}",
        pit::TICK_HZ,
        if keyboard { "ready" } else { "not found" }
    );

    let start = &raw const __kernel_start as usize;
    let end = &raw const __kernel_end as usize;
    println!("Kernel image: {:#x} - {:#x} ({} KiB)", start, end, (end - start) / 1024);
    println!("Boot drive:   {:#04x}", info.boot_drive);

    let map = info.memory_map();
    println!("Memory map ({} entries):", map.len());
    let mut usable = 0u64;
    for region in map {
        println!(
            "  {:#014x} - {:#014x}  {}",
            region.base,
            region.base + region.length,
            region.kind_name()
        );
        if region.is_usable() {
            usable += region.length;
        }
    }
    println!("Usable RAM:   {} MiB", usable / (1024 * 1024));
    for (i, (start, size)) in heap::stats().parts().enumerate() {
        let label = if i == 0 { "Heap:" } else { "" };
        println!("{label:<14}{start:#x} - {:#x} ({} MiB)", start + size, size / (1024 * 1024));
    }

    println!("Disk:         {}", fs::init(info.boot_drive as u8));

    rtc::init();
    let clock_ok = clock::load();
    let now = clock::now();
    println!("Clock:        {} {}", now.weekday_name(), now);
    if !clock_ok {
        println!("              ({} has no valid 'offset' line; using the hardware clock)", clock::CONFIG_PATH);
    }
    let theme_errors = theme::load();
    if let Some(first) = theme_errors.first() {
        println!("Theme:        {}: {first} (and {} more)", theme::CONFIG_PATH, theme_errors.len() - 1);
    }
    let mouse_start = pit::uptime_ms();
    let mouse = match mouse::init() {
        mouse::Status::Found => "found",
        mouse::Status::NotFound => "not found",
        mouse::Status::NotProbed => "off (mouse.cfg)",
    };
    println!("Mouse:        PS/2 {mouse} ({} ms)", pit::uptime_ms() - mouse_start);

    selftest::run();
    finish()
}

#[cfg(feature = "qemu-test")]
fn finish() -> ! {
    println!("Boot complete");
    qemu::exit(qemu::QemuExit::Success)
}

#[cfg(not(feature = "qemu-test"))]
fn finish() -> ! {
    start_display();
    println!("Boot complete.");
    shell::run()
}

/// Switches to the display mode saved with `display`, unless Shift was held at start-up.
#[cfg(not(feature = "qemu-test"))]
fn start_display() {
    match display::saved() {
        None => {}
        Some(_) if display::shift_at_boot() => println!("Display:      Shift held: staying in text mode"),
        Some(Err(e)) => println!("Display:      {}: {e}", display::CONFIG_PATH),
        Some(Ok(request)) => match console::set_display(request) {
            Ok(mode) => println!("Display:      {}", mode.describe()),
            Err(e) => println!("Display:      {e}; staying in text mode"),
        },
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    cpu::cli();
    unsafe { console::force_unlock() };
    console::show_current_for_panic();
    console::set_color(vga::Color::White, vga::Color::Red);
    println!("KERNEL PANIC: {}", info);
    #[cfg(feature = "qemu-test")]
    qemu::exit(qemu::QemuExit::Failed);
    #[cfg(not(feature = "qemu-test"))]
    cpu::halt()
}
