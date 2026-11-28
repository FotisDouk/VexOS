use super::*;

const VGA: [[u8; 3]; 16] = [
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

fn rgb_of(c: [u8; 3]) -> u32 {
    (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32
}

fn pat(x: usize, y: usize) -> u32 {
    let r = (x * 7) % 256;
    let g = (y * 36 + 5) % 256;
    let b = (x * y * 3 + 11) % 256;
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// A BMP built by hand.
struct B {
    w: i32,
    h: i32,
    bpp: u16,
    comp: u32,
    clr_used: u32,
    pal: Vec<[u8; 3]>,
    dib: usize,
    masks: Option<[u32; 3]>,
    planes: u16,
    /// Rows, top row first, without padding. `None`: zeros of the right size.
    rows: Option<Vec<Vec<u8>>>,
}

impl B {
    fn new(w: i32, h: i32, bpp: u16) -> B {
        B {
            w,
            h,
            bpp,
            comp: 0,
            clr_used: 0,
            pal: Vec::new(),
            dib: 40,
            masks: None,
            planes: 1,
            rows: None,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let top_down = self.h < 0;
        let height = self.h.unsigned_abs() as usize;
        let row_len = ((self.w.max(0) as usize * self.bpp as usize + 31) / 32) * 4;
        let mut pix: Vec<u8> = Vec::new();
        match &self.rows {
            Some(rows) => {
                let order: Vec<&Vec<u8>> =
                    if top_down { rows.iter().collect() } else { rows.iter().rev().collect() };
                for r in order {
                    let mut r = r.clone();
                    r.resize(row_len, 0);
                    pix.extend_from_slice(&r);
                }
            }
            None => {
                if row_len * height <= 1 << 20 {
                    pix.resize(row_len * height, 0);
                }
            }
        }
        let mut head = Vec::new();
        head.extend_from_slice(&(self.dib as u32).to_le_bytes());
        head.extend_from_slice(&self.w.to_le_bytes());
        head.extend_from_slice(&self.h.to_le_bytes());
        head.extend_from_slice(&self.planes.to_le_bytes());
        head.extend_from_slice(&self.bpp.to_le_bytes());
        head.extend_from_slice(&self.comp.to_le_bytes());
        head.extend_from_slice(&(pix.len() as u32).to_le_bytes());
        head.extend_from_slice(&2835u32.to_le_bytes());
        head.extend_from_slice(&2835u32.to_le_bytes());
        head.extend_from_slice(&self.clr_used.to_le_bytes());
        head.extend_from_slice(&0u32.to_le_bytes());
        head.resize(self.dib.max(40), 0);
        let mut extra = Vec::new();
        if let Some(m) = self.masks {
            for v in m {
                extra.extend_from_slice(&v.to_le_bytes());
            }
        }
        let mut pal = Vec::new();
        for c in &self.pal {
            pal.extend_from_slice(&[c[2], c[1], c[0], 0]);
        }
        let off = 14 + head.len() + extra.len() + pal.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&((off + pix.len()) as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(off as u32).to_le_bytes());
        out.extend_from_slice(&head);
        out.extend_from_slice(&extra);
        out.extend_from_slice(&pal);
        out.extend_from_slice(&pix);
        out
    }
}

fn row24(w: usize, y: usize) -> Vec<u8> {
    let mut r = Vec::new();
    for x in 0..w {
        let p = pat(x, y);
        r.extend_from_slice(&[p as u8, (p >> 8) as u8, (p >> 16) as u8]);
    }
    r
}

fn row32(w: usize, y: usize) -> Vec<u8> {
    let mut r = Vec::new();
    for x in 0..w {
        let p = pat(x, y);
        r.extend_from_slice(&[p as u8, (p >> 8) as u8, (p >> 16) as u8, 0x55]);
    }
    r
}

/// Packs palette indexes (1, 4 or 8 bits each) into a row.
fn pack(bpp: usize, idx: &[usize]) -> Vec<u8> {
    let mut r = vec![0u8; (idx.len() * bpp + 7) / 8];
    for (x, &i) in idx.iter().enumerate() {
        let bit = x * bpp;
        r[bit / 8] |= (i as u8) << (8 - bpp - bit % 8);
    }
    r
}

fn assert_pattern(img: &Image, w: usize, h: usize) {
    assert_eq!((img.width as usize, img.height as usize), (w, h));
    for y in 0..h {
        for x in 0..w {
            assert_eq!(img.get(x as i32, y as i32), Some(pat(x, y)), "pixel {},{}", x, y);
        }
    }
}

// ---------------------------------------------------------------- reading

#[test]
fn read_24bit_bottom_up_with_padding() {
    for (w, h) in [(33usize, 7usize), (1, 1), (2, 3), (3, 2), (4, 4), (5, 1)] {
        let mut b = B::new(w as i32, h as i32, 24);
        b.rows = Some((0..h).map(|y| row24(w, y)).collect());
        let img = decode(&b.bytes()).unwrap();
        assert_pattern(&img, w, h);
    }
}

#[test]
fn read_24bit_top_down() {
    let (w, h) = (7usize, 4usize);
    let mut b = B::new(w as i32, -(h as i32), 24);
    b.rows = Some((0..h).map(|y| row24(w, y)).collect());
    let bytes = b.bytes();
    assert!(probe(&bytes).unwrap().top_down);
    assert_pattern(&decode(&bytes).unwrap(), w, h);
}

#[test]
fn read_32bit_rgb_ignores_alpha() {
    let (w, h) = (5usize, 3usize);
    let mut b = B::new(w as i32, h as i32, 32);
    b.rows = Some((0..h).map(|y| row32(w, y)).collect());
    assert_pattern(&decode(&b.bytes()).unwrap(), w, h);
    b.h = -(h as i32);
    assert_pattern(&decode(&b.bytes()).unwrap(), w, h);
}

#[test]
fn read_32bit_bitfields() {
    let (w, h) = (4usize, 3usize);
    let mut b = B::new(w as i32, h as i32, 32);
    b.comp = 3;
    b.masks = Some([0x00FF_0000, 0x0000_FF00, 0x0000_00FF]);
    b.rows = Some((0..h).map(|y| row32(w, y)).collect());
    assert_pattern(&decode(&b.bytes()).unwrap(), w, h);
    // the masks may also sit inside a bigger header (V4: 108 bytes)
    let mut b = B::new(w as i32, h as i32, 32);
    b.comp = 3;
    b.dib = 108;
    b.rows = Some((0..h).map(|y| row32(w, y)).collect());
    let mut bytes = b.bytes();
    bytes[54..58].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
    bytes[58..62].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
    bytes[62..66].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
    assert_pattern(&decode(&bytes).unwrap(), w, h);
}

#[test]
fn bitfields_with_other_masks_is_unsupported() {
    for masks in [[0xFF, 0xFF00, 0xFF_0000], [0xF800, 0x7E0, 0x1F], [0x00FF_0000, 0xFF00, 0]] {
        let mut b = B::new(2, 2, 32);
        b.comp = 3;
        b.masks = Some(masks);
        assert_eq!(decode(&b.bytes()), Err(Error::Unsupported));
    }
    // BI_BITFIELDS for anything but 32 bits
    let mut b = B::new(2, 2, 24);
    b.comp = 3;
    b.masks = Some([0x00FF_0000, 0xFF00, 0xFF]);
    assert_eq!(decode(&b.bytes()), Err(Error::Unsupported));
}

#[test]
fn read_v5_header() {
    let (w, h) = (5usize, 3usize);
    let mut b = B::new(w as i32, h as i32, 24);
    b.dib = 124;
    b.rows = Some((0..h).map(|y| row24(w, y)).collect());
    let bytes = b.bytes();
    assert_eq!(u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]), 124);
    assert_pattern(&decode(&bytes).unwrap(), w, h);
}

#[test]
fn read_v5_header_with_palette() {
    // the palette starts after the whole header, not after 40 bytes
    let mut b = B::new(2, 1, 8);
    b.dib = 124;
    b.pal = vec![[10, 20, 30], [40, 50, 60]];
    b.clr_used = 2;
    b.rows = Some(vec![vec![1, 0]]);
    let img = decode(&b.bytes()).unwrap();
    assert_eq!(img.pixels, vec![0x28_32_3C, 0x0A_14_1E]);
}

#[test]
fn read_1bit() {
    let (w, h) = (21usize, 6usize);
    let pal = vec![[255, 0, 0], [0, 0, 255]];
    let f = |x: usize, y: usize| (x / 2 + y) % 3 == 0;
    for top_down in [false, true] {
        let mut b = B::new(w as i32, if top_down { -(h as i32) } else { h as i32 }, 1);
        b.pal = pal.clone();
        b.rows = Some((0..h).map(|y| pack(1, &(0..w).map(|x| f(x, y) as usize).collect::<Vec<_>>())).collect());
        let img = decode(&b.bytes()).unwrap();
        for y in 0..h {
            for x in 0..w {
                let want = if f(x, y) { 0x0000FF } else { 0xFF0000 };
                assert_eq!(img.get(x as i32, y as i32), Some(want));
            }
        }
    }
}

#[test]
fn read_4bit() {
    let (w, h) = (9usize, 5usize);
    let idx = |x: usize, y: usize| (x * 3 + y) % 16;
    let mut b = B::new(w as i32, h as i32, 4);
    b.pal = VGA.to_vec();
    b.rows = Some((0..h).map(|y| pack(4, &(0..w).map(|x| idx(x, y)).collect::<Vec<_>>())).collect());
    let img = decode(&b.bytes()).unwrap();
    for y in 0..h {
        for x in 0..w {
            assert_eq!(img.get(x as i32, y as i32), Some(rgb_of(VGA[idx(x, y)])));
        }
    }
}

#[test]
fn read_8bit_and_palette_edges() {
    let (w, h) = (6usize, 3usize);
    let pal = vec![[255, 0, 0], [0, 255, 0], [0, 0, 255]];
    let idx = |x: usize, y: usize| (x + y) % 3;
    let mut b = B::new(w as i32, h as i32, 8);
    b.pal = pal.clone();
    b.clr_used = 3;
    b.rows = Some((0..h).map(|y| (0..w).map(|x| idx(x, y) as u8).collect()).collect());
    let img = decode(&b.bytes()).unwrap();
    for y in 0..h {
        for x in 0..w {
            assert_eq!(img.get(x as i32, y as i32), Some(rgb_of(pal[idx(x, y)])));
        }
    }
    // an index beyond the palette is black
    let mut b = B::new(3, 1, 8);
    b.pal = pal;
    b.clr_used = 3;
    b.rows = Some(vec![vec![2, 3, 255]]);
    assert_eq!(decode(&b.bytes()).unwrap().pixels, vec![0x0000FF, 0, 0]);
    // a 4-bit palette of 2 colours: indexes 2..15 are black
    let mut b = B::new(4, 1, 4);
    b.pal = vec![[1, 2, 3], [4, 5, 6]];
    b.clr_used = 2;
    b.rows = Some(vec![pack(4, &[0, 1, 2, 15])]);
    assert_eq!(decode(&b.bytes()).unwrap().pixels, vec![0x010203, 0x040506, 0, 0]);
    // clrused larger than 2^bpp is clamped
    let mut b = B::new(1, 1, 1);
    b.pal = vec![[9, 9, 9], [1, 1, 1]];
    b.clr_used = 1000;
    assert_eq!(decode(&b.bytes()).unwrap().pixels, vec![0x090909]);
}

fn fixture_check(bmp: &[u8], rgb: &[u8], w: usize, h: usize) {
    assert_eq!(rgb.len(), w * h * 3);
    let img = decode(bmp).unwrap();
    assert_eq!((img.width as usize, img.height as usize), (w, h));
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            let want = (rgb[i] as u32) << 16 | (rgb[i + 1] as u32) << 8 | rgb[i + 2] as u32;
            assert_eq!(img.get(x as i32, y as i32), Some(want), "pixel {},{}", x, y);
        }
    }
}

macro_rules! fixture {
    ($name:ident, $file:expr, $w:expr, $h:expr) => {
        #[test]
        fn $name() {
            fixture_check(
                include_bytes!(concat!("../tests/fixtures/", $file, ".bmp")),
                include_bytes!(concat!("../tests/fixtures/", $file, ".rgb")),
                $w,
                $h,
            );
        }
    };
}

fixture!(fixture_rgb24_33x7, "rgb24_33x7", 33, 7);
fixture!(fixture_rgb24_1x1, "rgb24_1x1", 1, 1);
fixture!(fixture_rgba32, "rgba32_5x3", 5, 3);
fixture!(fixture_pal8, "pal8_17x5", 17, 5);
fixture!(fixture_gray8, "gray8_16x4", 16, 4);
fixture!(fixture_mono1, "mono1_21x6", 21, 6);
fixture!(fixture_pal4, "pal4_9x5", 9, 5);
fixture!(fixture_topdown24, "rgb24_topdown_7x4", 7, 4);
fixture!(fixture_topdown8, "pal8_topdown_6x3", 6, 3);
fixture!(fixture_v5, "v5_rgb24_5x3", 5, 3);
fixture!(fixture_bitfields, "bitfields32_4x3", 4, 3);

#[test]
fn fixtures_report_their_headers() {
    let h = probe(include_bytes!("../tests/fixtures/mono1_21x6.bmp")).unwrap();
    assert_eq!((h.width, h.height, h.bpp, h.top_down, h.palette_len), (21, 6, 1, false, 2));
    let h = probe(include_bytes!("../tests/fixtures/rgb24_topdown_7x4.bmp")).unwrap();
    assert_eq!((h.bpp, h.top_down, h.palette_len, h.row_bytes), (24, true, 0, 24));
    assert_eq!(h.pixel_bytes(), 7 * 4 * 4);
}

// ---------------------------------------------------------------- refusals

#[test]
fn refusals() {
    // not a BMP
    assert_eq!(decode(b""), Err(Error::NotBmp));
    assert_eq!(decode(b"hello, this is a text file, not a picture at all."), Err(Error::NotBmp));
    assert_eq!(decode(&[b'B', b'M']), Err(Error::NotBmp));
    let good = {
        let mut b = B::new(2, 2, 24);
        b.rows = Some(vec![row24(2, 0), row24(2, 1)]);
        b.bytes()
    };
    let mut bad = good.clone();
    bad[1] = b'N';
    assert_eq!(decode(&bad), Err(Error::NotBmp));
    assert_eq!(decode(&good[..40]), Err(Error::NotBmp));
    assert_eq!(decode(&B::new(0, 5, 24).bytes()), Err(Error::NotBmp));
    assert_eq!(decode(&B::new(5, 0, 24).bytes()), Err(Error::NotBmp));
    assert_eq!(decode(&B::new(-5, 5, 24).bytes()), Err(Error::NotBmp));
    // compressed
    for comp in [1, 2, 4, 5] {
        let mut b = B::new(4, 4, 8);
        b.comp = comp;
        b.pal = vec![[0; 3]; 256];
        assert_eq!(decode(&b.bytes()), Err(Error::Compressed), "compression {}", comp);
    }
    // 16-bit
    assert_eq!(decode(&B::new(4, 4, 16).bytes()), Err(Error::SixteenBit));
    let mut b = B::new(4, 4, 16);
    b.comp = 3;
    assert_eq!(decode(&b.bytes()), Err(Error::SixteenBit));
    // unsupported
    let mut os2 = B::new(4, 4, 24).bytes();
    os2[14..18].copy_from_slice(&12u32.to_le_bytes());
    assert_eq!(decode(&os2), Err(Error::Unsupported));
    let mut b = B::new(4, 4, 24);
    b.planes = 2;
    assert_eq!(decode(&b.bytes()), Err(Error::Unsupported));
    for bpp in [2, 3, 15, 48, 64] {
        assert_eq!(decode(&B::new(4, 4, bpp).bytes()), Err(Error::Unsupported), "bpp {}", bpp);
    }
    let mut b = B::new(4, 4, 24);
    b.comp = 6;
    assert_eq!(decode(&b.bytes()), Err(Error::Unsupported));
    // a header size between 12 and 40 is not a header
    let mut odd = B::new(4, 4, 24).bytes();
    odd[14..18].copy_from_slice(&20u32.to_le_bytes());
    assert_eq!(decode(&odd), Err(Error::NotBmp));
}

#[test]
fn messages() {
    use alloc::string::ToString;
    assert_eq!(Error::NotBmp.to_string(), "not a BMP picture");
    assert_eq!(
        Error::Compressed.to_string(),
        "compressed BMP is not supported (save it uncompressed)"
    );
    assert_eq!(Error::SixteenBit.to_string(), "16-bit BMP is not supported");
    assert_eq!(Error::Unsupported.to_string(), "this kind of BMP is not supported");
    assert_eq!(Error::CutShort.to_string(), "the file is cut short");
    assert_eq!(
        Error::TooLarge { width: 4000, height: 3000 }.to_string(),
        "4000x3000 is too large (at most 4096x4096)"
    );
}

#[test]
fn size_limits() {
    let big = |w: i32, h: i32| {
        let mut b = B::new(w, h, 24);
        b.rows = Some(Vec::new()); // header only: the limits are checked before the data
        b.bytes()
    };
    assert_eq!(probe(&big(5000, 3000)), Err(Error::TooLarge { width: 5000, height: 3000 }));
    assert_eq!(probe(&big(4097, 1)), Err(Error::TooLarge { width: 4097, height: 1 }));
    assert_eq!(probe(&big(1, 4097)), Err(Error::TooLarge { width: 1, height: 4097 }));
    assert_eq!(probe(&big(4097, 4096)), Err(Error::TooLarge { width: 4097, height: 4096 }));
    assert_eq!(probe(&big(4096, -4097)), Err(Error::TooLarge { width: 4096, height: 4097 }));
    assert_eq!(
        probe(&big(i32::MAX, i32::MIN)),
        Err(Error::TooLarge { width: i32::MAX as u32, height: 1 << 31 })
    );
    assert_eq!(probe(&big(1, i32::MIN)), Err(Error::TooLarge { width: 1, height: 1 << 31 }));
    // right at the limits the picture is accepted by the size check (then it is cut short)
    assert_eq!(probe(&big(1920, 1200)), Err(Error::CutShort));
    assert_eq!(probe(&big(1921, 1200)), Err(Error::CutShort));
    assert_eq!(probe(&big(4096, 4096)), Err(Error::CutShort));
    assert_eq!(probe(&big(4096, -4096)), Err(Error::CutShort));
    // a real 1920x1200 picture
    let img = Image::new(1920, 1200, 0x123456);
    let bytes = encode(&img);
    let h = probe(&bytes).unwrap();
    assert_eq!((h.width, h.height, h.pixel_bytes()), (1920, 1200, 1920 * 1200 * 4));
    assert_eq!(decode(&bytes).unwrap(), img);
}

#[test]
fn cut_short() {
    let mut b = B::new(8, 8, 24);
    b.rows = Some((0..8).map(|y| row24(8, y)).collect());
    let bytes = b.bytes();
    assert!(decode(&bytes).is_ok());
    assert_eq!(decode(&bytes[..bytes.len() - 1]), Err(Error::CutShort));
    // palette past the end
    let mut b = B::new(2, 2, 8);
    b.pal = vec![[1, 1, 1]; 256];
    let bytes = b.bytes();
    assert_eq!(decode(&bytes[..14 + 40 + 100]), Err(Error::CutShort));
    // pixel offset past the end
    let mut bytes = B::new(2, 2, 24).bytes();
    bytes[10..14].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    assert_eq!(decode(&bytes), Err(Error::CutShort));
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn valid_files() -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    let mut b = B::new(9, 5, 24);
    b.rows = Some((0..5).map(|y| row24(9, y)).collect());
    v.push(b.bytes());
    let mut b = B::new(9, -5, 4);
    b.pal = VGA.to_vec();
    v.push(b.bytes());
    let mut b = B::new(6, 3, 8);
    b.pal = vec![[1, 2, 3]; 256];
    v.push(b.bytes());
    let mut b = B::new(5, 3, 32);
    b.comp = 3;
    b.masks = Some([0xFF_0000, 0xFF00, 0xFF]);
    v.push(b.bytes());
    let mut b = B::new(13, 2, 1);
    b.pal = vec![[0; 3], [255; 3]];
    v.push(b.bytes());
    v.push(encode(&Image::new(7, 3, 0xABCDEF)));
    v.push(include_bytes!("../tests/fixtures/v5_rgb24_5x3.bmp").to_vec());
    v
}

#[test]
fn never_panics_on_truncation() {
    for file in valid_files() {
        assert!(decode(&file).is_ok());
        for len in 0..file.len() {
            let r = decode(&file[..len]);
            assert!(r.is_err(), "a prefix of {} bytes decoded", len);
            let _ = probe(&file[..len]);
        }
    }
}

#[test]
fn never_panics_on_noise() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    // byte soups, some starting like a BMP
    for _ in 0..3000 {
        let len = (rng.next() % 200) as usize;
        let mut d: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if len > 2 && rng.next() % 2 == 0 {
            d[0] = b'B';
            d[1] = b'M';
        }
        let _ = decode(&d);
    }
    // valid files with a few damaged bytes (mostly in the headers)
    let files = valid_files();
    for _ in 0..20000 {
        let mut d = files[(rng.next() % files.len() as u64) as usize].clone();
        for _ in 0..1 + rng.next() % 4 {
            let i = (rng.next() % d.len().min(70) as u64) as usize;
            d[i] = match rng.next() % 4 {
                0 => 0,
                1 => 255,
                _ => rng.next() as u8,
            };
        }
        let _ = decode(&d);
    }
}

// ---------------------------------------------------------------- writing

#[test]
fn encode_two_by_two_by_hand() {
    let mut img = Image::new(2, 2, 0);
    img.set(0, 0, 0xFF0000); // red
    img.set(1, 0, 0x00FF00); // green
    img.set(0, 1, 0x0000FF); // blue
    img.set(1, 1, 0xFFFFFF); // white
    let want: Vec<u8> = vec![
        b'B', b'M', 70, 0, 0, 0, // signature, file size
        0, 0, 0, 0, // reserved
        54, 0, 0, 0, // pixel data offset
        40, 0, 0, 0, // header size
        2, 0, 0, 0, 2, 0, 0, 0, // width, height (positive: bottom-up)
        1, 0, 24, 0, // planes, bits per pixel
        0, 0, 0, 0, // BI_RGB
        16, 0, 0, 0, // biSizeImage
        0x13, 0x0B, 0, 0, 0x13, 0x0B, 0, 0, // 2835 pixels per metre both ways
        0, 0, 0, 0, 0, 0, 0, 0, // colours used, important
        // bottom row first: blue, white, then 2 bytes of padding
        0xFF, 0, 0, 0xFF, 0xFF, 0xFF, 0, 0, //
        // top row: red, green, padding
        0, 0, 0xFF, 0, 0xFF, 0, 0, 0,
    ];
    assert_eq!(encode(&img), want);
    assert_eq!(decode(&want).unwrap(), img);
}

#[test]
fn encoded_sizes() {
    assert_eq!(encoded_size(64, 40), 7734);
    assert_eq!(encoded_size(2, 2), 70);
    assert_eq!(encoded_size(1, 1), 58);
    assert_eq!(encoded_size(33, 7), 54 + 100 * 7);
    for (w, h) in [(1u32, 1u32), (2, 2), (3, 5), (33, 7), (64, 40), (100, 1)] {
        let bytes = encode(&Image::new(w, h, 0x102030));
        assert_eq!(bytes.len(), encoded_size(w, h));
    }
}

#[test]
fn encode_then_decode_is_the_identity() {
    for (w, h) in [(1usize, 1usize), (2, 2), (3, 5), (4, 4), (33, 7), (64, 40), (101, 3), (1, 50)] {
        let mut img = Image::new(w as u32, h as u32, 0);
        for y in 0..h {
            for x in 0..w {
                img.set(x as i32, y as i32, pat(x, y));
            }
        }
        let bytes = encode(&img);
        assert_eq!(decode(&bytes).unwrap(), img, "{}x{}", w, h);
        assert_eq!(encode(&decode(&bytes).unwrap()), bytes);
    }
}

// ---------------------------------------------------------------- colour reduction

#[test]
fn nearest_exact_and_tie() {
    for (i, c) in VGA.iter().enumerate() {
        assert_eq!(nearest(&VGA, rgb_of(*c)), i as u8);
    }
    assert_eq!(nearest(&VGA, 0x010101), 0);
    assert_eq!(nearest(&VGA, 0xFEFEFE), 15);
    assert_eq!(nearest(&VGA, 0x8000AA), 5);
    // a tie goes to the lowest index
    let pal = [[10, 10, 10]; 16];
    assert_eq!(nearest(&pal, 0x808080), 0);
    let mut pal = [[255u8, 255, 255]; 16];
    pal[3] = [0, 0, 0];
    pal[9] = [0, 0, 0];
    assert_eq!(nearest(&pal, 0x000000), 3);
    // two different colours at the same distance (red +-10 around 100)
    let mut pal = [[0u8, 0, 0]; 16];
    for p in pal.iter_mut() {
        *p = [255, 255, 255];
    }
    pal[5] = [110, 0, 0];
    pal[2] = [90, 0, 0];
    assert_eq!(nearest(&pal, 0x640000), 2);
    // weights: green counts more than red
    let mut pal = [[255u8, 255, 255]; 16];
    pal[0] = [0, 10, 0];
    pal[1] = [10, 0, 0];
    assert_eq!(nearest(&pal, 0), 1);
}

#[test]
fn quantize_never_dithers_an_exact_colour() {
    for (i, c) in VGA.iter().enumerate() {
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(quantize(&VGA, rgb_of(*c), x, y, true), i as u8);
                assert_eq!(quantize(&VGA, rgb_of(*c), x, y, false), i as u8);
            }
        }
    }
    // the pattern repeats every 4 pixels
    assert_eq!(quantize(&VGA, 0x808080, 1, 2, true), quantize(&VGA, 0x808080, 5, 6, true));
}

#[test]
fn quantize_dither() {
    let mut dithered = [false; 16];
    let mut plain = [false; 16];
    for y in 0..8 {
        for x in 0..8 {
            dithered[quantize(&VGA, 0x808080, x, y, true) as usize] = true;
            plain[quantize(&VGA, 0x808080, x, y, false) as usize] = true;
        }
    }
    assert!(dithered.iter().filter(|&&b| b).count() > 1);
    assert_eq!(plain.iter().filter(|&&b| b).count(), 1);
    // dithering mixes the two nearest greys
    assert!(dithered[8] && dithered[7]);
    // clamped at the ends: near-black stays black
    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(quantize(&VGA, 0x050505, x, y, true), 0);
        }
    }
}

#[test]
fn half_block_cells() {
    // a 2x2 picture is two cells wide and one cell tall: each cell shows a column's two pixels
    let mut img = Image::new(2, 2, 0);
    img.set(0, 0, 0x000000);
    img.set(0, 1, 0xFFFFFF);
    img.set(1, 0, 0xAA0000);
    img.set(1, 1, 0xAA0000);
    let idx = |x, y| quantize(&VGA, img.get(x, y).unwrap(), x as u32, y as u32, true);
    let left = half_block(idx(0, 0), idx(0, 1));
    let right = half_block(idx(1, 0), idx(1, 1));
    assert_eq!(left, 0xF0DF); // char DF, foreground black, background white
    assert_eq!(right, 0x4420); // equal halves: a space on that background
    assert_eq!(half_block(1, 2), 0x21DF);
    assert_eq!(half_block(15, 0), 0x0FDF);
}

// ---------------------------------------------------------------- fit and scale

#[test]
fn fit_cases() {
    assert_eq!(fit(256, 128, 80, 48, false), (80, 40));
    assert_eq!(fit(256, 128, 80, 48, true), (80, 40));
    assert_eq!(fit(128, 256, 80, 48, false), (24, 48));
    assert_eq!(fit(64, 40, 80, 48, false), (64, 40));
    assert_eq!(fit(64, 40, 80, 48, true), (64, 40)); // factor 1
    assert_eq!(fit(64, 40, 1024, 752, false), (64, 40));
    assert_eq!(fit(64, 40, 1024, 752, true), (1024, 640)); // 16x, limited by the width
    assert_eq!(fit(64, 40, 2000, 100, true), (128, 80)); // 2x, limited by the height
    assert_eq!(fit(1000, 10, 100, 50, false), (100, 1));
    assert_eq!(fit(4000, 1, 80, 25, false), (80, 1));
    assert_eq!(fit(1, 4000, 80, 25, false), (1, 25));
    assert_eq!(fit(80, 25, 80, 25, true), (80, 25));
    assert_eq!(fit(100, 20, 80, 48, false), (80, 16)); // only the width is too big
    assert_eq!(fit(0, 5, 80, 48, false), (0, 0));
    assert_eq!(fit(5, 5, 0, 48, false), (0, 0));
    // never larger than the area, ratio kept within a pixel
    for (w, h) in [(7u32, 3u32), (300, 299), (1920, 1200), (13, 400)] {
        for (aw, ah) in [(80u32, 48u32), (79, 25), (1024, 752), (5, 5)] {
            let (fw, fh) = fit(w, h, aw, ah, false);
            assert!(fw >= 1 && fh >= 1 && fw <= aw.max(w) && fh <= ah.max(h));
            if w > aw || h > ah {
                assert!(fw <= aw && fh <= ah);
            }
        }
    }
}

#[test]
fn scaled_row_averages_when_shrinking() {
    let mut img = Image::new(2, 2, 0);
    img.set(1, 0, 0xFFFFFF);
    img.set(0, 1, 0xFFFFFF);
    let mut out = [0u32; 1];
    scaled_row(&img, 1, 1, 0, 0, &mut out);
    assert_eq!(out[0], 0x808080);
    // 4x1 -> 2x1: each output pixel averages two
    let mut img = Image::new(4, 1, 0);
    img.pixels = vec![0x000000, 0x0000FE, 0xFF0000, 0x000000];
    let mut out = [0u32; 2];
    scaled_row(&img, 2, 1, 0, 0, &mut out);
    assert_eq!(out, [0x00_00_7F, 0x80_00_00]);
    // 3 -> 2: boxes [0,1) and [1,3)
    let mut img = Image::new(3, 1, 0);
    img.pixels = vec![10, 20, 40];
    let mut out = [0u32; 2];
    scaled_row(&img, 2, 1, 0, 0, &mut out);
    assert_eq!(out, [10, 30]);
}

#[test]
fn scaled_row_nearest_when_enlarging() {
    let mut img = Image::new(2, 2, 0);
    img.pixels = vec![1, 2, 3, 4];
    let mut out = [0u32; 4];
    scaled_row(&img, 4, 4, 0, 0, &mut out);
    assert_eq!(out, [1, 1, 2, 2]);
    scaled_row(&img, 4, 4, 1, 0, &mut out);
    assert_eq!(out, [1, 1, 2, 2]);
    scaled_row(&img, 4, 4, 2, 0, &mut out);
    assert_eq!(out, [3, 3, 4, 4]);
    scaled_row(&img, 4, 4, 3, 0, &mut out);
    assert_eq!(out, [3, 3, 4, 4]);
    // the same size is a copy
    let mut out = [0u32; 2];
    scaled_row(&img, 2, 2, 1, 0, &mut out);
    assert_eq!(out, [3, 4]);
    // dimensions decide independently: shrink x, enlarge y
    let mut img = Image::new(4, 1, 0);
    img.pixels = vec![0, 100, 200, 100];
    let mut out = [0u32; 2];
    for y in 0..3 {
        scaled_row(&img, 2, 3, y, 0, &mut out);
        assert_eq!(out, [50, 150]);
    }
}

#[test]
fn scaled_row_pans_and_clips() {
    let mut img = Image::new(5, 3, 0);
    for y in 0..3 {
        for x in 0..5 {
            img.set(x, y, pat(x as usize, y as usize));
        }
    }
    for (dw, dh) in [(13u32, 8u32), (5, 3), (3, 2), (2, 7), (9, 1)] {
        for y in 0..dh {
            let mut full = vec![0u32; dw as usize];
            scaled_row(&img, dw, dh, y, 0, &mut full);
            for x0 in 0..dw {
                let mut part = vec![0u32; (dw - x0).min(3) as usize];
                scaled_row(&img, dw, dh, y, x0, &mut part);
                assert_eq!(part[..], full[x0 as usize..x0 as usize + part.len()]);
            }
        }
    }
    // past the scaled picture: 0
    let mut out = [7u32; 3];
    scaled_row(&img, 4, 4, 0, 3, &mut out);
    assert_eq!(out[1..], [0, 0]);
    let mut out = [7u32; 3];
    scaled_row(&img, 4, 4, 4, 0, &mut out);
    assert_eq!(out, [0, 0, 0]);
    // a shrunk full-size row of a flat picture stays flat
    let flat = Image::new(1920, 1200, 0x336699);
    let mut out = vec![0u32; 80];
    scaled_row(&flat, 80, 48, 47, 0, &mut out);
    assert!(out.iter().all(|&p| p == 0x336699));
}

// ---------------------------------------------------------------- rectangles and the canvas

#[test]
fn rect_helpers() {
    let a = Rect::new(2, 3, 4, 5);
    assert!(!a.is_empty());
    assert!(a.contains(2, 3) && a.contains(5, 7));
    assert!(!a.contains(6, 3) && !a.contains(2, 8) && !a.contains(1, 3));
    assert!(Rect::new(0, 0, 0, 5).is_empty());
    assert!(!Rect::EMPTY.contains(0, 0));
    assert_eq!(a.union(Rect::new(0, 0, 1, 1)), Rect::new(0, 0, 6, 8));
    assert_eq!(a.union(Rect::EMPTY), a);
    assert_eq!(Rect::EMPTY.union(a), a);
    assert!(Rect::EMPTY.union(Rect::EMPTY).is_empty());
    assert_eq!(a.clip_to(4, 5), Rect::new(2, 3, 2, 2));
    assert_eq!(a.clip_to(2, 5), Rect::EMPTY);
    assert_eq!(Rect::new(-3, -3, 5, 5).clip_to(10, 10), Rect::new(0, 0, 2, 2));
    assert_eq!(Rect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX).clip_to(10, 10), Rect::EMPTY);
    assert_eq!(Rect::new(-5, 0, i32::MAX, 3).clip_to(10, 10), Rect::new(0, 0, 10, 3));
    assert_eq!(Rect::from_corners(5, 7, 2, 3), Rect::new(2, 3, 4, 5));
    let u = Rect::new(i32::MAX - 1, 0, 2, 1).union(Rect::new(i32::MIN, 0, 1, 1));
    assert_eq!(u.w, i32::MAX); // saturates instead of overflowing
}

#[test]
fn image_get_set_and_rect_copy() {
    let mut img = Image::new(5, 4, 7);
    assert_eq!(img.get(0, 0), Some(7));
    assert_eq!(img.get(4, 3), Some(7));
    assert_eq!(img.get(5, 0), None);
    assert_eq!(img.get(0, 4), None);
    assert_eq!(img.get(-1, 0), None);
    assert_eq!(img.get(i32::MIN, i32::MAX), None);
    img.set(-1, 0, 1);
    img.set(5, 0, 1);
    img.set(2, 2, 9);
    assert_eq!(img.pixels.iter().filter(|&&p| p == 9).count(), 1);
    assert_eq!(img.pixels.iter().filter(|&&p| p == 1).count(), 0);

    let before = img.clone();
    let r = Rect::new(1, 1, 3, 2);
    let saved = img.copy_rect(r);
    assert_eq!(saved.len(), 6);
    assert_eq!(saved[1], 7);
    img.fill_rect(r, 0xAB);
    assert_eq!(img.get(1, 1), Some(0xAB));
    assert_eq!(img.get(3, 2), Some(0xAB));
    assert_eq!(img.get(4, 2), Some(7));
    img.paste_rect(r, &saved);
    assert_eq!(img, before);
    // clipping
    img.fill_rect(Rect::new(-2, -2, 4, 4), 5);
    assert_eq!(img.pixels.iter().filter(|&&p| p == 5).count(), 4);
    assert_eq!(img.copy_rect(Rect::new(100, 100, 3, 3)), Vec::<u32>::new());
    let part = img.copy_rect(Rect::new(-1, -1, 3, 3));
    assert_eq!(part, vec![5, 5, 5, 5]);
    // a paste that sticks out, or has too little data, never panics
    img.paste_rect(Rect::new(3, 2, 4, 4), &[1; 16]);
    assert_eq!(img.get(4, 3), Some(1));
    img.paste_rect(Rect::new(0, 0, 3, 3), &[2; 2]);
}

// ---------------------------------------------------------------- lines

#[test]
fn brush_rect_shapes() {
    assert_eq!(brush_rect(10, 10, 1), Rect::new(10, 10, 1, 1));
    assert_eq!(brush_rect(10, 10, 2), Rect::new(10, 10, 2, 2));
    assert_eq!(brush_rect(10, 10, 3), Rect::new(9, 9, 3, 3));
    assert_eq!(brush_rect(10, 10, 4), Rect::new(9, 9, 4, 4));
    assert_eq!(brush_rect(10, 10, 5), Rect::new(8, 8, 5, 5));
    assert_eq!(brush_rect(0, 0, 0), Rect::new(0, 0, 1, 1));
    assert_eq!(brush_rect(i32::MIN, i32::MAX, 9).w, 9);
}

fn points(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
    let mut v = Vec::new();
    line_points(x0, y0, x1, y1, |x, y| v.push((x, y)));
    v
}

#[test]
fn line_points_all_octants() {
    let c = 20;
    let offs = [-9, -5, -2, 0, 2, 5, 9];
    for &dx in &offs {
        for &dy in &offs {
            let p = points(c, c, c + dx, c + dy);
            let n = (dx as i32).abs().max((dy as i32).abs()) as usize + 1;
            assert_eq!(p.len(), n, "{},{}", dx, dy);
            assert_eq!(p[0], (c, c));
            assert_eq!(*p.last().unwrap(), (c + dx, c + dy));
            for w in p.windows(2) {
                let (ax, ay) = ((w[1].0 - w[0].0).abs(), (w[1].1 - w[0].1).abs());
                assert!(ax <= 1 && ay <= 1 && ax + ay >= 1, "gap or repeat at {},{}", dx, dy);
            }
        }
    }
    assert_eq!(points(3, 3, 3, 3), vec![(3, 3)]);
    assert_eq!(points(0, 0, 4, 0), vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    assert_eq!(points(0, 0, 3, 3), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    // monotone in the long direction
    let p = points(0, 0, 20, 7);
    assert!(p.windows(2).all(|w| w[1].0 == w[0].0 + 1));
}

#[test]
fn image_line_dot_and_clipping() {
    let mut img = Image::new(10, 10, 0);
    let r = img.dot(5, 5, 1, 1);
    assert_eq!(r, Rect::new(5, 5, 1, 1));
    let r = img.dot(5, 5, 3, 2);
    assert_eq!(r, Rect::new(4, 4, 3, 3));
    assert_eq!(img.pixels.iter().filter(|&&p| p == 2).count(), 9);
    // a dot on the corner is clipped, and a dot outside changes nothing
    let r = img.dot(0, 0, 3, 3);
    assert_eq!(r, Rect::new(0, 0, 2, 2));
    let before = img.clone();
    assert!(img.dot(-10, 3, 3, 9).is_empty());
    assert!(img.dot(3, 30, 1, 9).is_empty());
    assert_eq!(img, before);

    let mut img = Image::new(10, 10, 0);
    let r = img.line(1, 2, 8, 2, 1, 7);
    assert_eq!(r, Rect::new(1, 2, 8, 1));
    assert_eq!(img.pixels.iter().filter(|&&p| p == 7).count(), 8);
    let mut img = Image::new(10, 10, 0);
    let r = img.line(0, 0, 9, 9, 1, 7);
    assert_eq!(r, Rect::new(0, 0, 10, 10));
    for i in 0..10 {
        assert_eq!(img.get(i, i), Some(7));
    }
    assert_eq!(img.pixels.iter().filter(|&&p| p == 7).count(), 10);
    // starting and ending outside
    let mut img = Image::new(4, 4, 0);
    let r = img.line(-5, -5, 8, 8, 1, 7);
    assert_eq!(r, Rect::new(0, 0, 4, 4));
    assert_eq!(img.pixels.iter().filter(|&&p| p == 7).count(), 4);
    // a thick line
    let mut img = Image::new(20, 20, 0);
    let r = img.line(5, 10, 15, 10, 3, 7);
    assert_eq!(r, Rect::new(4, 9, 13, 3));
    assert_eq!(img.pixels.iter().filter(|&&p| p == 7).count(), 39);
    // wholly outside
    let mut img = Image::new(4, 4, 0);
    assert!(img.line(-9, -9, -2, -3, 3, 7).is_empty());
    assert!(img.pixels.iter().all(|&p| p == 0));
}

// ---------------------------------------------------------------- rectangles

#[test]
fn image_rect() {
    let mut img = Image::new(12, 12, 0);
    let r = img.rect(1, 1, 10, 10, 3, false, 5);
    assert_eq!(r, Rect::new(1, 1, 10, 10));
    for y in 0..12i32 {
        for x in 0..12i32 {
            let inside = (1..=10).contains(&x) && (1..=10).contains(&y);
            let band = inside && (x < 4 || x > 7 || y < 4 || y > 7);
            assert_eq!(img.get(x, y), Some(if band { 5 } else { 0 }), "{},{}", x, y);
        }
    }
    // corners in any order give the same picture
    let mut b = Image::new(12, 12, 0);
    b.rect(10, 10, 1, 1, 3, false, 5);
    assert_eq!(img, b);
    // size 1 is a 1 px outline
    let mut c = Image::new(6, 6, 0);
    c.rect(0, 0, 5, 5, 1, false, 1);
    assert_eq!(c.pixels.iter().filter(|&&p| p == 1).count(), 20);
    assert_eq!(c.get(2, 2), Some(0));
    // an outline thicker than the box fills it
    let mut d = Image::new(6, 6, 0);
    d.rect(1, 1, 3, 4, 9, false, 1);
    assert_eq!(d.pixels.iter().filter(|&&p| p == 1).count(), 12);
    // filled
    let mut e = Image::new(8, 8, 0);
    let r = e.rect(2, 2, 5, 4, 3, true, 9);
    assert_eq!(r, Rect::new(2, 2, 4, 3));
    assert_eq!(e.pixels.iter().filter(|&&p| p == 9).count(), 12);
    // clipping
    let mut f = Image::new(8, 8, 0);
    let r = f.rect(-5, -5, 2, 2, 1, true, 9);
    assert_eq!(r, Rect::new(0, 0, 3, 3));
    assert_eq!(f.pixels.iter().filter(|&&p| p == 9).count(), 9);
    let mut g = Image::new(8, 8, 0);
    let r = g.rect(-5, -5, 20, 20, 2, false, 9);
    assert_eq!(r, Rect::new(0, 0, 8, 8));
    assert!(g.pixels.iter().all(|&p| p == 0)); // the outline lies outside
    assert!(g.rect(20, 20, 30, 30, 1, true, 9).is_empty());
}

// ---------------------------------------------------------------- ellipses

fn outline(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
    let mut v = Vec::new();
    ellipse_outline(x0, y0, x1, y1, |x, y| v.push((x, y)));
    v
}

fn spans(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32, i32)> {
    let mut v = Vec::new();
    ellipse_spans(x0, y0, x1, y1, |y, a, b| v.push((y, a, b)));
    v
}

#[test]
fn ellipse_degenerate_boxes() {
    assert_eq!(outline(4, 4, 4, 4), vec![(4, 4)]);
    assert_eq!(spans(4, 4, 4, 4), vec![(4, 4, 4)]);
    // 1 pixel high: a horizontal line
    assert_eq!(outline(2, 5, 6, 5), (2..=6).map(|x| (x, 5)).collect::<Vec<_>>());
    assert_eq!(spans(2, 5, 6, 5), vec![(5, 2, 6)]);
    // 1 pixel wide: a vertical line
    assert_eq!(outline(3, 1, 3, 6), (1..=6).map(|y| (3, y)).collect::<Vec<_>>());
    assert_eq!(spans(3, 1, 3, 6), (1..=6).map(|y| (y, 3, 3)).collect::<Vec<_>>());
    // 2x2 is a square
    let mut o = outline(0, 0, 1, 1);
    o.sort();
    assert_eq!(o, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
}

#[test]
fn ellipse_circle_5x5_by_hand() {
    // 5x5: rows of 3, 5, 5, 5, 3 pixels
    let s = spans(0, 0, 4, 4);
    assert_eq!(s, vec![(0, 1, 3), (1, 0, 4), (2, 0, 4), (3, 0, 4), (4, 1, 3)]);
    let mut o = outline(0, 0, 4, 4);
    o.sort();
    let mut want = vec![(1, 0), (2, 0), (3, 0), (0, 1), (4, 1), (0, 2), (4, 2), (0, 3), (4, 3), (1, 4), (2, 4), (3, 4)];
    want.sort();
    assert_eq!(o, want);
}

fn check_ellipse(x0: i32, y0: i32, w: i32, h: i32) {
    let (x1, y1) = (x0 + w - 1, y0 + h - 1);
    let tag = (w, h);
    let sp = spans(x0, y0, x1, y1);
    assert_eq!(sp.len(), h as usize, "{:?}", tag);
    for (i, &(y, a, b)) in sp.iter().enumerate() {
        assert_eq!(y, y0 + i as i32);
        assert!(a >= x0 && b <= x1 && a <= b, "{:?} row {}", tag, i);
        assert_eq!(a - x0, x1 - b, "symmetric {:?} row {}", tag, i);
        if i > 0 {
            let (_, pa, pb) = sp[i - 1];
            if 2 * i as i32 <= h - 1 {
                assert!(a <= pa && b >= pb, "grows to the middle {:?} row {}", tag, i);
            } else {
                assert!(a >= pa && b <= pb, "shrinks after the middle {:?} row {}", tag, i);
            }
        }
    }
    for i in 0..sp.len() {
        assert_eq!(sp[i].1, sp[sp.len() - 1 - i].1, "vertical symmetry {:?}", tag);
    }
    // touches the four sides
    assert!(sp.iter().any(|s| s.1 == x0) && sp.iter().any(|s| s.2 == x1), "{:?}", tag);
    // the outline: no duplicates, inside the fill, 8-connected, touches all sides
    let o = outline(x0, y0, x1, y1);
    let mut sorted = o.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), o.len(), "duplicate outline points {:?}", tag);
    let filled = |x: i32, y: i32| {
        sp.iter().any(|&(sy, a, b)| sy == y && x >= a && x <= b)
    };
    for &(x, y) in &o {
        assert!(filled(x, y), "outline outside the fill {:?}", tag);
    }
    assert!(o.iter().any(|p| p.0 == x0) && o.iter().any(|p| p.0 == x1), "{:?}", tag);
    assert!(o.iter().any(|p| p.1 == y0) && o.iter().any(|p| p.1 == y1), "{:?}", tag);
    let mut seen = vec![false; o.len()];
    let mut stack = vec![0usize];
    seen[0] = true;
    while let Some(i) = stack.pop() {
        for j in 0..o.len() {
            if !seen[j] && (o[i].0 - o[j].0).abs() <= 1 && (o[i].1 - o[j].1).abs() <= 1 {
                seen[j] = true;
                stack.push(j);
            }
        }
    }
    assert!(seen.iter().all(|&s| s), "outline not connected {:?}", tag);
    // every filled pixel that is not on the outline has all four neighbours filled
    for &(y, a, b) in &sp {
        for x in a..=b {
            if !o.contains(&(x, y)) {
                for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                    assert!(filled(nx, ny), "hole next to {},{} in {:?}", x, y, tag);
                }
            }
        }
    }
    // the corner order does not matter
    assert_eq!(spans(x1, y1, x0, y0), sp);
    assert_eq!(spans(x1, y0, x0, y1), sp);
}

#[test]
fn ellipse_properties_on_small_boxes() {
    for w in 1..=18 {
        for h in 1..=18 {
            check_ellipse(3, -2, w, h);
        }
    }
    for (w, h) in [(40, 7), (7, 40), (64, 40), (100, 3), (3, 100), (101, 100)] {
        check_ellipse(0, 0, w, h);
    }
}

#[test]
fn image_ellipse() {
    let mut img = Image::new(20, 20, 0);
    let r = img.ellipse(2, 3, 11, 8, 1, true, 4);
    assert_eq!(r, Rect::new(2, 3, 10, 6));
    let want: usize = spans(2, 3, 11, 8).iter().map(|s| (s.2 - s.1 + 1) as usize).sum();
    assert_eq!(img.pixels.iter().filter(|&&p| p == 4).count(), want);
    assert_eq!(img.get(2, 3), Some(0)); // the corner of the box is outside the ellipse
    let mut img = Image::new(20, 20, 0);
    let r = img.ellipse(2, 3, 11, 8, 1, false, 4);
    assert_eq!(r.w, 10);
    assert_eq!(img.pixels.iter().filter(|&&p| p == 4).count(), outline(2, 3, 11, 8).len());
    assert_eq!(img.get(6, 5), Some(0)); // hollow inside
    // thick outline stays inside what the brush can reach
    let mut img = Image::new(30, 30, 0);
    let r = img.ellipse(5, 5, 24, 24, 3, false, 4);
    assert_eq!(r, Rect::new(4, 4, 22, 22));
    assert_eq!(img.get(14, 4), Some(4));
    assert_eq!(img.get(14, 14), Some(0));
    // clipped when it goes outside
    let mut img = Image::new(10, 10, 0);
    let r = img.ellipse(-10, -10, 9, 9, 1, true, 4);
    assert_eq!(r, Rect::new(0, 0, 10, 10));
    assert_eq!(img.get(0, 0), Some(4));
    assert_eq!(img.get(9, 9), Some(0)); // the corner of the box is outside the ellipse
    assert!(img.ellipse(50, 50, 60, 60, 1, true, 4).is_empty());
    // a single point and a line
    let mut img = Image::new(5, 5, 0);
    assert_eq!(img.ellipse(2, 2, 2, 2, 1, false, 4), Rect::new(2, 2, 1, 1));
    assert_eq!(img.ellipse(0, 4, 4, 4, 1, true, 5), Rect::new(0, 4, 5, 1));
}

// ---------------------------------------------------------------- flood fill

#[test]
fn flood_fill_small() {
    let mut img = Image::new(6, 4, 0);
    // a wall of colour 1 with a gap at the bottom
    for y in 0..3 {
        img.set(3, y, 1);
    }
    let r = img.flood_fill(0, 0, 2);
    assert_eq!(r, Rect::new(0, 0, 6, 4));
    assert_eq!(img.get(5, 0), Some(2)); // went round through the gap
    assert_eq!(img.get(3, 0), Some(1));
    // close the gap: the right side is not reached
    let mut img = Image::new(6, 4, 0);
    for y in 0..4 {
        img.set(3, y, 1);
    }
    let r = img.flood_fill(0, 0, 2);
    assert_eq!(r, Rect::new(0, 0, 3, 4));
    assert_eq!(img.get(4, 0), Some(0));
    // same colour: nothing changes
    let before = img.clone();
    assert!(img.flood_fill(0, 0, 2).is_empty());
    assert_eq!(img, before);
    // outside
    assert!(img.flood_fill(-1, 0, 9).is_empty());
    assert!(img.flood_fill(0, 4, 9).is_empty());
    // diagonal pixels are not connected
    let mut img = Image::new(3, 3, 0);
    img.set(1, 0, 1);
    img.set(0, 1, 1);
    img.set(2, 1, 1);
    img.set(1, 2, 1);
    let r = img.flood_fill(1, 1, 5);
    assert_eq!(r, Rect::new(1, 1, 1, 1));
    assert_eq!(img.get(0, 0), Some(0));
    assert_eq!(img.pixels.iter().filter(|&&p| p == 5).count(), 1);
}

#[test]
fn flood_fill_maze() {
    // a spiral corridor 1 px wide, walls of colour 1
    let (w, h) = (21usize, 21usize);
    let mut img = Image::new(w as u32, h as u32, 1);
    let (mut x, mut y) = (0i32, 0i32);
    let dirs = [(1, 0), (0, 1), (-1, 0), (0, -1)];
    let mut d = 0;
    let mut cells = 0;
    img.set(x, y, 0);
    cells += 1;
    loop {
        // go on in direction d while the next cell is inside, unpainted, and does not touch
        // another corridor cell on its far side
        let free = |img: &Image, x: i32, y: i32, d: usize| {
            let (nx, ny) = (x + dirs[d].0, y + dirs[d].1);
            if img.get(nx, ny) != Some(1) {
                return false;
            }
            let (fx, fy) = (nx + dirs[d].0, ny + dirs[d].1);
            img.get(fx, fy).map_or(true, |p| p == 1)
        };
        if free(&img, x, y, d) {
            x += dirs[d].0;
            y += dirs[d].1;
            img.set(x, y, 0);
            cells += 1;
        } else {
            d = (d + 1) % 4;
            if !free(&img, x, y, d) {
                break;
            }
        }
    }
    let r = img.flood_fill(0, 0, 9);
    assert_eq!(img.pixels.iter().filter(|&&p| p == 9).count(), cells);
    assert!(r.w > 15 && r.h > 15);
    assert!(img.pixels.iter().all(|&p| p == 1 || p == 9));
}

#[test]
fn flood_fill_big_canvases() {
    // 1920x1200 blank
    let mut img = Image::new(1920, 1200, 0xFFFFFF);
    let r = img.flood_fill(500, 500, 0x123456);
    assert_eq!(r, Rect::new(0, 0, 1920, 1200));
    // the fallible fill does the same, and reports an untouched spot as Ok and empty
    let mut again = Image::new(1920, 1200, 0xFFFFFF);
    assert_eq!(again.try_flood_fill(500, 500, 0x123456), Ok(Rect::new(0, 0, 1920, 1200)));
    assert_eq!(again, img);
    assert_eq!(again.try_flood_fill(5, 5, 0x123456), Ok(Rect::EMPTY));
    assert_eq!(again.try_flood_fill(-1, 5, 0), Ok(Rect::EMPTY));
    assert!(img.pixels.iter().all(|&p| p == 0x123456));
    // 1000x1000 checkerboard: every pixel is its own area
    let mut img = Image::new(1000, 1000, 0);
    for y in 0..1000 {
        for x in 0..1000 {
            img.set(x, y, ((x + y) & 1) as u32);
        }
    }
    let r = img.flood_fill(500, 500, 7);
    assert_eq!(r, Rect::new(500, 500, 1, 1));
    assert_eq!(img.pixels.iter().filter(|&&p| p == 7).count(), 1);
    // 1000x1000 comb: vertical walls with alternating gaps, one long snake
    let mut img = Image::new(1000, 1000, 0);
    for x in (1..1000).step_by(2) {
        for y in 0..1000 {
            img.set(x, y, 1);
        }
        let gap = if (x / 2) % 2 == 0 { 999 } else { 0 };
        img.set(x, gap, 0);
    }
    let r = img.flood_fill(0, 0, 3);
    assert_eq!(r, Rect::new(0, 0, 1000, 1000));
    assert_eq!(img.pixels.iter().filter(|&&p| p == 3).count(), 500 * 1000 + 500);
    // a wide striped picture: many runs per row
    let mut img = Image::new(1000, 1000, 0);
    for y in 0..1000 {
        for x in 0..1000 {
            img.set(x, y, if y % 2 == 0 { (x % 2) as u32 } else { 0 });
        }
    }
    let r = img.flood_fill(1, 1, 8);
    assert!(!r.is_empty());
}
