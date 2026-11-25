//! Just enough ACPI to power the machine off: find the FADT for the PM1 control ports and the
//! `\_S5` sleep type in the DSDT. The FADT also names the RTC's century register. All tables live
//! below 4 GiB, which is identity-mapped.

use crate::cpu;
use crate::port::{inw, outb, outw};
use core::ptr::read_unaligned;

const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
const BDA_EBDA_SEGMENT: usize = 0x40E;
const BIOS_AREA: (usize, usize) = (0xE0000, 0x100000);
const SDT_HEADER_LEN: usize = 36;
const MAPPED_LIMIT: u64 = 1 << 32;

// FADT field offsets.
const FADT_DSDT: usize = 40;
const FADT_SMI_CMD: usize = 48;
const FADT_ACPI_ENABLE: usize = 52;
const FADT_PM1A_CNT: usize = 64;
const FADT_PM1B_CNT: usize = 68;
const FADT_CENTURY: usize = 108;
const FADT_X_DSDT: usize = 140;

const SCI_EN: u16 = 1 << 0;
const SLP_EN: u16 = 1 << 13;
const SLP_TYP_SHIFT: u16 = 10;

// AML opcodes used to parse `Name(_S5_, Package() { a, b, ... })`.
const AML_NAME_OP: u8 = 0x08;
const AML_PACKAGE_OP: u8 = 0x12;
const AML_BYTE_PREFIX: u8 = 0x0A;
const AML_ROOT_CHAR: u8 = b'\\';

#[derive(Clone, Copy, Debug)]
pub struct PowerInfo {
    pub pm1a_cnt: u16,
    pub pm1b_cnt: u16,
    pub smi_cmd: u16,
    pub acpi_enable: u8,
    pub slp_typ_a: u16,
    pub slp_typ_b: u16,
}

fn read<T: Copy>(addr: usize) -> T {
    unsafe { read_unaligned(addr as *const T) }
}

fn bytes(addr: usize, len: usize) -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(addr as *const u8, len) }
}

fn checksum_ok(addr: usize, len: usize) -> bool {
    bytes(addr, len).iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

fn find_rsdp() -> Option<usize> {
    let scan = |start: usize, end: usize| {
        (start..end)
            .step_by(16)
            .find(|&a| bytes(a, 8) == RSDP_SIGNATURE && checksum_ok(a, 20))
    };
    let ebda = (read::<u16>(BDA_EBDA_SEGMENT) as usize) << 4;
    let in_ebda = if (0x80000..0xA0000).contains(&ebda) { scan(ebda, ebda + 1024) } else { None };
    in_ebda.or_else(|| scan(BIOS_AREA.0, BIOS_AREA.1))
}

/// Returns the address of a table header if it is mapped and its checksum is valid.
fn table(addr: u64) -> Option<usize> {
    if addr == 0 || addr >= MAPPED_LIMIT {
        return None;
    }
    let addr = addr as usize;
    let len = read::<u32>(addr + 4) as usize;
    (len >= SDT_HEADER_LEN && checksum_ok(addr, len)).then_some(addr)
}

fn find_table(rsdp: usize, signature: &[u8; 4]) -> Option<usize> {
    let revision: u8 = read(rsdp + 15);
    let xsdt = if revision >= 2 { table(read::<u64>(rsdp + 24)) } else { None };
    let (root, entry_size) = match xsdt {
        Some(x) => (x, 8),
        None => (table(read::<u32>(rsdp + 16) as u64)?, 4),
    };
    let len = read::<u32>(root + 4) as usize;
    (SDT_HEADER_LEN..len).step_by(entry_size).find_map(|off| {
        let entry = if entry_size == 8 { read::<u64>(root + off) } else { read::<u32>(root + off) as u64 };
        table(entry).filter(|&t| bytes(t, 4) == signature)
    })
}

/// Finds the `\_S5` package in AML and returns its first two values (SLP_TYPa, SLP_TYPb).
pub fn parse_s5(aml: &[u8]) -> Option<(u16, u16)> {
    let pos = aml.windows(4).enumerate().find_map(|(i, w)| {
        let named = (i >= 1 && aml[i - 1] == AML_NAME_OP)
            || (i >= 2 && aml[i - 2] == AML_NAME_OP && aml[i - 1] == AML_ROOT_CHAR);
        (w == b"_S5_" && named).then_some(i + 4)
    })?;
    let mut p = pos;
    if *aml.get(p)? != AML_PACKAGE_OP {
        return None;
    }
    p += 1;
    // PkgLength: the top two bits of the lead byte give the number of extra bytes.
    let extra = (*aml.get(p)? >> 6) as usize;
    p += 1 + extra;
    p += 1; // NumElements
    let mut value = || -> Option<u16> {
        let b = *aml.get(p)?;
        if b == AML_BYTE_PREFIX {
            p += 2;
            Some(*aml.get(p - 1)? as u16)
        } else {
            p += 1;
            Some(b as u16)
        }
    };
    let a = value()?;
    let b = value()?;
    Some((a, b))
}

pub fn power_info() -> Option<PowerInfo> {
    let rsdp = find_rsdp()?;
    let fadt = find_table(rsdp, b"FACP")?;
    let fadt_len = read::<u32>(fadt + 4) as usize;
    let x_dsdt = if fadt_len >= FADT_X_DSDT + 8 { read::<u64>(fadt + FADT_X_DSDT) } else { 0 };
    let dsdt = table(x_dsdt).or_else(|| table(read::<u32>(fadt + FADT_DSDT) as u64))?;
    let dsdt_len = read::<u32>(dsdt + 4) as usize;
    let (slp_typ_a, slp_typ_b) =
        parse_s5(bytes(dsdt + SDT_HEADER_LEN, dsdt_len - SDT_HEADER_LEN))?;
    Some(PowerInfo {
        pm1a_cnt: read::<u32>(fadt + FADT_PM1A_CNT) as u16,
        pm1b_cnt: read::<u32>(fadt + FADT_PM1B_CNT) as u16,
        smi_cmd: read::<u32>(fadt + FADT_SMI_CMD) as u16,
        acpi_enable: read(fadt + FADT_ACPI_ENABLE),
        slp_typ_a,
        slp_typ_b,
    })
}

/// The CMOS register that holds the century of the RTC date, if the FADT names one.
pub fn century_register() -> Option<u8> {
    let rsdp = find_rsdp()?;
    let fadt = find_table(rsdp, b"FACP")?;
    let fadt_len = read::<u32>(fadt + 4) as usize;
    if fadt_len <= FADT_CENTURY {
        return None;
    }
    let reg: u8 = read(fadt + FADT_CENTURY);
    // Registers 0x0E-0x7F are general CMOS RAM; anything else is not a real century register.
    (0x0E..0x80).contains(&reg).then_some(reg)
}

/// Powers the machine off. Tries ACPI S5 first, then the fixed ports that emulators use.
/// Returns only if nothing worked.
pub fn power_off() {
    cpu::cli();
    if let Some(info) = power_info() {
        unsafe {
            if inw(info.pm1a_cnt) & SCI_EN == 0 && info.smi_cmd != 0 && info.acpi_enable != 0 {
                outb(info.smi_cmd, info.acpi_enable);
                for _ in 0..10_000_000 {
                    if inw(info.pm1a_cnt) & SCI_EN != 0 {
                        break;
                    }
                    core::hint::spin_loop();
                }
            }
            outw(info.pm1a_cnt, info.slp_typ_a << SLP_TYP_SHIFT | SLP_EN);
            if info.pm1b_cnt != 0 {
                outw(info.pm1b_cnt, info.slp_typ_b << SLP_TYP_SHIFT | SLP_EN);
            }
        }
        spin(10_000_000);
    }
    unsafe {
        outw(0x604, 0x2000); // QEMU
        outw(0xB004, 0x2000); // Bochs, older QEMU
        outw(0x4004, 0x3400); // VirtualBox
    }
    spin(10_000_000);
}

fn spin(n: u32) {
    for _ in 0..n {
        core::hint::spin_loop();
    }
}
