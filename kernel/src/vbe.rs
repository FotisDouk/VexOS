//! The video BIOS (INT 10h) and the Bochs display interface ("DISPI", in QEMU, Bochs and
//! VirtualBox): VESA BIOS Extensions (VBE) graphics modes with a linear framebuffer, the VGA text
//! modes, the BIOS's 8x16 font, and the monitor's preferred resolution (EDID).
//!
//! The BIOS is called through `bios::call` (real mode). It writes its answers to low memory at
//! `VIDEO_BUF` (`Bootloader/config.inc`), which only this module uses. Everything here is called
//! by `display.rs` with its lock held, so the buffer is never used twice at once.

use crate::bios::{self, Regs};
use crate::port::{inw, outw};
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_bytes};

/// 0x3200-0x357F: VBE controller information (512 bytes), mode information (256), EDID (128).
const VIDEO_BUF: usize = 0x3200;
const INFO_BUF: usize = VIDEO_BUF;
const MODE_BUF: usize = VIDEO_BUF + 0x200;
const EDID_BUF: usize = VIDEO_BUF + 0x300;
/// What every VBE function returns in AX when it worked.
const VBE_OK: u32 = 0x004F;

fn int10(regs: Regs) -> Regs {
    bios::call(0x10, regs)
}

fn ok(r: &Regs) -> bool {
    r.eax & 0xFFFF == VBE_OK
}

fn segment(addr: usize) -> u16 {
    (addr >> 4) as u16
}

fn read8(addr: usize) -> u8 {
    unsafe { read_volatile(addr as *const u8) }
}

fn read16(addr: usize) -> u16 {
    u16::from_le_bytes([read8(addr), read8(addr + 1)])
}

fn read32(addr: usize) -> u32 {
    u32::from_le_bytes([read8(addr), read8(addr + 1), read8(addr + 2), read8(addr + 3)])
}

/// Where the bits of red, green and blue are in a pixel: (size, position) of each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PixelFormat {
    pub red: (u8, u8),
    pub green: (u8, u8),
    pub blue: (u8, u8),
}

impl PixelFormat {
    pub const RGB32: PixelFormat = PixelFormat { red: (8, 16), green: (8, 8), blue: (8, 0) };

    fn default_for(bpp: u32) -> PixelFormat {
        match bpp {
            16 => PixelFormat { red: (5, 11), green: (6, 5), blue: (5, 0) },
            15 => PixelFormat { red: (5, 10), green: (5, 5), blue: (5, 0) },
            _ => Self::RGB32,
        }
    }

    /// The pixel value of a color.
    pub fn pixel(&self, [r, g, b]: [u8; 3]) -> u32 {
        let part = |value: u8, (size, pos): (u8, u8)| ((value as u32) >> (8 - size.min(8))) << pos;
        part(r, self.red) | part(g, self.green) | part(b, self.blue)
    }
}

/// What the video BIOS offers as a whole.
pub struct Controller {
    /// 0x0200 = VBE 2.0, 0x0300 = VBE 3.0.
    pub version: u16,
    /// Video memory in bytes.
    pub memory: u64,
    pub modes: Vec<u16>,
}

/// INT 10h AX=4F00h: the VBE version, the video memory and the list of mode numbers.
pub fn controller() -> Option<Controller> {
    unsafe {
        write_bytes(INFO_BUF as *mut u8, 0, 512);
        core::ptr::copy_nonoverlapping(b"VBE2".as_ptr(), INFO_BUF as *mut u8, 4);
    }
    let r = int10(Regs { eax: 0x4F00, es: segment(INFO_BUF), edi: 0, ..Default::default() });
    if !ok(&r) || (0..4).map(|i| read8(INFO_BUF + i)).ne(*b"VESA") {
        return None;
    }
    let version = read16(INFO_BUF + 4);
    let list = read16(INFO_BUF + 16) as usize * 16 + read16(INFO_BUF + 14) as usize;
    let memory = read16(INFO_BUF + 18) as u64 * 64 * 1024;
    let mut modes = Vec::new();
    if list + 2 <= 0x100000 {
        for i in 0..512 {
            let mode = read16(list + i * 2);
            if mode == 0xFFFF {
                break;
            }
            modes.push(mode);
        }
    }
    Some(Controller { version, memory, modes })
}

/// A graphics mode the system can use: direct color, 15 to 32 bits per pixel, with a linear
/// framebuffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModeInfo {
    pub number: u16,
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    /// Bytes from one line of pixels to the next.
    pub pitch: u32,
    /// Physical address of the framebuffer.
    pub base: u32,
    pub format: PixelFormat,
}

/// INT 10h AX=4F01h: what mode `number` is, if it is a mode the system can use.
pub fn mode_info(number: u16, version: u16) -> Option<ModeInfo> {
    const SUPPORTED: u16 = 1 << 0;
    const GRAPHICS: u16 = 1 << 4;
    const LINEAR: u16 = 1 << 7;
    const PACKED_PIXEL: u8 = 4;
    const DIRECT_COLOR: u8 = 6;
    unsafe { write_bytes(MODE_BUF as *mut u8, 0, 256) };
    let r = int10(Regs { eax: 0x4F01, ecx: number as u32, es: segment(MODE_BUF), edi: 0, ..Default::default() });
    if !ok(&r) {
        return None;
    }
    let attributes = read16(MODE_BUF);
    let wanted = SUPPORTED | GRAPHICS | LINEAR;
    let bpp = read8(MODE_BUF + 25) as u32;
    let model = read8(MODE_BUF + 27);
    if attributes & wanted != wanted || !matches!(bpp, 15 | 16 | 24 | 32) || !matches!(model, PACKED_PIXEL | DIRECT_COLOR) {
        return None;
    }
    let linear_pitch = if version >= 0x300 { read16(MODE_BUF + 50) } else { 0 };
    let pitch = if linear_pitch != 0 { linear_pitch } else { read16(MODE_BUF + 16) } as u32;
    let base = read32(MODE_BUF + 40);
    let width = read16(MODE_BUF + 18) as u32;
    let height = read16(MODE_BUF + 20) as u32;
    if base == 0 || width == 0 || height == 0 || pitch < width * bpp.div_ceil(8) {
        return None;
    }
    let field = |at: usize| (read8(MODE_BUF + at), read8(MODE_BUF + at + 1));
    let format = match (field(31), field(33), field(35)) {
        (red, green, blue) if model == DIRECT_COLOR && red.0 > 0 && green.0 > 0 && blue.0 > 0 => {
            PixelFormat { red, green, blue }
        }
        _ => PixelFormat::default_for(bpp),
    };
    Some(ModeInfo { number, width, height, bpp, pitch, base, format })
}

/// INT 10h AX=4F02h: switches to graphics mode `number` with its linear framebuffer.
pub fn set_mode(number: u16) -> bool {
    const USE_LINEAR: u32 = 0x4000;
    ok(&int10(Regs { eax: 0x4F02, ebx: number as u32 | USE_LINEAR, ..Default::default() }))
}

/// VGA text mode 3 (80x25), or 80x50 with the BIOS's 8x8 font.
pub fn set_text_mode(rows50: bool) {
    int10(Regs { eax: 0x0003, ..Default::default() });
    if rows50 {
        // AX=1112h: load the 8x8 ROM font, which gives 50 rows.
        int10(Regs { eax: 0x1112, ebx: 0, ..Default::default() });
    }
}

/// INT 10h AX=1130h BH=6: the video BIOS's 8x16 font, 16 bytes per character, one bit per pixel
/// (bit 7 = the leftmost).
pub fn font_8x16() -> Option<[u8; 4096]> {
    let r = int10(Regs { eax: 0x1130, ebx: 0x0600, ..Default::default() });
    let addr = r.es as usize * 16 + (r.ebp & 0xFFFF) as usize;
    if addr == 0 || addr + 4096 > 0x100000 {
        return None;
    }
    let mut font = [0u8; 4096];
    for (i, byte) in font.iter_mut().enumerate() {
        *byte = read8(addr + i);
    }
    // A real font draws something for 'A' and nothing for a space.
    let glyph = |c: u8| &font[c as usize * 16..c as usize * 16 + 16];
    let blank = |c: u8| glyph(c).iter().all(|&b| b == 0);
    (!blank(b'A') && blank(b' ')).then_some(font)
}

/// INT 10h AX=4F15h BL=1: the monitor's preferred resolution, from the first detailed timing of
/// its EDID. `None` if the BIOS or the monitor does not say.
pub fn preferred_resolution() -> Option<(u32, u32)> {
    unsafe { write_bytes(EDID_BUF as *mut u8, 0, 128) };
    let r = int10(Regs { eax: 0x4F15, ebx: 1, ecx: 0, edx: 0, es: segment(EDID_BUF), edi: 0, ..Default::default() });
    let edid: Vec<u8> = (0..128).map(|i| read8(EDID_BUF + i)).collect();
    if !ok(&r) || edid[..8] != [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0] {
        return None;
    }
    let timing = &edid[54..72];
    if timing[0] == 0 && timing[1] == 0 {
        return None;
    }
    let width = timing[2] as u32 | (timing[4] as u32 & 0xF0) << 4;
    let height = timing[5] as u32 | (timing[7] as u32 & 0xF0) << 4;
    (width > 0 && height > 0).then_some((width, height))
}

// --- The Bochs display interface: any resolution, set directly through ports 0x1CE/0x1CF ---

const DISPI_INDEX: u16 = 0x01CE;
const DISPI_DATA: u16 = 0x01CF;
const DISPI_ID: u16 = 0;
const DISPI_XRES: u16 = 1;
const DISPI_YRES: u16 = 2;
const DISPI_BPP: u16 = 3;
const DISPI_ENABLE: u16 = 4;
const DISPI_VIRT_WIDTH: u16 = 6;
const DISPI_X_OFFSET: u16 = 8;
const DISPI_Y_OFFSET: u16 = 9;
const DISPI_ENABLED: u16 = 0x01;
const DISPI_GETCAPS: u16 = 0x02;
const DISPI_LINEAR: u16 = 0x40;

fn dispi_read(index: u16) -> u16 {
    unsafe {
        outw(DISPI_INDEX, index);
        inw(DISPI_DATA)
    }
}

fn dispi_write(index: u16, value: u16) {
    unsafe {
        outw(DISPI_INDEX, index);
        outw(DISPI_DATA, value);
    }
}

/// The largest resolution the Bochs display interface can be set to, if there is one.
pub fn dispi_limits() -> Option<(u32, u32)> {
    if !(0xB0C0..=0xB0C5).contains(&dispi_read(DISPI_ID)) {
        return None;
    }
    let enable = dispi_read(DISPI_ENABLE);
    dispi_write(DISPI_ENABLE, enable | DISPI_GETCAPS);
    let limits = (dispi_read(DISPI_XRES) as u32, dispi_read(DISPI_YRES) as u32);
    dispi_write(DISPI_ENABLE, enable);
    // Without capabilities (old versions) the registers show the current mode instead.
    Some(if limits.0 >= 1024 && limits.1 >= 768 { limits } else { (1600, 1200) })
}

/// Sets `width` x `height` at 32 bits per pixel through the Bochs display interface. Returns the
/// size the hardware took (it may round the width) and the pitch in bytes.
pub fn dispi_set(width: u32, height: u32) -> Option<(u32, u32, u32)> {
    dispi_write(DISPI_ENABLE, 0);
    dispi_write(DISPI_XRES, width as u16);
    dispi_write(DISPI_YRES, height as u16);
    dispi_write(DISPI_BPP, 32);
    dispi_write(DISPI_ENABLE, DISPI_ENABLED | DISPI_LINEAR);
    dispi_write(DISPI_X_OFFSET, 0);
    dispi_write(DISPI_Y_OFFSET, 0);
    let (w, h) = (dispi_read(DISPI_XRES) as u32, dispi_read(DISPI_YRES) as u32);
    let virt = dispi_read(DISPI_VIRT_WIDTH) as u32;
    if dispi_read(DISPI_BPP) != 32 || w == 0 || h == 0 {
        dispi_write(DISPI_ENABLE, 0);
        return None;
    }
    Some((w, h, virt.max(w) * 4))
}

/// Turns the Bochs display interface off (before going back to a VGA mode).
pub fn dispi_disable() {
    if (0xB0C0..=0xB0C5).contains(&dispi_read(DISPI_ID)) {
        dispi_write(DISPI_ENABLE, 0);
    }
}
