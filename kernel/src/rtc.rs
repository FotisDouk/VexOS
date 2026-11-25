//! The CMOS real-time clock (RTC): the battery-backed clock that keeps the date and time while
//! the computer is off. Read by polling ports 0x70/0x71; IRQ 8 stays masked.
//!
//! `DateTime` also does the calendar arithmetic (days since 1970, weekdays, parsing), which is
//! pure and tested by the self-tests.

use crate::port::{inb, outb};
use crate::sync::SpinLock;
use core::fmt;
use core::sync::atomic::{AtomicU8, Ordering};

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

const STATUS_A_UPDATING: u8 = 0x80;
const STATUS_B_24_HOUR: u8 = 0x02;
const STATUS_B_BINARY: u8 = 0x04;
const HOUR_PM: u8 = 0x80;

/// The CMOS register holding the century, from the ACPI FADT; 0 = none (assume 20xx).
static CENTURY_REG: AtomicU8 = AtomicU8::new(0);
/// The index/data port pair must not be used by two readers at once.
static LOCK: SpinLock<()> = SpinLock::new(());

/// Looks up the century register. Call once at boot.
pub fn init() {
    CENTURY_REG.store(crate::acpi::century_register().unwrap_or(0), Ordering::Relaxed);
}

fn read_reg(reg: u8) -> u8 {
    unsafe {
        outb(INDEX, reg);
        inb(DATA)
    }
}

/// Waits (briefly) until the RTC is not in the middle of updating its registers.
fn wait_not_updating() {
    for _ in 0..100_000 {
        if read_reg(REG_STATUS_A) & STATUS_A_UPDATING == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

fn read_raw(century_reg: u8) -> [u8; 7] {
    wait_not_updating();
    [
        read_reg(REG_SECONDS),
        read_reg(REG_MINUTES),
        read_reg(REG_HOURS),
        read_reg(REG_DAY),
        read_reg(REG_MONTH),
        read_reg(REG_YEAR),
        if century_reg != 0 { read_reg(century_reg) } else { 0 },
    ]
}

fn from_bcd(value: u8) -> u8 {
    (value >> 4) * 10 + (value & 0x0F)
}

/// Reads the hardware clock. The registers are read until two reads in a row agree, so an
/// update in the middle of a read cannot give a mixed-up time.
pub fn read() -> DateTime {
    let century_reg = CENTURY_REG.load(Ordering::Relaxed);
    let _guard = LOCK.lock();
    let mut raw = read_raw(century_reg);
    for _ in 0..4 {
        let again = read_raw(century_reg);
        if again == raw {
            break;
        }
        raw = again;
    }
    let status_b = read_reg(REG_STATUS_B);
    drop(_guard);

    let binary = status_b & STATUS_B_BINARY != 0;
    let value = |v: u8| if binary { v } else { from_bcd(v) };
    let pm = raw[2] & HOUR_PM != 0;
    let mut hour = value(raw[2] & !HOUR_PM);
    if status_b & STATUS_B_24_HOUR == 0 {
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let century = match value(raw[6]) {
        c @ 19..=29 if century_reg != 0 => c as u16,
        _ => 20,
    };
    DateTime {
        year: century * 100 + value(raw[5]) as u16,
        month: value(raw[4]),
        day: value(raw[3]),
        hour,
        minute: value(raw[1]),
        second: value(raw[0]),
    }
}

/// A calendar date and time of day (no time zone).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        2 if is_leap(year as i64) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given date (proleptic Gregorian calendar).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

impl DateTime {
    /// Seconds since 1970-01-01 00:00:00.
    pub fn to_unix(&self) -> i64 {
        let days = days_from_civil(self.year as i64, self.month as i64, self.day as i64);
        days * 86_400 + self.hour as i64 * 3600 + self.minute as i64 * 60 + self.second as i64
    }

    /// The inverse of `to_unix`. Times before 1970 are clamped to 1970-01-01.
    pub fn from_unix(seconds: i64) -> DateTime {
        let seconds = seconds.max(0);
        let (year, month, day) = civil_from_days(seconds / 86_400);
        let rest = seconds % 86_400;
        DateTime {
            year: year.min(9999) as u16,
            month,
            day,
            hour: (rest / 3600) as u8,
            minute: (rest / 60 % 60) as u8,
            second: (rest % 60) as u8,
        }
    }

    /// True if every field is in range (as the RTC should report, but may not after a dead
    /// battery).
    pub fn is_valid(&self) -> bool {
        (1970..=9999).contains(&self.year)
            && (1..=12).contains(&self.month)
            && self.day >= 1
            && self.day <= days_in_month(self.year, self.month)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
    }

    /// 0 = Sunday ... 6 = Saturday.
    pub fn weekday(&self) -> u8 {
        let days = days_from_civil(self.year as i64, self.month as i64, self.day as i64);
        // 1970-01-01 was a Thursday.
        (days + 4).rem_euclid(7) as u8
    }

    pub fn weekday_name(&self) -> &'static str {
        WEEKDAYS[self.weekday() as usize]
    }

    pub fn month_name(&self) -> &'static str {
        MONTHS[(self.month.clamp(1, 12) - 1) as usize]
    }

    /// Parses `YYYY-MM-DD`.
    pub fn parse_date(text: &str) -> Option<(u16, u8, u8)> {
        let mut parts = text.split('-');
        let year: u16 = parts.next()?.parse().ok()?;
        let month: u8 = parts.next()?.parse().ok()?;
        let day: u8 = parts.next()?.parse().ok()?;
        if parts.next().is_some() || !(1970..=9999).contains(&year) || !(1..=12).contains(&month) {
            return None;
        }
        (day >= 1 && day <= days_in_month(year, month)).then_some((year, month, day))
    }

    /// Parses `HH:MM` or `HH:MM:SS` (24-hour).
    pub fn parse_time(text: &str) -> Option<(u8, u8, u8)> {
        let mut parts = text.split(':');
        let hour: u8 = parts.next()?.parse().ok()?;
        let minute: u8 = parts.next()?.parse().ok()?;
        let second: u8 = match parts.next() {
            Some(s) => s.parse().ok()?,
            None => 0,
        };
        if parts.next().is_some() || hour >= 24 || minute >= 60 || second >= 60 {
            return None;
        }
        Some((hour, minute, second))
    }
}

/// `2026-11-25 14:30:05`
impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}
