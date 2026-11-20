//! Kernel heap: a first-fit free-list allocator over one physical memory region.
//!
//! The free list is sorted by address and adjacent free blocks are merged on free. Every block
//! address and size is a multiple of `BLOCK_ALIGN`, so splitting a block never leaves a fragment
//! too small to hold a `FreeBlock` header.

use crate::boot_info::BootInfo;
use crate::sync::SpinLock;
use core::alloc::{GlobalAlloc, Layout};
use core::mem::size_of;
use core::ptr;

const BLOCK_ALIGN: usize = 16;
const PAGE: u64 = 4096;
const MAX_HEAP_SIZE: u64 = 32 * 1024 * 1024;
const MIN_HEAP_SIZE: u64 = 1024 * 1024;
/// Stage2 identity-maps only the first 4 GiB.
const MAPPED_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
/// BootInfo, the memory map, page tables and the bounce buffer live below 1 MiB.
const LOW_MEMORY_END: u64 = 0x100000;

struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

const _: () = assert!(size_of::<FreeBlock>() <= BLOCK_ALIGN);

struct Heap {
    /// Dummy list head with size 0; `head.next` is the lowest free block.
    head: FreeBlock,
    start: usize,
    size: usize,
    used: usize,
}

// The raw pointers only ever point into the heap region, which the lock protects.
unsafe impl Send for Heap {}

#[derive(Clone, Copy)]
pub struct Stats {
    pub start: usize,
    pub size: usize,
    pub used: usize,
}

impl Stats {
    pub fn free(&self) -> usize {
        self.size - self.used
    }
}

impl Heap {
    const fn empty() -> Self {
        Self { head: FreeBlock { size: 0, next: ptr::null_mut() }, start: 0, size: 0, used: 0 }
    }

    /// Normalized size and alignment of an allocation.
    fn block_layout(layout: Layout) -> (usize, usize) {
        let size = layout.size().max(1).next_multiple_of(BLOCK_ALIGN);
        (size, layout.align().max(BLOCK_ALIGN))
    }

    /// Adds `[addr, addr + size)` to the free list, merging it with its neighbours.
    unsafe fn insert(&mut self, addr: usize, size: usize) {
        unsafe {
            let head: *mut FreeBlock = &mut self.head;
            let mut prev = head;
            while !(*prev).next.is_null() && ((*prev).next as usize) < addr {
                prev = (*prev).next;
            }
            let next = (*prev).next;
            debug_assert!(prev == head || prev as usize + (*prev).size <= addr, "double free");
            debug_assert!(next.is_null() || addr + size <= next as usize, "double free");

            let block = addr as *mut FreeBlock;
            block.write(FreeBlock { size, next });
            (*prev).next = block;

            if !next.is_null() && addr + size == next as usize {
                (*block).size += (*next).size;
                (*block).next = (*next).next;
            }
            if prev != head && prev as usize + (*prev).size == addr {
                (*prev).size += (*block).size;
                (*prev).next = (*block).next;
            }
        }
    }

    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let (size, align) = Self::block_layout(layout);
        unsafe {
            let mut prev: *mut FreeBlock = &mut self.head;
            while !(*prev).next.is_null() {
                let block = (*prev).next;
                let block_start = block as usize;
                let block_end = block_start + (*block).size;
                let start = block_start.next_multiple_of(align);
                if let Some(end) = start.checked_add(size).filter(|&end| end <= block_end) {
                    (*prev).next = (*block).next;
                    if end < block_end {
                        self.insert(end, block_end - end);
                    }
                    if start > block_start {
                        self.insert(block_start, start - block_start);
                    }
                    self.used += size;
                    return start as *mut u8;
                }
                prev = block;
            }
        }
        ptr::null_mut()
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let (size, _) = Self::block_layout(layout);
        self.used -= size;
        unsafe { self.insert(ptr as usize, size) };
    }
}

pub struct KernelAllocator(SpinLock<Heap>);

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { self.0.lock().alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.lock().dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator(SpinLock::new(Heap::empty()));

extern "C" {
    static __kernel_end: u8;
}

/// Puts the heap in the first usable memory region above the kernel image and below the 4 GiB
/// that is identity-mapped, capped at `MAX_HEAP_SIZE`. Panics if there is no such region.
pub fn init(info: &BootInfo) {
    let kernel_end = (&raw const __kernel_end as u64).next_multiple_of(PAGE);
    let floor = kernel_end.max(LOW_MEMORY_END);

    let region = info
        .memory_map()
        .iter()
        .filter(|r| r.is_usable())
        .filter_map(|r| {
            let start = r.base.max(floor).next_multiple_of(PAGE);
            // Stop below the program region, which `program` uses.
            let end = (r.base.saturating_add(r.length)).min(MAPPED_LIMIT).min(crate::program::PROGRAM_BASE) / PAGE * PAGE;
            (end >= start + MIN_HEAP_SIZE).then_some((start, end))
        })
        .min_by_key(|&(start, _)| start);
    let (start, end) = region.expect("no usable memory region for the kernel heap");
    let size = (end - start).min(MAX_HEAP_SIZE);

    let mut heap = ALLOCATOR.0.lock();
    assert!(heap.size == 0, "heap initialized twice");
    heap.start = start as usize;
    heap.size = size as usize;
    unsafe { heap.insert(start as usize, size as usize) };
}

pub fn stats() -> Stats {
    let heap = ALLOCATOR.0.lock();
    Stats { start: heap.start, size: heap.size, used: heap.used }
}
