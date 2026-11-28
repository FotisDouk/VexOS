//! The display: VGA text mode, or a VESA graphics mode in which the text cells are drawn as
//! pixels, with the video BIOS's 8x16 font at a scale of 100% to 800%.
//!
//! Screens (`vga.rs`) are grids of cells in the VGA text format either way, and the screen shown
//! writes through to `put`. In text mode a cell goes to VGA memory at 0xB8000. In a graphics mode
//! its glyph is drawn into the linear framebuffer, and `drawn` keeps what every cell shows, so
//! cells that did not change are not drawn again (that keeps scrolling fast). The cursor is the
//! VGA hardware cursor in text mode and is drawn by hand in graphics modes.
//!
//! The resolution asked for is the area the text is laid out in. When the video card has no mode
//! of exactly that size, a larger mode is used with the area in its middle and black around it
//! ("framed"). The Bochs display interface (QEMU, Bochs, VirtualBox) can be set to any size.
//!
//! The current mode is saved in `/system/display.cfg` by the `display` command and set again
//! after the boot self-tests, unless Shift is held while the computer starts.

use crate::fs;
use crate::port::{inb, outb};
use crate::sync::SpinLock;
use crate::vbe::{self, ModeInfo, PixelFormat};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const FONT_WIDTH: usize = 8;
pub const FONT_HEIGHT: usize = 16;
pub const MIN_SCALE: u32 = 100;
pub const MAX_SCALE: u32 = 800;
/// The text grid of a graphics mode must be at least this large (the desktop and the editors
/// need the room) and at most this large (every task keeps a copy of its screen).
pub const MIN_COLS: usize = 40;
pub const MIN_ROWS: usize = 12;
pub const MAX_COLS: usize = 640;
pub const MAX_ROWS: usize = 270;
/// Resolutions that can be asked for, in pixels.
pub const MIN_WIDTH: u32 = 320;
pub const MIN_HEIGHT: u32 = 200;
pub const MAX_SIZE: u32 = 16384;

pub const CONFIG_PATH: &str = "/system/display.cfg";

const TEXT_BUFFER: *mut u16 = 0xB8000 as *mut u16;
const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;
const CRTC_CURSOR_START: u8 = 0x0A;
const CRTC_CURSOR_END: u8 = 0x0B;
const CRTC_CURSOR_HIGH: u8 = 0x0E;
const CRTC_CURSOR_LOW: u8 = 0x0F;
/// Bit 5 of the cursor start register turns the cursor off.
const CURSOR_DISABLE: u8 = 1 << 5;
const ATTR_INDEX: u16 = 0x3C0;
const ATTR_READ: u16 = 0x3C1;
const INPUT_STATUS: u16 = 0x3DA;
const DAC_WRITE_INDEX: u16 = 0x3C8;
const DAC_DATA: u16 = 0x3C9;
/// The DAC entries text mode uses for the 16 colors (the BIOS's attribute palette).
const TEXT_DAC_INDEX: [u8; 16] = [0, 1, 2, 3, 4, 5, 20, 7, 56, 57, 58, 59, 60, 61, 62, 63];

/// The standard VGA colors, in the order of `vga::Color`.
pub const DEFAULT_PALETTE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0x00, 0x00, 0xAA],
    [0x00, 0xAA, 0x00],
    [0x00, 0xAA, 0xAA],
    [0xAA, 0x00, 0x00],
    [0xAA, 0x00, 0xAA],
    [0xAA, 0x55, 0x00],
    [0xAA, 0xAA, 0xAA],
    [0x55, 0x55, 0x55],
    [0x55, 0x55, 0xFF],
    [0x55, 0xFF, 0x55],
    [0x55, 0xFF, 0xFF],
    [0xFF, 0x55, 0x55],
    [0xFF, 0x55, 0xFF],
    [0xFF, 0xFF, 0x55],
    [0xFF, 0xFF, 0xFF],
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Underline,
    Block,
}

/// A display mode to switch to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// VGA text mode: 80x25, or 80x50 with the 8x8 font.
    Text { rows50: bool },
    /// A graphics mode: the area the text is laid out in, in pixels, and the text size in percent.
    Graphics { width: u32, height: u32, scale: u32 },
}

/// How the current mode was set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    Text,
    /// A VESA mode of the video BIOS, by number.
    Vbe(u16),
    /// Directly through the Bochs display interface.
    Direct,
}

/// The current display mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mode {
    pub request: Request,
    /// The text grid.
    pub cols: usize,
    pub rows: usize,
    pub how: How,
    /// The video mode the hardware runs: its size in pixels and bits per pixel.
    pub hw_width: u32,
    pub hw_height: u32,
    pub bpp: u32,
}

impl Mode {
    const TEXT: Mode = Mode {
        request: Request::Text { rows50: false },
        cols: 80,
        rows: 25,
        how: How::Text,
        hw_width: 720,
        hw_height: 400,
        bpp: 4,
    };

    /// One line describing the mode, e.g. "1920x1080 at 150%: 160x45 characters (VESA mode
    /// 0x118, 32 bits per pixel)".
    pub fn describe(&self) -> String {
        match self.request {
            Request::Text { .. } => format!("VGA text mode, {}x{} characters", self.cols, self.rows),
            Request::Graphics { width, height, scale } => {
                let mut text = format!("{width}x{height} at {scale}%: {}x{} characters", self.cols, self.rows);
                if (self.hw_width, self.hw_height) != (width, height) {
                    text.push_str(&format!(", framed in {}x{}", self.hw_width, self.hw_height));
                }
                let how = match self.how {
                    How::Vbe(number) => format!("VESA mode {number:#x}"),
                    _ => String::from("set directly"),
                };
                text.push_str(&format!(" ({how}, {} bits per pixel)", self.bpp));
                text
            }
        }
    }
}

/// The pixel size of a character cell at `scale` percent.
pub fn cell_size(scale: u32) -> (usize, usize) {
    let size = |base: usize| (base * scale as usize + 50) / 100;
    (size(FONT_WIDTH), size(FONT_HEIGHT))
}

/// The text grid of `width` x `height` pixels at `scale`, or why it cannot be used.
pub fn grid(width: u32, height: u32, scale: u32) -> Result<(usize, usize), String> {
    if !(MIN_SCALE..=MAX_SCALE).contains(&scale) {
        return Err(format!("the scale must be {MIN_SCALE} to {MAX_SCALE} (percent)"));
    }
    let (cell_w, cell_h) = cell_size(scale);
    let (cols, rows) = (width as usize / cell_w, height as usize / cell_h);
    if cols < MIN_COLS || rows < MIN_ROWS {
        return Err(format!(
            "at {scale}% the text would be only {cols}x{rows} characters, and {MIN_COLS}x{MIN_ROWS} is the least: use a smaller scale"
        ));
    }
    if cols > MAX_COLS || rows > MAX_ROWS {
        return Err(format!(
            "at {scale}% the text would be {cols}x{rows} characters, and {MAX_COLS}x{MAX_ROWS} is the most: use a larger scale"
        ));
    }
    Ok((cols, rows))
}

/// A framebuffer and how text is laid out in it.
pub struct Fb {
    /// Address of the top left pixel of the mode.
    base: usize,
    /// Bytes from one line of pixels to the next.
    pitch: usize,
    bytes_pp: usize,
    width: usize,
    height: usize,
    format: PixelFormat,
    /// Where the cell at row 0, column 0 starts, in pixels.
    left: usize,
    top: usize,
    pub cell_w: usize,
    pub cell_h: usize,
    /// The text grid that fits.
    cols: usize,
    rows: usize,
    /// The font column (0-7) of each pixel column of a cell, and the font row of each pixel row:
    /// scaling by repeating pixels.
    col_map: [u8; 64],
    row_map: [u8; 128],
    /// The 16 colors as pixel values.
    colors: [u32; 16],
}

impl Fb {
    /// Text area `area` (in pixels) at `scale`, centered in a mode of `width` x `height`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base: usize,
        pitch: usize,
        bpp: u32,
        width: u32,
        height: u32,
        format: PixelFormat,
        area: (u32, u32),
        scale: u32,
        palette: &[[u8; 3]; 16],
    ) -> Fb {
        let (cell_w, cell_h) = cell_size(scale);
        let (width, height) = (width as usize, height as usize);
        let (area_w, area_h) = ((area.0 as usize).min(width), (area.1 as usize).min(height));
        let (cols, rows) = (area_w / cell_w, area_h / cell_h);
        let mut fb = Fb {
            base,
            pitch,
            bytes_pp: bpp.div_ceil(8) as usize,
            width,
            height,
            format,
            left: (width - area_w) / 2 + (area_w - cols * cell_w) / 2,
            top: (height - area_h) / 2 + (area_h - rows * cell_h) / 2,
            cell_w,
            cell_h,
            cols,
            rows,
            col_map: core::array::from_fn(|x| (x * FONT_WIDTH / cell_w).min(FONT_WIDTH - 1) as u8),
            row_map: core::array::from_fn(|y| (y * FONT_HEIGHT / cell_h).min(FONT_HEIGHT - 1) as u8),
            colors: [0; 16],
        };
        fb.set_colors(palette);
        fb
    }

    pub fn set_colors(&mut self, palette: &[[u8; 3]; 16]) {
        self.colors = core::array::from_fn(|i| self.format.pixel(palette[i]));
    }

    pub fn color(&self, index: usize) -> u32 {
        self.colors[index & 0x0F]
    }

    unsafe fn write(&self, addr: usize, value: u32) {
        unsafe {
            match self.bytes_pp {
                4 => write_volatile(addr as *mut u32, value),
                3 => {
                    write_volatile(addr as *mut u8, value as u8);
                    write_volatile((addr + 1) as *mut u8, (value >> 8) as u8);
                    write_volatile((addr + 2) as *mut u8, (value >> 16) as u8);
                }
                _ => write_volatile(addr as *mut u16, value as u16),
            }
        }
    }

    unsafe fn read(&self, addr: usize) -> u32 {
        unsafe {
            match self.bytes_pp {
                4 => read_volatile(addr as *const u32),
                3 => {
                    read_volatile(addr as *const u8) as u32
                        | (read_volatile((addr + 1) as *const u8) as u32) << 8
                        | (read_volatile((addr + 2) as *const u8) as u32) << 16
                }
                _ => read_volatile(addr as *const u16) as u32,
            }
        }
    }

    fn addr(&self, x: usize, y: usize) -> usize {
        self.base + y * self.pitch + x * self.bytes_pp
    }

    /// The text area in framebuffer pixels: left, top, width, height.
    fn area(&self) -> (usize, usize, usize, usize) {
        (self.left, self.top, self.cols * self.cell_w, self.rows * self.cell_h)
    }

    /// Fills the whole mode with color `index`.
    pub fn fill(&self, index: usize) {
        let value = self.color(index);
        for y in 0..self.height {
            let line = self.base + y * self.pitch;
            for x in 0..self.width {
                unsafe { self.write(line + x * self.bytes_pp, value) };
            }
        }
    }

    /// Draws one cell: the glyph of the character byte in the colors of the color byte, with the
    /// cursor on it if `cursor` says how.
    pub fn draw_cell(&self, font: &[u8; 4096], row: usize, col: usize, cell: u16, cursor: Option<CursorShape>) {
        let attr = (cell >> 8) as usize;
        let (mut fg, mut bg) = (self.color(attr), self.color(attr >> 4));
        if cursor == Some(CursorShape::Block) {
            core::mem::swap(&mut fg, &mut bg);
        }
        let underline = match cursor {
            Some(CursorShape::Underline) => self.cell_h - (self.cell_h / 8).max(2),
            _ => usize::MAX,
        };
        let glyph = &font[(cell as u8) as usize * FONT_HEIGHT..][..FONT_HEIGHT];
        let x0 = self.left + col * self.cell_w;
        let y0 = self.top + row * self.cell_h;
        if x0 + self.cell_w > self.width || y0 + self.cell_h > self.height {
            return;
        }
        for y in 0..self.cell_h {
            let bits = if y >= underline { 0xFF } else { glyph[self.row_map[y] as usize] };
            let line = self.base + (y0 + y) * self.pitch + x0 * self.bytes_pp;
            for x in 0..self.cell_w {
                let on = bits & (0x80 >> self.col_map[x]) != 0;
                unsafe { self.write(line + x * self.bytes_pp, if on { fg } else { bg }) };
            }
        }
    }

    /// The pixel at `x`, `y` (32 bits per pixel only; for the self-tests).
    pub fn pixel(&self, x: usize, y: usize) -> u32 {
        unsafe { read_volatile((self.base + y * self.pitch + x * self.bytes_pp) as *const u32) }
    }

    /// Where the cell at `row`, `col` starts, in pixels.
    pub fn cell_origin(&self, row: usize, col: usize) -> (usize, usize) {
        (self.left + col * self.cell_w, self.top + row * self.cell_h)
    }
}

/// How a resolution would be shown on this video card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plan {
    /// A VESA mode of the video BIOS: exactly that size, or larger (framed).
    Vbe(ModeInfo),
    /// The Bochs display interface, at this size (the width rounded up to a multiple of 8).
    Direct { width: u32, height: u32 },
}

impl Plan {
    /// The size of the mode the hardware would run.
    pub fn size(&self) -> (u32, u32) {
        match *self {
            Plan::Vbe(m) => (m.width, m.height),
            Plan::Direct { width, height } => (width, height),
        }
    }
}

/// The mouse pointer of graphics modes, 12x19 at 100%: `B` black outline, `W` white fill, `.`
/// see-through. The hot spot is the top left pixel. Fixed colors, so it shows on any background.
const ARROW: [&[u8; ARROW_W]; ARROW_H] = [
    b"B...........",
    b"BB..........",
    b"BWB.........",
    b"BWWB........",
    b"BWWWB.......",
    b"BWWWWB......",
    b"BWWWWWB.....",
    b"BWWWWWWB....",
    b"BWWWWWWWB...",
    b"BWWWWWWWWB..",
    b"BWWWWWWWWWB.",
    b"BWWWWWWBBBBB",
    b"BWWWBWWB....",
    b"BWWBBWWB....",
    b"BWB..BWWB...",
    b"BB...BWWB...",
    b"B.....BWWB..",
    b"......BWWB..",
    b".......BB...",
];
const ARROW_W: usize = 12;
const ARROW_H: usize = 19;

/// A cell of `drawn` whose pixels were drawn over (`pixel_span`): it is drawn again whatever it
/// holds next.
const NOT_DRAWN: u32 = u32::MAX;

/// The mouse pointer (`mouse.rs` decides where and whether): drawn on top of everything, never
/// part of a screen and never sent to serial. Every drawing below takes it off first where it
/// overlaps and puts it back after, so it leaves nothing behind.
struct Pointer {
    /// Where it should be, in area pixels (see `area_px`), if shown.
    want: Option<(usize, usize)>,
    /// Text mode: highlight the cell (false over paint's canvas, whose cursor follows the mouse).
    highlight: bool,
    /// Text mode: the colors for a cell whose foreground and background are the same.
    fallback: u8,
    /// Graphics: the box it is drawn in (framebuffer pixels) and the pixel values under it.
    under_box: Option<(usize, usize, usize, usize)>,
    under: Vec<u32>,
    /// Text mode: the highlighted cell and what it really holds.
    cell: Option<(usize, usize, u16)>,
}

/// A text cell with its colors swapped, or in `fallback` colors if swapping changes nothing.
fn highlighted(cell: u16, fallback: u8) -> u16 {
    let attr = (cell >> 8) as u8;
    let swapped = attr.rotate_left(4);
    let attr = if swapped == attr { fallback } else { swapped };
    (cell & 0xFF) | (attr as u16) << 8
}

fn boxes_overlap(a: (usize, usize, usize, usize), b: (usize, usize, usize, usize)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

struct Display {
    mode: Mode,
    fb: Option<Fb>,
    /// Graphics modes: what each cell of the grid shows on the screen (`NOT_DRAWN`: pixels).
    drawn: Vec<u32>,
    /// Graphics modes: the cell the cursor is drawn on.
    cursor: Option<(usize, usize)>,
    shape: CursorShape,
    palette: [[u8; 3]; 16],
    font: [u8; 4096],
    has_font: bool,
    /// The video BIOS's usable graphics modes, once asked for.
    modes: Option<Vec<ModeInfo>>,
    /// Video memory in bytes, from the video BIOS.
    memory: u64,
    vbe_version: u16,
    /// The Bochs display interface's largest size, once asked for (`Some(None)`: there is none).
    dispi: Option<Option<(u32, u32)>>,
    /// The monitor's preferred resolution, once asked for.
    preferred: Option<Option<(u32, u32)>>,
    /// The scale of the last graphics mode, used when a new one is asked for without one.
    scale: u32,
    pointer: Pointer,
}

static DISPLAY: SpinLock<Display> = SpinLock::new(Display {
    mode: Mode::TEXT,
    fb: None,
    drawn: Vec::new(),
    cursor: None,
    shape: CursorShape::Underline,
    palette: DEFAULT_PALETTE,
    font: [0; 4096],
    has_font: false,
    modes: None,
    memory: 0,
    vbe_version: 0,
    dispi: None,
    preferred: None,
    scale: MIN_SCALE,
    pointer: Pointer { want: None, highlight: true, fallback: 0x70, under_box: None, under: Vec::new(), cell: None },
});

/// Counts changes of the display mode and of the looks, so full-screen code (the editors, the
/// desktop) knows when to draw everything again.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Shift was held when the system started: the saved mode is not used.
static SHIFT_AT_BOOT: AtomicBool = AtomicBool::new(false);

impl Display {
    fn text_rows50(&self) -> bool {
        self.mode.request == Request::Text { rows50: true }
    }

    /// Bright background colors instead of blinking, and the palette, in VGA text mode.
    fn text_setup(&self) {
        unsafe {
            inb(INPUT_STATUS);
            // Mode control register (index 0x10), keeping the screen on (0x20).
            outb(ATTR_INDEX, 0x30);
            let control = inb(ATTR_READ);
            outb(ATTR_INDEX, control & !0x08);
        }
        self.write_dac();
    }

    fn write_dac(&self) {
        for (i, rgb) in self.palette.iter().enumerate() {
            unsafe {
                outb(DAC_WRITE_INDEX, TEXT_DAC_INDEX[i]);
                for &part in rgb {
                    outb(DAC_DATA, part >> 2);
                }
            }
        }
    }

    /// Shows one cell, without looking at the pointer in graphics modes (the caller has taken it
    /// off if it overlaps). In text mode the highlighted cell keeps its highlight.
    fn put_raw(&mut self, row: usize, col: usize, cell: u16) {
        let (cols, rows) = (self.mode.cols, self.mode.rows);
        if row >= rows || col >= cols {
            return;
        }
        let Some(fb) = &self.fb else {
            let fallback = self.pointer.fallback;
            let shown = match &mut self.pointer.cell {
                Some((r, c, real)) if (*r, *c) == (row, col) => {
                    *real = cell;
                    highlighted(cell, fallback)
                }
                _ => cell,
            };
            unsafe { write_volatile(TEXT_BUFFER.add(row * cols + col), shown) };
            return;
        };
        let at = row * cols + col;
        if self.drawn[at] == cell as u32 {
            return;
        }
        self.drawn[at] = cell as u32;
        let cursor = (self.cursor == Some((row, col))).then_some(self.shape);
        fb.draw_cell(&self.font, row, col, cell, cursor);
    }

    /// The pixel box of the cells at `row`, columns `col..col + n` (framebuffer pixels).
    fn cells_box(&self, row: usize, col: usize, n: usize) -> Option<(usize, usize, usize, usize)> {
        let fb = self.fb.as_ref()?;
        let (x, y) = fb.cell_origin(row, col);
        Some((x, y, n * fb.cell_w, fb.cell_h))
    }

    /// Graphics modes: takes the pointer off if it overlaps `bounds`. True if it did (then
    /// `show_pointer` puts it back).
    fn lift_pointer(&mut self, bounds: Option<(usize, usize, usize, usize)>) -> bool {
        match (self.pointer.under_box, bounds) {
            (Some(drawn), Some(bounds)) if boxes_overlap(drawn, bounds) => {
                self.hide_pointer();
                true
            }
            _ => false,
        }
    }

    fn put(&mut self, row: usize, col: usize, cell: u16) {
        if self.fb.is_some() && self.drawn.get(row * self.mode.cols + col) == Some(&(cell as u32)) {
            return;
        }
        let lifted = self.lift_pointer(self.cells_box(row, col, 1));
        self.put_raw(row, col, cell);
        if lifted {
            self.show_pointer();
        }
    }

    fn put_row(&mut self, row: usize, cells: &[u16]) {
        let lifted = self.lift_pointer(self.cells_box(row, 0, cells.len()));
        for (col, &cell) in cells.iter().enumerate() {
            self.put_raw(row, col, cell);
        }
        if lifted {
            self.show_pointer();
        }
    }

    // --- The pointer ---

    /// The size of the area the pointer moves in: the text area in pixels, or in text mode a
    /// virtual 8x16 pixels per cell.
    fn area_px(&self) -> (usize, usize) {
        let (cell_w, cell_h) = self.cell_px();
        (self.mode.cols * cell_w, self.mode.rows * cell_h)
    }

    fn cell_px(&self) -> (usize, usize) {
        self.fb.as_ref().map_or((FONT_WIDTH, FONT_HEIGHT), |fb| (fb.cell_w, fb.cell_h))
    }

    /// Graphics modes: where the arrow goes (framebuffer pixels), clipped to the text area.
    fn arrow_box(&self) -> Option<(usize, usize, usize, usize)> {
        let fb = self.fb.as_ref()?;
        let (x, y) = self.pointer.want?;
        let (left, top, w, h) = fb.area();
        if x >= w || y >= h {
            return None;
        }
        let size = self.arrow_size();
        Some((left + x, top + y, (ARROW_W * size).min(w - x), (ARROW_H * size).min(h - y)))
    }

    /// How many pixels each arrow pixel takes: the text scale / 100, 1 to 8.
    fn arrow_size(&self) -> usize {
        match self.mode.request {
            Request::Graphics { scale, .. } => (scale / 100).clamp(1, 8) as usize,
            Request::Text { .. } => 1,
        }
    }

    /// Takes the pointer off the screen, putting back what was under it.
    fn hide_pointer(&mut self) {
        if let Some((x0, y0, w, h)) = self.pointer.under_box.take() {
            if let Some(fb) = &self.fb {
                for y in 0..h {
                    for x in 0..w {
                        unsafe { fb.write(fb.addr(x0 + x, y0 + y), self.pointer.under[y * w + x]) };
                    }
                }
            }
        }
        if let Some((row, col, real)) = self.pointer.cell.take() {
            if self.fb.is_none() && row < self.mode.rows && col < self.mode.cols {
                unsafe { write_volatile(TEXT_BUFFER.add(row * self.mode.cols + col), real) };
            }
        }
    }

    /// Draws the pointer where it is wanted (if it is not drawn already).
    fn show_pointer(&mut self) {
        if self.pointer.under_box.is_some() || self.pointer.cell.is_some() {
            return;
        }
        let Some((x, y)) = self.pointer.want else { return };
        if self.fb.is_none() {
            let (row, col) = (y / FONT_HEIGHT, x / FONT_WIDTH);
            if !self.pointer.highlight || row >= self.mode.rows || col >= self.mode.cols {
                return;
            }
            let at = unsafe { TEXT_BUFFER.add(row * self.mode.cols + col) };
            let real = unsafe { read_volatile(at) };
            unsafe { write_volatile(at, highlighted(real, self.pointer.fallback)) };
            self.pointer.cell = Some((row, col, real));
            return;
        }
        let Some((x0, y0, w, h)) = self.arrow_box() else { return };
        let size = self.arrow_size();
        let Some(fb) = &self.fb else { return };
        let (black, white) = (fb.format.pixel([0, 0, 0]), fb.format.pixel([0xFF, 0xFF, 0xFF]));
        let mut under = core::mem::take(&mut self.pointer.under);
        under.clear();
        under.reserve(w * h);
        for y in 0..h {
            let bits = ARROW[y / size];
            for x in 0..w {
                let addr = fb.addr(x0 + x, y0 + y);
                under.push(unsafe { fb.read(addr) });
                match bits[x / size] {
                    b'B' => unsafe { fb.write(addr, black) },
                    b'W' => unsafe { fb.write(addr, white) },
                    _ => {}
                }
            }
        }
        self.pointer.under = under;
        self.pointer.under_box = Some((x0, y0, w, h));
    }

    /// After a mode change: the old pointer went with the old picture; keep it in the new area.
    fn reset_pointer(&mut self) {
        self.pointer.under_box = None;
        self.pointer.cell = None;
        if let Some((x, y)) = self.pointer.want {
            let (w, h) = self.area_px();
            self.pointer.want = Some((x.min(w.saturating_sub(1)), y.min(h.saturating_sub(1))));
        }
    }

    // --- Pixels ---

    /// Graphics modes: `n` pixels from (x, y) (area pixels), colors from `rgb(i)` (0x00RRGGBB),
    /// clipped to the text area. The cells drawn over are marked so they are drawn again next.
    fn pixel_span(&mut self, x: usize, y: usize, n: usize, rgb: impl Fn(usize) -> u32) {
        let Some(fb) = &self.fb else { return };
        let (left, top, w, h) = fb.area();
        if x >= w || y >= h {
            return;
        }
        let n = n.min(w - x);
        if n == 0 {
            return;
        }
        let (cell_w, cell_h) = (fb.cell_w, fb.cell_h);
        let lifted = self.lift_pointer(Some((left + x, top + y, n, 1)));
        let Some(fb) = &self.fb else { return };
        let line = fb.addr(left + x, top + y);
        let mut last = u32::MAX;
        let mut value = 0;
        for i in 0..n {
            let color = rgb(i) & 0x00FF_FFFF;
            if color != last {
                last = color;
                value = fb.format.pixel([(color >> 16) as u8, (color >> 8) as u8, color as u8]);
            }
            unsafe { fb.write(line + i * fb.bytes_pp, value) };
        }
        let row = y / cell_h;
        let cols = self.mode.cols;
        for col in x / cell_w..=((x + n - 1) / cell_w).min(cols - 1) {
            if let Some(d) = self.drawn.get_mut(row * cols + col) {
                *d = NOT_DRAWN;
            }
        }
        if lifted {
            self.show_pointer();
        }
    }

    fn cursor(&mut self, row: usize, col: usize, visible: bool) {
        let (cols, rows) = (self.mode.cols, self.mode.rows);
        if self.fb.is_none() {
            let height = if self.text_rows50() { 8 } else { 16 };
            let (start, end) = match self.shape {
                CursorShape::Underline => (height - 2, height - 1),
                CursorShape::Block => (0, height - 1),
            };
            let pos = (row.min(rows - 1) * cols + col.min(cols - 1)) as u16;
            unsafe {
                outb(CRTC_INDEX, CRTC_CURSOR_START);
                let keep = inb(CRTC_DATA) & 0xC0;
                outb(CRTC_DATA, if visible { keep | start } else { keep | CURSOR_DISABLE });
                outb(CRTC_INDEX, CRTC_CURSOR_END);
                outb(CRTC_DATA, (inb(CRTC_DATA) & 0xE0) | end);
                outb(CRTC_INDEX, CRTC_CURSOR_LOW);
                outb(CRTC_DATA, pos as u8);
                outb(CRTC_INDEX, CRTC_CURSOR_HIGH);
                outb(CRTC_DATA, (pos >> 8) as u8);
            }
            return;
        }
        let new = (visible && row < rows && col < cols).then_some((row, col));
        if new == self.cursor {
            return;
        }
        let old = core::mem::replace(&mut self.cursor, new);
        let under = self.pointer.under_box;
        let lifted = [old, new]
            .into_iter()
            .flatten()
            .any(|(r, c)| under.zip(self.cells_box(r, c, 1)).is_some_and(|(a, b)| boxes_overlap(a, b)));
        if lifted {
            self.hide_pointer();
        }
        if let Some(fb) = &self.fb {
            // A cell drawn over by pixels shows them until it is drawn again: leave it alone.
            let drawn = |(r, c): (usize, usize)| Some((r, c, self.drawn[r * cols + c])).filter(|d| d.2 != NOT_DRAWN);
            if let Some((r, c, cell)) = old.and_then(drawn) {
                fb.draw_cell(&self.font, r, c, cell as u16, None);
            }
            if let Some((r, c, cell)) = new.and_then(drawn) {
                fb.draw_cell(&self.font, r, c, cell as u16, Some(self.shape));
            }
        }
        if lifted {
            self.show_pointer();
        }
    }

    /// Graphics modes: clears the framebuffer, so everything is drawn again.
    fn invalidate(&mut self) {
        if let Some(fb) = &self.fb {
            fb.fill(0);
            self.drawn.fill(0);
            self.cursor = None;
            self.pointer.under_box = None;
            self.show_pointer();
        }
    }

    fn modes(&mut self) -> Vec<ModeInfo> {
        if self.modes.is_none() {
            let mut list = Vec::new();
            if let Some(controller) = vbe::controller() {
                self.memory = controller.memory;
                self.vbe_version = controller.version;
                list.extend(controller.modes.iter().filter_map(|&n| vbe::mode_info(n, controller.version)));
                list.sort_by_key(|m| (m.width, m.height, m.bpp));
                list.dedup_by_key(|m| (m.width, m.height, m.bpp));
            }
            self.modes = Some(list);
        }
        self.modes.clone().unwrap_or_default()
    }

    fn dispi(&mut self) -> Option<(u32, u32)> {
        *self.dispi.get_or_insert_with(vbe::dispi_limits)
    }

    fn plan(&mut self, width: u32, height: u32) -> Result<Plan, String> {
        let rank = |m: &&ModeInfo| match m.bpp {
            32 => 0,
            24 => 1,
            16 => 2,
            _ => 3,
        };
        let modes = self.modes();
        if let Some(m) = modes.iter().filter(|m| (m.width, m.height) == (width, height)).min_by_key(rank) {
            return Ok(Plan::Vbe(*m));
        }
        if !modes.is_empty() {
            if let Some((max_w, max_h)) = self.dispi() {
                let w = width.next_multiple_of(8);
                if w <= max_w && height <= max_h && w as u64 * height as u64 * 4 <= self.memory {
                    return Ok(Plan::Direct { width: w, height });
                }
            }
        }
        let bigger = modes
            .iter()
            .filter(|m| m.width >= width && m.height >= height)
            .min_by_key(|m| (m.width as u64 * m.height as u64, rank(m)));
        match (bigger, modes.iter().max_by_key(|m| m.width as u64 * m.height as u64)) {
            (Some(m), _) => Ok(Plan::Vbe(*m)),
            (None, Some(largest)) => Err(format!(
                "this video card has no mode of {width}x{height} or larger (the largest is {}x{})",
                largest.width, largest.height
            )),
            (None, None) => Err(String::from(
                "this video card has no VESA graphics modes with a linear framebuffer (VBE 2.0 or later)",
            )),
        }
    }

    fn set_text(&mut self, rows50: bool) {
        if self.mode.how == How::Direct {
            vbe::dispi_disable();
        }
        vbe::set_text_mode(rows50);
        self.fb = None;
        self.drawn = Vec::new();
        self.cursor = None;
        self.mode = Mode { request: Request::Text { rows50 }, rows: if rows50 { 50 } else { 25 }, ..Mode::TEXT };
        self.text_setup();
        self.reset_pointer();
        self.show_pointer();
    }

    /// Switches to a graphics mode. On failure: the message, and whether the hardware was
    /// changed (then the old mode has to be set again).
    fn set_graphics(&mut self, width: u32, height: u32, scale: u32) -> Result<(), (String, bool)> {
        if !self.has_font {
            return Err((String::from("the video BIOS gave no font, so graphics modes cannot show text"), false));
        }
        let (cols, rows) = grid(width, height, scale).map_err(|e| (e, false))?;
        let plan = self.plan(width, height).map_err(|e| (e, false))?;
        let any_base = self.modes().first().map(|m| m.base);
        let (base, pitch, bpp, hw_width, hw_height, format, how) = match plan {
            Plan::Vbe(m) => {
                if self.mode.how == How::Direct {
                    vbe::dispi_disable();
                }
                if !vbe::set_mode(m.number) {
                    return Err((format!("the video BIOS did not switch to mode {:#x}", m.number), true));
                }
                (m.base, m.pitch, m.bpp, m.width, m.height, m.format, How::Vbe(m.number))
            }
            Plan::Direct { width: w, height: h } => match (any_base, vbe::dispi_set(w, h)) {
                (Some(base), Some((got_w, got_h, pitch))) if got_w >= width && got_h >= height => {
                    (base, pitch, 32, got_w, got_h, PixelFormat::RGB32, How::Direct)
                }
                _ => {
                    vbe::dispi_disable();
                    return Err((format!("the display did not take {w}x{h}"), true));
                }
            },
        };
        crate::paging::set_write_combining(base as u64, pitch as u64 * hw_height as u64);
        let fb = Fb::new(base as usize, pitch as usize, bpp, hw_width, hw_height, format, (width, height), scale, &self.palette);
        fb.fill(0);
        self.fb = Some(fb);
        self.drawn = vec![0; cols * rows];
        self.cursor = None;
        self.mode = Mode { request: Request::Graphics { width, height, scale }, cols, rows, how, hw_width, hw_height, bpp };
        self.scale = scale;
        self.reset_pointer();
        self.show_pointer();
        Ok(())
    }

    fn apply(&mut self, request: Request) -> Result<(), (String, bool)> {
        match request {
            Request::Text { rows50 } => {
                self.set_text(rows50);
                Ok(())
            }
            Request::Graphics { width, height, scale } => self.set_graphics(width, height, scale),
        }
    }
}

/// At boot, after `bios::init`: gets the BIOS font and sets up VGA text mode's colors.
pub fn init() {
    let font = vbe::font_8x16();
    let mut d = DISPLAY.lock();
    if let Some(font) = font {
        d.font = font;
        d.has_font = true;
    }
    d.text_setup();
}

/// At the very start, before the kernel takes over the keyboard: remembers whether Shift is held
/// (the BIOS keeps the shift keys' state at 0x417).
pub fn note_boot_keys() {
    let flags = unsafe { read_volatile(0x417 as *const u8) };
    SHIFT_AT_BOOT.store(flags & 0b11 != 0, Ordering::Relaxed);
}

pub fn shift_at_boot() -> bool {
    SHIFT_AT_BOOT.load(Ordering::Relaxed)
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Something about the looks changed: full-screen code draws everything again.
pub fn touch() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

pub fn mode() -> Mode {
    DISPLAY.lock().mode
}

/// The text grid: (columns, rows).
pub fn size() -> (usize, usize) {
    let d = DISPLAY.lock();
    (d.mode.cols, d.mode.rows)
}

/// The scale of the last graphics mode (100 before there was one).
pub fn last_scale() -> u32 {
    DISPLAY.lock().scale
}

/// Graphics modes with 32 bits per pixel: the pixel at `dx`, `dy` inside the cell at `row`,
/// `col` (for the self-tests).
#[cfg_attr(not(feature = "qemu-test"), allow(dead_code))]
pub fn pixel_in_cell(row: usize, col: usize, dx: usize, dy: usize) -> Option<u32> {
    let d = DISPLAY.lock();
    let fb = d.fb.as_ref().filter(|_| d.mode.bpp == 32)?;
    let (x, y) = fb.cell_origin(row, col);
    Some(fb.pixel(x + dx, y + dy))
}

/// The pixel value color `index` has in the current graphics mode (for the self-tests).
#[cfg_attr(not(feature = "qemu-test"), allow(dead_code))]
pub fn color_pixel(index: usize) -> Option<u32> {
    DISPLAY.lock().fb.as_ref().map(|fb| fb.color(index))
}

/// A copy of the font (for the self-tests).
pub fn font() -> Option<[u8; 4096]> {
    let d = DISPLAY.lock();
    d.has_font.then_some(d.font)
}

/// Shows one cell.
pub fn put(row: usize, col: usize, cell: u16) {
    DISPLAY.lock().put(row, col, cell);
}

/// Shows one row of cells.
pub fn put_row(row: usize, cells: &[u16]) {
    DISPLAY.lock().put_row(row, cells);
}

// --- The pixel surface (graphics modes) ---
//
// Coordinates are "area pixels": (0, 0) is the top left pixel of the text grid's cell 0, 0, so a
// framed mode's border is never touched. Tasks draw through `console::draw_pixels`, which only
// lets the task whose screen is shown draw. Cells drawn over are drawn again on the next write
// to them, e.g. when another screen is shown.

/// True in a graphics mode (the pixel surface exists).
pub fn is_graphics() -> bool {
    DISPLAY.lock().fb.is_some()
}

/// The pixel size of a text cell; in text mode the mouse's virtual 8x16.
pub fn cell_px() -> (usize, usize) {
    DISPLAY.lock().cell_px()
}

/// The text area's size in area pixels (in text mode the mouse's virtual grid).
pub fn area_px() -> (usize, usize) {
    DISPLAY.lock().area_px()
}

/// Graphics modes: the pixel rectangle (x, y, width, height) of text rows `first..end`.
#[cfg_attr(not(feature = "qemu-test"), allow(dead_code))]
pub fn rows_rect(first: usize, end: usize) -> Option<(usize, usize, usize, usize)> {
    let d = DISPLAY.lock();
    let fb = d.fb.as_ref()?;
    let end = end.min(d.mode.rows);
    (first < end).then(|| (0, first * fb.cell_h, d.mode.cols * fb.cell_w, (end - first) * fb.cell_h))
}

/// Graphics modes: draws a row of 0x00RRGGBB pixels from (x, y), clipped to the text area.
/// Use `console::draw_pixels`, which checks the caller's screen is shown.
pub fn draw_pixels(x: usize, y: usize, rgb: &[u32]) {
    DISPLAY.lock().pixel_span(x, y, rgb.len(), |i| rgb[i]);
}

/// Graphics modes: fills a rectangle with one 0x00RRGGBB color, clipped to the text area.
/// Use `console::fill_pixels`.
pub fn fill_pixels(x: usize, y: usize, w: usize, h: usize, rgb: u32) {
    for line in y..y.saturating_add(h) {
        DISPLAY.lock().pixel_span(x, line, w, |_| rgb);
    }
}

/// Graphics modes with 32 bits per pixel: the pixel at (x, y) in area pixels, as 0x00RRGGBB
/// (for the self-tests). Sees the pointer if it is there.
#[cfg_attr(not(feature = "qemu-test"), allow(dead_code))]
pub fn area_pixel(x: usize, y: usize) -> Option<u32> {
    let d = DISPLAY.lock();
    let fb = d.fb.as_ref().filter(|_| d.mode.bpp == 32 && d.mode.how != How::Text)?;
    let (left, top, w, h) = fb.area();
    (x < w && y < h).then(|| fb.pixel(left + x, top + y) & 0x00FF_FFFF)
}

// --- The mouse pointer ---

/// Shows the pointer at (x, y) in area pixels, moves it, or hides it (`None`). In text mode the
/// cell under it is highlighted, unless `highlight` is false; `fallback` colors a cell whose
/// foreground and background are the same. Called by `mouse::refresh`.
pub fn set_pointer(want: Option<(usize, usize)>, highlight: bool, fallback: u8) {
    let mut d = DISPLAY.lock();
    let want = want.map(|(x, y)| {
        let (w, h) = d.area_px();
        (x.min(w.saturating_sub(1)), y.min(h.saturating_sub(1)))
    });
    let p = &d.pointer;
    let text_same = d.fb.is_some() || (p.highlight == highlight && p.fallback == fallback);
    if p.want == want && text_same {
        return;
    }
    d.hide_pointer();
    d.pointer.want = want;
    d.pointer.highlight = highlight;
    d.pointer.fallback = fallback;
    d.show_pointer();
}

/// Graphics modes: the box the arrow is drawn in, in area pixels (for the self-tests).
#[cfg_attr(not(feature = "qemu-test"), allow(dead_code))]
pub fn pointer_box() -> Option<(usize, usize, usize, usize)> {
    let d = DISPLAY.lock();
    let (left, top, ..) = d.fb.as_ref()?.area();
    d.pointer.under_box.map(|(x, y, w, h)| (x - left, y - top, w, h))
}

/// Text mode: the highlighted cell, if the pointer shows (for the self-tests).
#[cfg_attr(not(feature = "qemu-test"), allow(dead_code))]
pub fn pointer_cell() -> Option<(usize, usize)> {
    DISPLAY.lock().pointer.cell.map(|(r, c, _)| (r, c))
}

/// Puts the cursor at `row`, `col`, or hides it.
pub fn cursor(row: usize, col: usize, visible: bool) {
    DISPLAY.lock().cursor(row, col, visible);
}

pub fn set_cursor_shape(shape: CursorShape) {
    let mut d = DISPLAY.lock();
    if d.shape == shape {
        return;
    }
    d.shape = shape;
    // Graphics modes: draw the cursor again in its new shape.
    if let Some((row, col)) = d.cursor.take() {
        d.cursor(row, col, true);
    }
}

/// What VGA text memory shows at `row`, `col` (for the boot screen, before the heap).
pub fn text_cell(row: usize, col: usize) -> u16 {
    let d = DISPLAY.lock();
    if d.fb.is_some() || row >= d.mode.rows || col >= d.mode.cols {
        return crate::vga::cell(b' ', crate::vga::DEFAULT_ATTR);
    }
    match d.pointer.cell {
        Some((r, c, real)) if (r, c) == (row, col) => real,
        _ => unsafe { read_volatile(TEXT_BUFFER.add(row * d.mode.cols + col)) },
    }
}

/// Scrolls the top `rows` rows of VGA text memory up by one (for the boot screen, before the
/// heap).
pub fn scroll_text(rows: usize, blank: u16) {
    let mut d = DISPLAY.lock();
    if d.fb.is_some() {
        return;
    }
    d.hide_pointer();
    let cols = d.mode.cols;
    let rows = rows.min(d.mode.rows);
    unsafe {
        for i in 0..(rows - 1) * cols {
            write_volatile(TEXT_BUFFER.add(i), read_volatile(TEXT_BUFFER.add(i + cols)));
        }
        for i in (rows - 1) * cols..rows * cols {
            write_volatile(TEXT_BUFFER.add(i), blank);
        }
    }
    d.show_pointer();
}

/// Sets the RGB values of the 16 colors. In a graphics mode a change clears the screen: the
/// caller draws it again.
pub fn set_palette(palette: [[u8; 3]; 16]) {
    let mut guard = DISPLAY.lock();
    let d = &mut *guard;
    if d.fb.is_some() && d.palette == palette {
        return;
    }
    d.palette = palette;
    match d.fb.as_mut() {
        Some(fb) => {
            fb.set_colors(&palette);
            d.invalidate();
        }
        None => d.write_dac(),
    }
}

/// Switches the display mode. If that fails part way, the old mode is set again. Called by
/// `console::set_display`, which then fits every screen to the new size.
pub fn set(request: Request) -> Result<Mode, String> {
    let mut d = DISPLAY.lock();
    let old = d.mode.request;
    let result = match d.apply(request) {
        Ok(()) => Ok(d.mode),
        Err((message, changed)) => {
            if changed && d.apply(old).is_err() {
                d.set_text(false);
            }
            Err(message)
        }
    };
    drop(d);
    touch();
    result
}

/// How `width` x `height` would be shown, or why it cannot be.
pub fn plan(width: u32, height: u32) -> Result<Plan, String> {
    DISPLAY.lock().plan(width, height)
}

/// The video BIOS's usable graphics modes, its VBE version and its video memory in bytes.
pub fn vbe_modes() -> (Vec<ModeInfo>, u16, u64) {
    let mut d = DISPLAY.lock();
    let modes = d.modes();
    (modes, d.vbe_version, d.memory)
}

/// The largest size the Bochs display interface can be set to, if there is one.
pub fn direct_limits() -> Option<(u32, u32)> {
    DISPLAY.lock().dispi()
}

/// The monitor's preferred resolution, if the BIOS can tell.
pub fn preferred_resolution() -> Option<(u32, u32)> {
    let mut d = DISPLAY.lock();
    *d.preferred.get_or_insert_with(vbe::preferred_resolution)
}

pub unsafe fn force_unlock() {
    unsafe { DISPLAY.force_unlock() };
}

// --- Naming modes, and the saved mode ---

/// Short names for common resolutions.
pub const NAMES: &[(&str, u32, u32)] = &[
    ("vga", 640, 480),
    ("svga", 800, 600),
    ("xga", 1024, 768),
    ("720p", 1280, 720),
    ("1080p", 1920, 1080),
    ("1440p", 2560, 1440),
    ("2160p", 3840, 2160),
    ("4k", 3840, 2160),
    ("uw1080", 2560, 1080),
    ("uw1440", 3440, 1440),
];

/// Common resolutions by aspect ratio, for `display -l`.
pub const COMMON: &[(&str, &[(u32, u32)])] = &[
    ("4:3", &[(640, 480), (800, 600), (1024, 768), (1280, 960), (1400, 1050), (1600, 1200)]),
    ("5:4", &[(1280, 1024)]),
    ("16:10", &[(1280, 800), (1440, 900), (1680, 1050), (1920, 1200), (2560, 1600)]),
    ("16:9", &[(1280, 720), (1366, 768), (1600, 900), (1920, 1080), (2560, 1440), (3840, 2160)]),
    ("21:9", &[(2560, 1080), (3440, 1440), (3840, 1600), (5120, 2160)]),
    ("32:9", &[(3840, 1080), (5120, 1440)]),
];

/// `text`, `text50`, `<width>x<height>` or a name from `NAMES`; graphics modes get `scale`.
pub fn parse_mode(name: &str, scale: u32) -> Result<Request, String> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "text" | "text25" | "80x25" => return Ok(Request::Text { rows50: false }),
        "text50" | "80x50" => return Ok(Request::Text { rows50: true }),
        _ => {}
    }
    let (width, height) = match NAMES.iter().find(|(n, ..)| *n == lower) {
        Some(&(_, w, h)) => (w, h),
        None => lower
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
            .ok_or_else(|| format!("not a mode: {name} (try text, text50, 1920x1080 or 1080p)"))?,
    };
    if !(MIN_WIDTH..=MAX_SIZE).contains(&width) || !(MIN_HEIGHT..=MAX_SIZE).contains(&height) {
        return Err(format!(
            "{width}x{height}: a resolution must be {MIN_WIDTH}x{MIN_HEIGHT} to {MAX_SIZE}x{MAX_SIZE}"
        ));
    }
    Ok(Request::Graphics { width, height, scale })
}

/// The name of a mode as `parse_mode` takes it.
pub fn mode_name(request: Request) -> String {
    match request {
        Request::Text { rows50: false } => String::from("text"),
        Request::Text { rows50: true } => String::from("text50"),
        Request::Graphics { width, height, .. } => format!("{width}x{height}"),
    }
}

pub fn config_text(request: Request) -> String {
    let mut text = String::from(
        "# The screen mode used at start-up, written by 'display' (see\n\
         # /docs/commands/screen.txt). Hold Shift while the computer starts to\n\
         # stay in text mode once.\n",
    );
    text.push_str(&format!("mode {}\n", mode_name(request)));
    if let Request::Graphics { scale, .. } = request {
        text.push_str(&format!("scale {scale}\n"));
    }
    text
}

/// Reads `mode <name>` and `scale <percent>` lines; `#` lines are comments.
pub fn parse_config(text: &str) -> Result<Request, String> {
    let mut mode = None;
    let mut scale = MIN_SCALE;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match (key, value.trim()) {
            ("mode", value) => mode = Some(value),
            ("scale", value) => {
                scale = value.trim_end_matches('%').parse().map_err(|_| format!("line {}: not a scale: {value}", number + 1))?
            }
            _ => return Err(format!("line {}: unknown setting: {line}", number + 1)),
        }
    }
    parse_mode(mode.ok_or("no 'mode' line")?, scale)
}

/// Saves `request` as the mode to use at start-up.
pub fn save(request: Request) -> fs::Result<()> {
    let text = config_text(request);
    fs::with(|fs| fs.write(CONFIG_PATH, text.as_bytes()))
}

/// The saved mode, if there is a saved file.
pub fn saved() -> Option<Result<Request, String>> {
    let data = fs::with(|fs| fs.read(CONFIG_PATH)).ok()?;
    Some(parse_config(&String::from_utf8_lossy(&data)))
}
