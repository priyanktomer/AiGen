#!/usr/bin/env python3
"""Generate SwiftLoad's icon set.

Committed as a script rather than as opaque binaries so the mark can be changed without a
design tool, and so anyone can see exactly what the bytes are. Run from this directory:

    python3 generate.py

Writes 32x32.png, 128x128.png, 128x128@2x.png and icon.ico.
"""
import struct
import zlib

ACCENT = (37, 99, 235)      # the app's blue
GLYPH = (255, 255, 255)
SS = 4                      # supersampling factor, for antialiasing


def shape(x, y):
    """Coverage of the glyph at a point in the unit square, as (in_tile, in_glyph)."""
    # Rounded square tile.
    r = 0.22
    cx = min(max(x, r), 1 - r)
    cy = min(max(y, r), 1 - r)
    in_tile = (x - cx) ** 2 + (y - cy) ** 2 <= r * r

    # Download arrow: a stem, a head, and a baseline under it.
    stem = 0.44 <= x <= 0.56 and 0.20 <= y <= 0.52
    # Triangle head, apex down at y=0.70.
    head = False
    if 0.52 <= y <= 0.70:
        t = (y - 0.52) / 0.18
        half = 0.20 * (1 - t)
        head = abs(x - 0.5) <= half
    base = 0.26 <= x <= 0.74 and 0.78 <= y <= 0.86
    return in_tile, (stem or head or base)


def render(size):
    px = bytearray()
    for py in range(size):
        for pxi in range(size):
            tile = glyph = 0
            for sy in range(SS):
                for sx in range(SS):
                    x = (pxi + (sx + 0.5) / SS) / size
                    y = (py + (sy + 0.5) / SS) / size
                    t, g = shape(x, y)
                    tile += t
                    glyph += g
            n = SS * SS
            a_tile = tile / n
            a_glyph = glyph / n
            # Glyph over tile, tile over transparency.
            r = ACCENT[0] * (1 - a_glyph) + GLYPH[0] * a_glyph
            g_ = ACCENT[1] * (1 - a_glyph) + GLYPH[1] * a_glyph
            b = ACCENT[2] * (1 - a_glyph) + GLYPH[2] * a_glyph
            px += bytes((int(r), int(g_), int(b), int(255 * a_tile)))
    return bytes(px)


def png(size, pixels):
    raw = b"".join(
        b"\x00" + pixels[y * size * 4 : (y + 1) * size * 4] for y in range(size)
    )

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico(entries):
    """A PNG-compressed ICO, which every supported Windows version reads."""
    out = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    body = b""
    for size, data in entries:
        dim = 0 if size >= 256 else size  # 0 means 256 in the ICO header
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
        body += data
    return out + body


if __name__ == "__main__":
    cache = {s: png(s, render(s)) for s in (16, 32, 48, 128, 256)}
    for name, size in (("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256)):
        with open(name, "wb") as f:
            f.write(cache[size])
    with open("icon.png", "wb") as f:
        f.write(cache[256])
    with open("icon.ico", "wb") as f:
        f.write(ico([(s, cache[s]) for s in (16, 32, 48, 256)]))
    print("wrote 32x32.png 128x128.png 128x128@2x.png icon.png icon.ico")
