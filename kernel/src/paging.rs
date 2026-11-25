//! Program memory: one 16 MiB block of RAM per running program, all seen at `PROGRAM_BASE`.
//!
//! Programs are assembled for the fixed address `PROGRAM_BASE` (64 MiB), so two programs can
//! only run at once if each sees its own memory there. Stage2's page tables identity-map the
//! first 4 GiB with 2 MiB pages. Program slot 0 is the identity-mapped block at 64 MiB itself,
//! used with those page tables. Every other slot k is a block at 80 MiB + (k-1) * 16 MiB with its
//! own copy of the tables, in which the 8 entries for 64-80 MiB point at that block instead.
//! Everything else (the kernel, the heap, low memory, every block's own identity address) is
//! mapped the same way in all of them.
//!
//! The tables are in `.bss`, below 4 GiB, because the BIOS trampoline saves CR3 as 32 bits.

use crate::boot_info;
use crate::program::{PROGRAM_BASE, PROGRAM_SIZE};
use crate::sync::SpinLock;
use crate::task::MAX_TASKS;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const MAX_SLOTS: usize = 8;
const ENTRIES: usize = 512;
const PAGE_2M: u64 = 2 * 1024 * 1024;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const FLAGS_MASK: u64 = 0xFFF;
/// Physical address of program slot 1; the others follow every 16 MiB.
const FIRST_EXTRA_BLOCK: u64 = PROGRAM_BASE + PROGRAM_SIZE;

#[repr(C, align(4096))]
struct Tables {
    pml4: [u64; ENTRIES],
    pdpt: [u64; ENTRIES],
    /// Covers 0-1 GiB, with entries 32-39 (64-80 MiB) pointing at the slot's block.
    pd: [u64; ENTRIES],
}

static mut TABLES: [Tables; MAX_SLOTS] =
    [const { Tables { pml4: [0; ENTRIES], pdpt: [0; ENTRIES], pd: [0; ENTRIES] } }; MAX_SLOTS];

static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);
/// The slot's block is ordinary RAM.
static USABLE: [AtomicBool; MAX_SLOTS] = [const { AtomicBool::new(false) }; MAX_SLOTS];
/// The task slot using each program slot.
static OWNER: SpinLock<[Option<usize>; MAX_SLOTS]> = SpinLock::new([None; MAX_SLOTS]);

/// Physical address of a program slot's 16 MiB.
pub fn block_of(slot: usize) -> u64 {
    if slot == 0 {
        PROGRAM_BASE
    } else {
        FIRST_EXTRA_BLOCK + (slot as u64 - 1) * PROGRAM_SIZE
    }
}

fn ram_at(base: u64) -> bool {
    let end = base + PROGRAM_SIZE;
    end <= 1 << 32
        && boot_info::get().is_some_and(|info| {
            info.memory_map().iter().any(|r| r.is_usable() && r.base <= base && r.base + r.length >= end)
        })
}

/// Builds the page tables of every program slot that has RAM behind it. Needs the BootInfo.
pub fn init() {
    let cr3 = crate::cpu::read_cr3() & ADDR_MASK;
    KERNEL_CR3.store(cr3, Ordering::Relaxed);
    let kernel_pml4 = cr3 as *const [u64; ENTRIES];
    let first = PROGRAM_BASE / PAGE_2M;
    unsafe {
        let pdpt_addr = (*kernel_pml4)[0] & ADDR_MASK;
        let kernel_pdpt = pdpt_addr as *const [u64; ENTRIES];
        let pd_addr = (*kernel_pdpt)[0] & ADDR_MASK;
        let kernel_pd = pd_addr as *const [u64; ENTRIES];
        for slot in 0..MAX_SLOTS {
            if !ram_at(block_of(slot)) {
                continue;
            }
            USABLE[slot].store(true, Ordering::Relaxed);
            if slot == 0 {
                continue;
            }
            let tables = &raw mut TABLES[slot];
            (*tables).pml4 = *kernel_pml4;
            (*tables).pml4[0] = (&raw const (*tables).pdpt) as u64 | ((*kernel_pml4)[0] & FLAGS_MASK);
            (*tables).pdpt = *kernel_pdpt;
            (*tables).pdpt[0] = (&raw const (*tables).pd) as u64 | ((*kernel_pdpt)[0] & FLAGS_MASK);
            (*tables).pd = *kernel_pd;
            for i in 0..(PROGRAM_SIZE / PAGE_2M) {
                let entry = (first + i) as usize;
                (*tables).pd[entry] = (block_of(slot) + i * PAGE_2M) | ((*kernel_pd)[entry] & FLAGS_MASK);
            }
        }
    }
}

/// Stage2's page tables, which every task uses when it is not running a program in slot 1+.
pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(Ordering::Relaxed)
}

/// The page tables for a program slot.
pub fn cr3_of(slot: usize) -> u64 {
    if slot == 0 {
        kernel_cr3()
    } else {
        unsafe { (&raw const TABLES[slot]) as u64 }
    }
}

/// How many programs can run at once on this machine.
pub fn usable_slots() -> usize {
    USABLE.iter().filter(|u| u.load(Ordering::Relaxed)).count()
}

/// Takes a free program slot for task `task`. The boot task prefers slot 0, so the shell
/// without the desktop runs programs exactly where it always did; other tasks prefer the rest.
pub fn alloc(task: usize) -> Option<usize> {
    let mut owners = OWNER.lock();
    let free = |i: usize| owners[i].is_none() && USABLE[i].load(Ordering::Relaxed);
    let slot = if task == crate::task::BOOT {
        (0..MAX_SLOTS).find(|&i| free(i))
    } else {
        (1..MAX_SLOTS).chain(0..1).find(|&i| free(i))
    }?;
    owners[slot] = Some(task);
    Some(slot)
}

pub fn free(slot: usize) {
    if let Some(owner) = OWNER.lock().get_mut(slot) {
        *owner = None;
    }
}

/// Frees whatever program slot a task that ended (or was killed) still had.
pub fn release_task(task: usize) {
    for owner in OWNER.lock().iter_mut() {
        if *owner == Some(task) {
            *owner = None;
        }
    }
}

/// Program slots in use, with the task slot using each.
pub fn owners() -> [Option<usize>; MAX_SLOTS] {
    *OWNER.lock()
}

/// The program slot task `task` uses, if any.
pub fn slot_of_task(task: usize) -> Option<usize> {
    OWNER.lock().iter().position(|&o| o == Some(task))
}

const _: () = assert!(MAX_TASKS >= MAX_SLOTS);
