#![no_std]
#![no_main]

mod boot_info;
mod console;
mod port;
mod serial;
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

    println!("kernel-test: running in 64-bit long mode");

    let info = unsafe { BootInfo::from_ptr(boot_info) }
        .expect("bootloader passed an invalid BootInfo");

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

    println!("Boot complete. System halted.");
    halt()
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe { console::force_unlock() };
    console::set_color(vga::Color::White, vga::Color::Red);
    println!("KERNEL PANIC: {}", info);
    halt()
}
