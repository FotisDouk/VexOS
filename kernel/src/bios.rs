//! Calling the BIOS from long mode through the real-mode trampoline in `Bootloader/bioscall.asm`,
//! and a disk driver built on INT 13h. Works with whatever device the PC booted from (USB stick,
//! SATA in AHCI mode, IDE), as long as the machine boots through a legacy BIOS or CSM.
//!
//! The addresses and the request block layout must match `Bootloader/config.inc`.

use crate::pic;
use crate::pit;
use crate::sync::SpinLock;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use kfs::{BlockDevice, Error, SECTOR};

const BIOSCALL_ADDR: usize = 0x2000;
const BIOSCALL_DATA: usize = 0x3000;
const BIOSCALL_CODE_MAX: usize = BIOSCALL_DATA - BIOSCALL_ADDR;
/// Disk address packet for INT 13h extensions, right after the request block.
const DAP_ADDR: usize = BIOSCALL_DATA + 0x40;
/// Low-memory buffer the BIOS reads into and writes from (config.inc BOUNCE_ADDR / _SECTORS).
const BOUNCE_ADDR: usize = 0x10000;
const BOUNCE_SECTORS: usize = 64;

static TRAMPOLINE: &[u8] = include_bytes!("../../build/bioscall.bin");
const _: () = assert!(TRAMPOLINE.len() <= BIOSCALL_CODE_MAX);

static READY: AtomicBool = AtomicBool::new(false);
static LOCK: SpinLock<()> = SpinLock::new(());

const FLAG_CARRY: u32 = 1;

/// Registers passed to and returned from a BIOS interrupt.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Regs {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    pub esi: u32,
    pub edi: u32,
    pub ebp: u32,
    pub ds: u16,
    pub es: u16,
    pub eflags: u32,
}

impl Regs {
    pub fn carry(&self) -> bool {
        self.eflags & FLAG_CARRY != 0
    }
}

/// The request block at BIOSCALL_DATA (config.inc BC_*).
#[repr(C)]
struct Block {
    regs: Regs,
    int: u8,
    bios_masks: [u8; 2],
    kernel_masks: [u8; 2],
    kernel_bases: [u8; 2],
    _pad: u8,
    pit_divisor: u16,
}

const _: () = assert!(core::mem::offset_of!(Block, int) == 36);
const _: () = assert!(core::mem::offset_of!(Block, bios_masks) == 37);
const _: () = assert!(core::mem::offset_of!(Block, kernel_masks) == 39);
const _: () = assert!(core::mem::offset_of!(Block, kernel_bases) == 41);
const _: () = assert!(core::mem::offset_of!(Block, pit_divisor) == 44);

/// Copies the trampoline into low memory. Needs `pic::init` to have saved the BIOS masks.
pub fn init() {
    unsafe {
        core::ptr::copy_nonoverlapping(TRAMPOLINE.as_ptr(), BIOSCALL_ADDR as *mut u8, TRAMPOLINE.len());
    }
    READY.store(true, Ordering::Release);
}

/// Runs `int vector` in real mode with `regs`, and returns the registers the BIOS left.
pub fn call(vector: u8, regs: Regs) -> Regs {
    assert!(READY.load(Ordering::Acquire), "bios::call before bios::init");
    let _guard = LOCK.lock();
    let block = BIOSCALL_DATA as *mut Block;
    unsafe {
        write_volatile(
            block,
            Block {
                regs,
                int: vector,
                bios_masks: pic::bios_masks(),
                kernel_masks: pic::KERNEL_MASKS,
                kernel_bases: [pic::IRQ_BASE, pic::IRQ_BASE + 8],
                _pad: 0,
                pit_divisor: pit::DIVISOR,
            },
        );
        let trampoline: extern "sysv64" fn() = core::mem::transmute(BIOSCALL_ADDR);
        trampoline();
        // Pick up what the PIC reset may have hidden (see `keyboard::poll_pending`).
        crate::keyboard::poll_pending();
        read_volatile(&raw const (*block).regs)
    }
}

/// A disk accessed through INT 13h extensions.
pub struct BiosDisk {
    drive: u8,
    sectors: u64,
}

impl BiosDisk {
    /// Returns `None` if the drive does not support the INT 13h extensions.
    pub fn open(drive: u8) -> Option<BiosDisk> {
        let r = call(0x13, Regs { eax: 0x4100, ebx: 0x55AA, edx: drive as u32, ..Default::default() });
        if r.carry() || r.ebx & 0xFFFF != 0xAA55 {
            return None;
        }
        // AH=48h: extended drive parameters into the bounce buffer (u64 sector count at 0x10).
        let params = BOUNCE_ADDR as *mut u8;
        unsafe {
            core::ptr::write_bytes(params, 0, 0x1E);
            write_volatile(params as *mut u16, 0x1E);
        }
        let r = call(
            0x13,
            Regs {
                eax: 0x4800,
                edx: drive as u32,
                esi: 0,
                ds: (BOUNCE_ADDR >> 4) as u16,
                ..Default::default()
            },
        );
        if r.carry() {
            return None;
        }
        let sectors = unsafe { read_volatile((BOUNCE_ADDR + 0x10) as *const u64) };
        Some(BiosDisk { drive, sectors })
    }

    pub fn drive(&self) -> u8 {
        self.drive
    }

    /// Transfers up to BOUNCE_SECTORS sectors between the disk and the bounce buffer.
    fn transfer(&mut self, write: bool, lba: u64, count: usize) -> kfs::Result<()> {
        for _attempt in 0..3 {
            unsafe {
                let dap = DAP_ADDR as *mut u8;
                write_volatile(dap, 0x10);
                write_volatile(dap.add(1), 0);
                write_volatile(dap.add(2) as *mut u16, count as u16);
                write_volatile(dap.add(4) as *mut u16, 0);
                write_volatile(dap.add(6) as *mut u16, (BOUNCE_ADDR >> 4) as u16);
                write_volatile(dap.add(8) as *mut u64, lba);
            }
            let ah = if write { 0x43 } else { 0x42 };
            let r = call(
                0x13,
                Regs { eax: ah << 8, edx: self.drive as u32, esi: DAP_ADDR as u32, ..Default::default() },
            );
            if !r.carry() {
                return Ok(());
            }
            call(0x13, Regs { eax: 0, edx: self.drive as u32, ..Default::default() });
        }
        Err(Error::Io)
    }
}

impl BlockDevice for BiosDisk {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> kfs::Result<()> {
        for (i, chunk) in buf.chunks_mut(BOUNCE_SECTORS * SECTOR).enumerate() {
            let at = lba + (i * BOUNCE_SECTORS) as u64;
            self.transfer(false, at, chunk.len() / SECTOR)?;
            unsafe {
                core::ptr::copy_nonoverlapping(BOUNCE_ADDR as *const u8, chunk.as_mut_ptr(), chunk.len());
            }
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> kfs::Result<()> {
        for (i, chunk) in buf.chunks(BOUNCE_SECTORS * SECTOR).enumerate() {
            let at = lba + (i * BOUNCE_SECTORS) as u64;
            unsafe {
                core::ptr::copy_nonoverlapping(chunk.as_ptr(), BOUNCE_ADDR as *mut u8, chunk.len());
            }
            self.transfer(true, at, chunk.len() / SECTOR)?;
        }
        Ok(())
    }
}
