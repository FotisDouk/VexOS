"""Makes the BMP fixtures of the bmp crate (run: python make.py; needs Pillow).

Each `name.bmp` comes with `name.rgb`: the picture the file must decode to, as raw R,G,B bytes,
rows top to bottom (alpha ignored). PIL writes the 1, 8, 24 and 32-bit files; PIL cannot write
4-bit, top-down, V5-header or BI_BITFIELDS files, so those are built by hand with struct.
"""
import os
import struct

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))

VGA = [
    (0x00, 0x00, 0x00), (0x00, 0x00, 0xAA), (0x00, 0xAA, 0x00), (0x00, 0xAA, 0xAA),
    (0xAA, 0x00, 0x00), (0xAA, 0x00, 0xAA), (0xAA, 0x55, 0x00), (0xAA, 0xAA, 0xAA),
    (0x55, 0x55, 0x55), (0x55, 0x55, 0xFF), (0x55, 0xFF, 0x55), (0x55, 0xFF, 0xFF),
    (0xFF, 0x55, 0x55), (0xFF, 0x55, 0xFF), (0xFF, 0xFF, 0x55), (0xFF, 0xFF, 0xFF),
]


def out(name, bmp_bytes, rgb_rows):
    """Writes name.bmp and name.rgb (rgb_rows: list of rows of (r, g, b), top row first)."""
    with open(os.path.join(HERE, name + ".bmp"), "wb") as f:
        f.write(bmp_bytes)
    with open(os.path.join(HERE, name + ".rgb"), "wb") as f:
        for row in rgb_rows:
            for (r, g, b) in row:
                f.write(bytes((r, g, b)))


def pil_bytes(img, path_name):
    path = os.path.join(HERE, path_name + ".bmp")
    img.save(path, "BMP")
    with open(path, "rb") as f:
        return f.read()


def rows_of(img):
    rgb = img.convert("RGB")
    w, h = rgb.size
    return [[rgb.getpixel((x, y)) for x in range(w)] for y in range(h)]


def pil(name, img):
    data = pil_bytes(img, name)
    out(name, data, rows_of(img))


def pad4(b):
    return b + b"\0" * (-len(b) % 4)


def build(w, h, bpp, rows, palette=None, top_down=False, dib=40, compression=0, masks=None):
    """A BMP by hand. rows: raw (unpadded) row bytes, top row first."""
    stored = list(rows) if top_down else list(reversed(rows))
    pix = b"".join(pad4(r) for r in stored)
    head = struct.pack(
        "<IiiHHIIiiII", dib, w, -h if top_down else h, 1, bpp, 3 if masks else compression,
        len(pix), 2835, 2835, len(palette) if palette else 0, 0)
    head += b"\0" * (dib - 40)
    extra = b""
    if masks:
        extra = struct.pack("<III", *masks)  # follows the 40-byte header
    pal = b"".join(bytes((b, g, r, 0)) for (r, g, b) in (palette or []))
    off = 14 + dib + len(extra) + len(pal)
    return b"BM" + struct.pack("<IHHI", off + len(pix), 0, 0, off) + head + extra + pal + pix


def rgb_pattern(x, y):
    return ((x * 7) % 256, (y * 36 + 5) % 256, (x * y * 3 + 11) % 256)


def main():
    # PIL: 24-bit with row padding (33*3 = 99 bytes, padded to 100) and the smallest picture.
    img = Image.new("RGB", (33, 7))
    img.putdata([rgb_pattern(x, y) for y in range(7) for x in range(33)])
    pil("rgb24_33x7", img)
    img = Image.new("RGB", (1, 1))
    img.putpixel((0, 0), (200, 100, 50))
    pil("rgb24_1x1", img)

    # PIL: 32-bit (alpha varies and must be ignored).
    img = Image.new("RGBA", (5, 3))
    img.putdata([(x * 40, y * 90, 255 - x * 30, (x + y * 5) * 17 % 256)
                 for y in range(3) for x in range(5)])
    pil("rgba32_5x3", img)

    # PIL: 8-bit palette (6 colours used), grey, and 1-bit.
    img = Image.new("P", (17, 5))
    img.putpalette([c for rgb in VGA[:6] for c in rgb])
    img.putdata([(x * 2 + y) % 6 for y in range(5) for x in range(17)])
    pil("pal8_17x5", img)
    img = Image.new("L", (16, 4))
    img.putdata([(x * 16 + y * 3) % 256 for y in range(4) for x in range(16)])
    pil("gray8_16x4", img)
    img = Image.new("1", (21, 6))
    img.putdata([255 if (x // 2 + y) % 3 == 0 else 0 for y in range(6) for x in range(21)])
    pil("mono1_21x6", img)

    # By hand: 4-bit with the 16 VGA colours (9 px wide: 5 bytes a row, padded to 8).
    w, h = 9, 5
    idx = [[(x * 3 + y) % 16 for x in range(w)] for y in range(h)]
    rows = []
    for r in idx:
        b = bytearray()
        for x in range(0, w, 2):
            hi = r[x]
            lo = r[x + 1] if x + 1 < w else 0
            b.append(hi << 4 | lo)
        rows.append(bytes(b))
    out("pal4_9x5", build(w, h, 4, rows, palette=VGA), [[VGA[i] for i in r] for r in idx])

    # By hand: 24-bit top-down.
    w, h = 7, 4
    px = [[rgb_pattern(x, y) for x in range(w)] for y in range(h)]
    rows = [b"".join(bytes((b, g, r)) for (r, g, b) in row) for row in px]
    out("rgb24_topdown_7x4", build(w, h, 24, rows, top_down=True), px)

    # By hand: 8-bit top-down with 3 palette entries.
    w, h = 6, 3
    pal = [(255, 0, 0), (0, 255, 0), (0, 0, 255)]
    idx = [[(x + y) % 3 for x in range(w)] for y in range(h)]
    out("pal8_topdown_6x3", build(w, h, 8, [bytes(r) for r in idx], palette=pal, top_down=True),
        [[pal[i] for i in r] for r in idx])

    # By hand: 24-bit with a V5 header (124 bytes).
    w, h = 5, 3
    px = [[rgb_pattern(x * 3, y * 5) for x in range(w)] for y in range(h)]
    rows = [b"".join(bytes((b, g, r)) for (r, g, b) in row) for row in px]
    out("v5_rgb24_5x3", build(w, h, 24, rows, dib=124), px)

    # By hand: 32-bit BI_BITFIELDS (masks 00FF0000 0000FF00 000000FF after the 40-byte header).
    w, h = 4, 3
    px = [[rgb_pattern(x * 9, y * 7) for x in range(w)] for y in range(h)]
    rows = [b"".join(bytes((b, g, r, 0x80)) for (r, g, b) in row) for row in px]
    out("bitfields32_4x3", build(w, h, 32, rows, masks=(0xFF0000, 0xFF00, 0xFF)), px)


main()
