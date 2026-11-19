//! ATA PIO driver for IDE disks (and SATA disks in IDE compatibility mode) on the two legacy
//! channels. Polled, no interrupts: nIEN is set on each channel.

use crate::port::{inb, inw, outb, outw};
use alloc::string::String;
use alloc::vec::Vec;
use kfs::{BlockDevice, Error, SECTOR};

const CHANNELS: [(u16, u16); 2] = [(0x1F0, 0x3F6), (0x170, 0x376)];

// Register offsets from the I/O base.
const REG_DATA: u16 = 0;
const REG_SECCOUNT: u16 = 2;
const REG_LBA0: u16 = 3;
const REG_LBA1: u16 = 4;
const REG_LBA2: u16 = 5;
const REG_DRIVE: u16 = 6;
const REG_STATUS: u16 = 7;
const REG_COMMAND: u16 = 7;

const STATUS_ERR: u8 = 1 << 0;
const STATUS_DRQ: u8 = 1 << 3;
const STATUS_DF: u8 = 1 << 5;
const STATUS_BSY: u8 = 1 << 7;

const CTRL_NIEN: u8 = 1 << 1;

const CMD_IDENTIFY: u8 = 0xEC;
const CMD_READ: u8 = 0x20;
const CMD_READ_EXT: u8 = 0x24;
const CMD_WRITE: u8 = 0x30;
const CMD_WRITE_EXT: u8 = 0x34;
const CMD_FLUSH: u8 = 0xE7;
const CMD_FLUSH_EXT: u8 = 0xEA;

/// Sectors per command: fits both the LBA28 (256) and LBA48 counts.
const MAX_PER_COMMAND: usize = 128;
const TIMEOUT_POLLS: u32 = 5_000_000;

pub struct AtaDisk {
    base: u16,
    ctrl: u16,
    slave: bool,
    lba48: bool,
    sectors: u64,
    pub model: String,
}

impl AtaDisk {
    pub fn name(&self) -> String {
        let channel = if self.base == CHANNELS[0].0 { "primary" } else { "secondary" };
        alloc::format!("ata {channel} {}", if self.slave { "slave" } else { "master" })
    }

    fn status(&self) -> u8 {
        unsafe { inb(self.base + REG_STATUS) }
    }

    /// About 400 ns: four reads of the alternate status register.
    fn delay(&self) {
        for _ in 0..4 {
            unsafe { inb(self.ctrl) };
        }
    }

    fn wait_not_busy(&self) -> kfs::Result<u8> {
        for _ in 0..TIMEOUT_POLLS {
            let s = self.status();
            if s & STATUS_BSY == 0 {
                return Ok(s);
            }
            core::hint::spin_loop();
        }
        Err(Error::Io)
    }

    fn wait_drq(&self) -> kfs::Result<()> {
        for _ in 0..TIMEOUT_POLLS {
            let s = self.status();
            if s & (STATUS_ERR | STATUS_DF) != 0 {
                return Err(Error::Io);
            }
            if s & STATUS_BSY == 0 && s & STATUS_DRQ != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Io)
    }

    fn select(&self, lba_bits: u8) {
        let drive = 0xE0 | (self.slave as u8) << 4 | lba_bits;
        unsafe { outb(self.base + REG_DRIVE, drive) };
        self.delay();
    }

    fn command(&mut self, lba: u64, count: usize, write: bool) -> kfs::Result<()> {
        self.wait_not_busy()?;
        unsafe {
            if self.lba48 {
                self.select(0);
                outb(self.base + REG_SECCOUNT, (count >> 8) as u8);
                outb(self.base + REG_LBA0, (lba >> 24) as u8);
                outb(self.base + REG_LBA1, (lba >> 32) as u8);
                outb(self.base + REG_LBA2, (lba >> 40) as u8);
                outb(self.base + REG_SECCOUNT, count as u8);
                outb(self.base + REG_LBA0, lba as u8);
                outb(self.base + REG_LBA1, (lba >> 8) as u8);
                outb(self.base + REG_LBA2, (lba >> 16) as u8);
                outb(self.base + REG_COMMAND, if write { CMD_WRITE_EXT } else { CMD_READ_EXT });
            } else {
                if lba + count as u64 > 1 << 28 {
                    return Err(Error::Io);
                }
                self.select(((lba >> 24) & 0xF) as u8);
                outb(self.base + REG_SECCOUNT, count as u8);
                outb(self.base + REG_LBA0, lba as u8);
                outb(self.base + REG_LBA1, (lba >> 8) as u8);
                outb(self.base + REG_LBA2, (lba >> 16) as u8);
                outb(self.base + REG_COMMAND, if write { CMD_WRITE } else { CMD_READ });
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> kfs::Result<()> {
        self.select(0);
        unsafe { outb(self.base + REG_COMMAND, if self.lba48 { CMD_FLUSH_EXT } else { CMD_FLUSH }) };
        self.delay();
        let s = self.wait_not_busy()?;
        if s & (STATUS_ERR | STATUS_DF) != 0 {
            return Err(Error::Io);
        }
        Ok(())
    }
}

impl BlockDevice for AtaDisk {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> kfs::Result<()> {
        for (i, chunk) in buf.chunks_mut(MAX_PER_COMMAND * SECTOR).enumerate() {
            let count = chunk.len() / SECTOR;
            self.command(lba + (i * MAX_PER_COMMAND) as u64, count, false)?;
            for sector in chunk.chunks_mut(SECTOR) {
                self.delay();
                self.wait_drq()?;
                for word in sector.chunks_mut(2) {
                    let w = unsafe { inw(self.base + REG_DATA) };
                    word.copy_from_slice(&w.to_le_bytes());
                }
            }
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> kfs::Result<()> {
        for (i, chunk) in buf.chunks(MAX_PER_COMMAND * SECTOR).enumerate() {
            let count = chunk.len() / SECTOR;
            self.command(lba + (i * MAX_PER_COMMAND) as u64, count, true)?;
            for sector in chunk.chunks(SECTOR) {
                self.delay();
                self.wait_drq()?;
                for word in sector.chunks(2) {
                    unsafe { outw(self.base + REG_DATA, u16::from_le_bytes([word[0], word[1]])) };
                }
            }
        }
        self.flush()
    }
}

/// Finds the ATA disks on both legacy channels.
pub fn probe() -> Vec<AtaDisk> {
    let mut disks = Vec::new();
    for (base, ctrl) in CHANNELS {
        // A floating bus reads 0xFF: no controller on this channel.
        if unsafe { inb(base + REG_STATUS) } == 0xFF {
            continue;
        }
        unsafe { outb(ctrl, CTRL_NIEN) };
        for slave in [false, true] {
            if let Some(disk) = identify(base, ctrl, slave) {
                disks.push(disk);
            }
        }
    }
    disks
}

fn identify(base: u16, ctrl: u16, slave: bool) -> Option<AtaDisk> {
    let mut disk = AtaDisk { base, ctrl, slave, lba48: false, sectors: 0, model: String::new() };
    disk.select(0);
    unsafe {
        outb(base + REG_SECCOUNT, 0);
        outb(base + REG_LBA0, 0);
        outb(base + REG_LBA1, 0);
        outb(base + REG_LBA2, 0);
        outb(base + REG_COMMAND, CMD_IDENTIFY);
    }
    disk.delay();
    if disk.status() == 0 {
        return None; // no device
    }
    disk.wait_not_busy().ok()?;
    // ATAPI and SATA devices answer with a signature in LBA1/LBA2 instead.
    if unsafe { inb(base + REG_LBA1) != 0 || inb(base + REG_LBA2) != 0 } {
        return None;
    }
    disk.wait_drq().ok()?;
    let mut id = [0u16; 256];
    for word in id.iter_mut() {
        *word = unsafe { inw(base + REG_DATA) };
    }
    disk.lba48 = id[83] & (1 << 10) != 0;
    disk.sectors = if disk.lba48 {
        id[100] as u64 | (id[101] as u64) << 16 | (id[102] as u64) << 32 | (id[103] as u64) << 48
    } else {
        id[60] as u64 | (id[61] as u64) << 16
    };
    if disk.sectors == 0 {
        return None;
    }
    // Model: words 27-46, two characters per word, high byte first.
    let mut model = String::new();
    for w in &id[27..47] {
        for b in [(w >> 8) as u8, *w as u8] {
            model.push(if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' });
        }
    }
    disk.model = String::from(model.trim());
    Some(disk)
}
