//! The boot disk and its file system.
//!
//! At boot the disk the BIOS booted from is opened through INT 13h. If an ATA disk has the same
//! contents (sector 0 and the superblock), the native ATA driver is used for it instead, since it
//! is much faster. The KFS file system on it is mounted; raw sector access works even when it is
//! not formatted.

use crate::ata::{self, AtaDisk};
use crate::bios::BiosDisk;
use crate::sync::SpinLock;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use kfs::{BlockDevice, Fs, SECTOR};

pub type Device = Box<dyn BlockDevice + Send>;

enum State {
    Mounted(Fs<Device>),
    Unformatted(Device),
}

struct Disk {
    driver: String,
    state: State,
}

static DISK: SpinLock<Option<Disk>> = SpinLock::new(None);
static DISKS: SpinLock<Vec<String>> = SpinLock::new(Vec::new());

#[derive(Debug)]
pub enum FsError {
    NoDisk,
    NotFormatted,
    Fs(kfs::Error),
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            FsError::NoDisk => f.write_str("no boot disk available"),
            FsError::NotFormatted => f.write_str("the boot disk has no file system (use 'format')"),
            FsError::Fs(e) => write!(f, "{e}"),
        }
    }
}

impl From<kfs::Error> for FsError {
    fn from(e: kfs::Error) -> Self {
        FsError::Fs(e)
    }
}

pub type Result<T> = core::result::Result<T, FsError>;

fn read_sectors(dev: &mut dyn BlockDevice, lba: u64, count: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; count * SECTOR];
    dev.read(lba, &mut buf).ok()?;
    Some(buf)
}

/// Opens the boot disk with the best driver and mounts its file system. Returns a description.
pub fn init(boot_drive: u8) -> String {
    let mut bios = BiosDisk::open(boot_drive);
    let mut atas = ata::probe();

    let mut list = Vec::new();
    if let Some(b) = &bios {
        list.push(alloc::format!("bios {:#04x}: boot disk, {} MiB", b.drive(), b.sector_count() / 2048));
    }
    for a in &atas {
        list.push(alloc::format!("{}: {} MiB, {}", a.name(), a.sector_count() / 2048, a.model));
    }
    *DISKS.lock() = list;

    // Prefer an ATA disk with the same first sectors as the BIOS boot disk.
    let reference = bios.as_mut().and_then(|b| read_sectors(b, 0, kfs::SUPERBLOCK_LBA as usize + 1));
    let same = |a: &mut AtaDisk| {
        reference.as_ref().is_some_and(|r| read_sectors(a, 0, kfs::SUPERBLOCK_LBA as usize + 1).as_ref() == Some(r))
    };
    let (driver, device): (String, Device) = match atas.iter_mut().position(same) {
        Some(i) => {
            let disk = atas.swap_remove(i);
            (alloc::format!("{} (ATA PIO)", disk.name()), Box::new(disk))
        }
        None => match bios {
            Some(b) => (alloc::format!("bios {:#04x} (INT 13h)", b.drive()), Box::new(b)),
            None => return String::from("no usable boot disk (INT 13h extensions missing)"),
        },
    };

    let sectors = device.sector_count();
    let state = remount_device(device);
    let status = match state {
        State::Mounted(_) => "KFS mounted",
        State::Unformatted(_) => "no file system",
    };
    let description = alloc::format!("{driver}, {} MiB, {status}", sectors / 2048);
    *DISK.lock() = Some(Disk { driver, state });
    description
}

/// Runs `f` on the mounted file system.
pub fn with<R>(f: impl FnOnce(&mut Fs<Device>) -> kfs::Result<R>) -> Result<R> {
    match DISK.lock().as_mut() {
        None => Err(FsError::NoDisk),
        Some(Disk { state: State::Unformatted(_), .. }) => Err(FsError::NotFormatted),
        Some(Disk { state: State::Mounted(fs), .. }) => Ok(f(fs)?),
    }
}

/// Runs `f` on the raw boot disk. If it writes, the file system is re-read afterwards, so raw
/// changes to the superblock or bitmap take effect.
pub fn raw<R>(write: bool, f: impl FnOnce(&mut dyn BlockDevice) -> kfs::Result<R>) -> Result<R> {
    let mut guard = DISK.lock();
    let disk = guard.as_mut().ok_or(FsError::NoDisk)?;
    let result = match &mut disk.state {
        State::Mounted(fs) => f(fs.device()),
        State::Unformatted(dev) => f(dev),
    };
    if write {
        remount(disk);
    }
    Ok(result?)
}

fn remount(disk: &mut Disk) {
    // Swap the state out to move the device through mount.
    let placeholder = State::Unformatted(Box::new(kfs::MemDisk::new(0)));
    let device = match core::mem::replace(&mut disk.state, placeholder) {
        State::Mounted(fs) => fs.into_device(),
        State::Unformatted(dev) => dev,
    };
    disk.state = remount_device(device);
}

/// Mounts `device`, falling back to raw access when there is no valid file system. Mounting
/// consumes the device even on failure, so it is first tried on a borrowed device.
fn remount_device(mut device: Device) -> State {
    if Fs::mount(&mut device).is_ok() {
        if let Ok(fs) = Fs::mount(device) {
            return State::Mounted(fs);
        }
        unreachable!("mount succeeded once and then failed");
    }
    State::Unformatted(device)
}

/// Re-creates the file system on the boot disk, keeping the bootloader and kernel files (and
/// the clock setting, if one was made).
pub fn format() -> Result<()> {
    let mut guard = DISK.lock();
    let disk = guard.as_mut().ok_or(FsError::NoDisk)?;
    let mut keep = Vec::new();
    if let State::Mounted(fs) = &mut disk.state {
        for path in [kfs::BOOT_PATH, kfs::KERNEL_PATH] {
            if let Ok(data) = fs.read(path) {
                keep.push((path, data));
            }
        }
    }
    let placeholder = State::Unformatted(Box::new(kfs::MemDisk::new(0)));
    let device = match core::mem::replace(&mut disk.state, placeholder) {
        State::Mounted(fs) => fs.into_device(),
        State::Unformatted(dev) => dev,
    };
    let mut fs = Fs::format(device)?;
    let restored = (|| {
        fs.mkdir("/system")?;
        fs.mkdir("/programs")?;
        fs.mkdir(crate::desktop::home::DESKTOP_DIR)?;
        for (path, data) in &keep {
            fs.write(path, data)?;
        }
        // The clock setting lives in memory too, so it survives the format.
        let offset = crate::clock::offset();
        if offset != 0 {
            fs.write(crate::clock::CONFIG_PATH, crate::clock::config_text(offset).as_bytes())?;
        }
        Ok(())
    })();
    disk.state = State::Mounted(fs);
    restored.map_err(FsError::Fs)
}

pub fn driver() -> Option<String> {
    DISK.lock().as_ref().map(|d| d.driver.clone())
}

pub fn disk_list() -> Vec<String> {
    DISKS.lock().clone()
}
