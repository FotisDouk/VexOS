#![no_std]
#![no_main]
// The test build exits after the self-tests and never starts the interactive shell.
#![cfg_attr(feature = "qemu-test", allow(dead_code))]

extern crate alloc;

mod acpi;
mod ata;
mod bios;
mod boot_info;
mod console;
mod cpu;
mod editor;
mod fs;
mod gdt;
mod heap;
mod input;
mod interrupts;
mod keyboard;
mod pic;
mod pit;
mod port;
mod program;
#[cfg(feature = "qemu-test")]
mod qemu;
mod selftest;
mod serial;
mod shell;
mod sync;
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
    console::init();
    gdt::init();
    interrupts::init();

    println!("kernel-test: running in 64-bit long mode");
    println!("CPU: kernel GDT, TSS and IDT loaded");

    let info = unsafe { BootInfo::from_ptr(boot_info) }
        .expect("bootloader passed an invalid BootInfo");
    boot_info::set(info);
    heap::init(info);

    pic::init();
    bios::init();
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
    let heap = heap::stats();
    println!(
        "Heap:         {:#x} - {:#x} ({} MiB)",
        heap.start,
        heap.start + heap.size,
        heap.size / (1024 * 1024)
    );

    println!("Disk:         {}", fs::init(info.boot_drive as u8));

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
    println!("Boot complete.");
    shell::run()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    cpu::cli();
    unsafe { console::force_unlock() };
    console::set_color(vga::Color::White, vga::Color::Red);
    println!("KERNEL PANIC: {}", info);
    #[cfg(feature = "qemu-test")]
    qemu::exit(qemu::QemuExit::Failed);
    #[cfg(not(feature = "qemu-test"))]
    cpu::halt()
}
