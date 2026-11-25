//! The time of day the user sees: the hardware clock (RTC) plus an offset in seconds.
//!
//! `date -s` does not change the hardware clock. It stores the difference between the time the
//! user typed and the RTC in `/system/clock.cfg`, which is read at boot. That works the same on
//! real PCs (the RTC keeps running while the power is off) and in QEMU (which resets the RTC to
//! the host's time at every start).

use crate::fs;
use crate::rtc::{self, DateTime};
use alloc::string::String;
use core::sync::atomic::{AtomicI64, Ordering};

pub const CONFIG_PATH: &str = "/system/clock.cfg";

static OFFSET: AtomicI64 = AtomicI64::new(0);

/// Reads the saved offset from `CONFIG_PATH`, if the file is there. Returns false if it exists
/// but has no valid `offset` line.
pub fn load() -> bool {
    let Ok(data) = fs::with(|fs| fs.read(CONFIG_PATH)) else {
        return true;
    };
    match parse_config(&String::from_utf8_lossy(&data)) {
        Some(offset) => {
            OFFSET.store(offset, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Finds the `offset <seconds>` line; `#` lines are comments.
pub fn parse_config(text: &str) -> Option<i64> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .find_map(|line| line.strip_prefix("offset")?.trim().parse().ok())
}

pub fn config_text(offset: i64) -> String {
    alloc::format!(
        "# Seconds added to the hardware clock (RTC) to get the time of day.\n\
         # Written by 'date -s'.\noffset {offset}\n"
    )
}

pub fn offset() -> i64 {
    OFFSET.load(Ordering::Relaxed)
}

/// Replaces the offset in memory only (the self-tests restore it this way).
#[cfg_attr(not(feature = "qemu-test"), allow(dead_code))]
pub fn set_offset(offset: i64) {
    OFFSET.store(offset, Ordering::Relaxed);
}

/// Seconds since 1970 in the user's time.
pub fn unix_now() -> i64 {
    rtc::read().to_unix() + offset()
}

pub fn now() -> DateTime {
    DateTime::from_unix(unix_now())
}

/// Makes `time` the current time and saves the new offset to `CONFIG_PATH`.
pub fn set(time: DateTime) -> fs::Result<()> {
    let offset = time.to_unix() - rtc::read().to_unix();
    OFFSET.store(offset, Ordering::Relaxed);
    save()
}

pub fn save() -> fs::Result<()> {
    let text = config_text(offset());
    fs::with(|fs| fs.write(CONFIG_PATH, text.as_bytes()))
}
