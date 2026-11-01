#!/usr/bin/env python3
"""Draws VibeOS's app icon, "Night Shift", into an .iconset folder.

    scripts/app-icon.py build/AppIcon.iconset      # then: iconutil -c icns …
    scripts/app-icon.py --preview out.png 512      # one size, for a look

The icon is a Mac at night: a squircle of deep ink with four windows, the
compartments agents run in. Three glow phosphor green, as working agents
do in the menu; one is dark, with a crescent moon, as an agent that has
gone to sleep. The menu bar glyph (MenuBarGlyph.swift) is the same four
windows, lit one by one as agents wake.

Pure Python, standard library only, so it runs anywhere scripts/sign-macos.sh
does, and no image is checked in. Every shape is a signed distance field
with analytic antialiasing, drawn at each size directly, so small sizes stay
crisp and drop the details they can't hold (the stars, then the moon).
"""

from array import array
import math
import os
import struct
import sys
import zlib

# The palette. INK is the night; PHOSPHOR the lit windows (the app's signal
# colour, as in Theme.swift); MOON the sleeping window's crescent.
INK_TOP = (0x26, 0x2B, 0x5E)
INK_BOTTOM = (0x0A, 0x0C, 0x1F)
PHOSPHOR_TOP = (0x8C, 0xFA, 0xD6)
PHOSPHOR_BOTTOM = (0x1E, 0xC2, 0x96)
GLOW = (0x3C, 0xE3, 0xB0)
MOON = (0xE6, 0xE4, 0xFF)

# Apple's macOS icon grid, on a 1024 canvas: the squircle is 824 wide,
# centred, and the artwork drops a soft shadow below it.
CANVAS = 1024.0
BODY = 824.0
BODY_RADIUS = 185.4


def clamp(x, lo=0.0, hi=1.0):
    return lo if x < lo else hi if x > hi else x


def mix(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def smooth(t):
    t = clamp(t)
    return t * t * (3 - 2 * t)


def rounded_rect(px, py, cx, cy, hw, hh, r):
    """Signed distance to a rounded rectangle centred on (cx, cy)."""
    qx = abs(px - cx) - (hw - r)
    qy = abs(py - cy) - (hh - r)
    ox, oy = max(qx, 0.0), max(qy, 0.0)
    return math.hypot(ox, oy) + min(max(qx, qy), 0.0) - r


def squircle(px, py, cx, cy, half, r):
    """Apple's continuous-corner rounded square, closely enough: a rounded
    rectangle whose corners blend toward a superellipse."""
    d = rounded_rect(px, py, cx, cy, half, half, r)
    # Pull the corner's shoulders in a touch, as the continuous curve does.
    nx, ny = abs(px - cx) / half, abs(py - cy) / half
    corner = smooth((min(nx, ny) - 0.55) / 0.45)
    return d + corner * r * 0.06


def circle(px, py, cx, cy, r):
    return math.hypot(px - cx, py - cy) - r


def segment(px, py, ax, ay, bx, by, r):
    """Signed distance to a capsule from (ax, ay) to (bx, by), r thick."""
    vx, vy, wx, wy = bx - ax, by - ay, px - ax, py - ay
    t = clamp((wx * vx + wy * vy) / (vx * vx + vy * vy))
    return math.hypot(wx - vx * t, wy - vy * t) - r


def prompt(px, py, cx, cy):
    """A terminal prompt, `>_`, centred on (cx, cy): an agent at work."""
    chevron = min(segment(px, py, cx - 58, cy - 34, cx - 18, cy, 11),
                  segment(px, py, cx - 18, cy, cx - 58, cy + 34, 11))
    cursor = segment(px, py, cx + 4, cy + 34, cx + 56, cy + 34, 11)
    return min(chevron, cursor)


def cover(d, aa):
    """Coverage of a pixel whose centre is d from an edge, aa wide."""
    return clamp(0.5 - d / aa)


class Canvas:
    def __init__(self, size):
        self.size = size
        # Premultiplied RGBA, flat: four floats a pixel.
        self.px = array("d", bytes(8 * 4 * size * size))

    def over(self, i, rgb, a):
        """Source-over a colour with alpha a onto pixel i."""
        if a <= 0:
            return
        p = self.px
        j = 4 * i
        k = 1 - a
        p[j] = rgb[0] * a + p[j] * k
        p[j + 1] = rgb[1] * a + p[j + 1] * k
        p[j + 2] = rgb[2] * a + p[j + 2] * k
        p[j + 3] = a + p[j + 3] * k

    def png(self):
        p = self.px
        raw = bytearray()
        for y in range(self.size):
            raw.append(0)
            for x in range(self.size):
                j = 4 * (y * self.size + x)
                a = p[j + 3]
                for v in (p[j], p[j + 1], p[j + 2]):
                    raw.append(int(clamp(v / a / 255.0) * 255 + 0.5) if a > 0 else 0)
                raw.append(int(clamp(a) * 255 + 0.5))

        def chunk(kind, data):
            body = kind + data
            return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

        header = struct.pack(">IIBBBBB", self.size, self.size, 8, 6, 0, 0, 0)
        return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
                + chunk(b"IEND", b""))


# The four windows, in the squircle's own units (0…824 across).
CELL = 226.0
GAP = 46.0
CELL_RADIUS = 54.0
GRID = CELL * 2 + GAP
ORIGIN = (BODY - GRID) / 2
# The windows sit a little low, leaving sky above them for the stars.
DROP = 22.0
# Which windows are lit: the top right one sleeps.
LIT = [(0, 0), (0, 1), (1, 1)]  # (column, row)
ASLEEP = (1, 0)
STARS = [(132, 96, 5.0, 0.9), (300, 70, 3.4, 0.7), (560, 62, 4.2, 0.8), (708, 118, 3.0, 0.6),
         (84, 236, 2.8, 0.5), (752, 300, 2.6, 0.45)]


def cell_centre(col, row):
    x = ORIGIN + col * (CELL + GAP) + CELL / 2
    y = ORIGIN + DROP + row * (CELL + GAP) + CELL / 2
    return x, y


def draw(size):
    c = Canvas(size)
    s = size / CANVAS  # canvas units to pixels
    aa = 1.0 / s  # one pixel, in canvas units
    body_x0 = (CANVAS - BODY) / 2
    centre = CANVAS / 2
    half = BODY / 2
    detail_stars = size >= 128
    detail_moon = size >= 32
    detail_prompt = size >= 64
    lit = [cell_centre(*w) for w in LIT]
    sleep = cell_centre(*ASLEEP)
    glow_reach = 150.0

    for y in range(size):
        cy = (y + 0.5) / s
        for x in range(size):
            cx = (x + 0.5) / s
            i = y * size + x

            # The shadow under the body: soft, offset down.
            ds = squircle(cx, cy - 12, centre, centre, half, BODY_RADIUS)
            if ds < 90:
                c.over(i, (0, 0, 0), 0.26 * math.exp(-max(ds, 0) / 20.0) * smooth((90 - ds) / 90))

            d = squircle(cx, cy, centre, centre, half, BODY_RADIUS)
            body = cover(d, aa)
            if body <= 0:
                continue
            lx, ly = cx - body_x0, cy - body_x0  # squircle-local
            t = ly / BODY

            # The night: ink, lighter toward the top, with a faint bloom
            # behind the windows.
            rgb = mix(INK_TOP, INK_BOTTOM, smooth(t))
            bloom = math.exp(-((lx - 412) ** 2 + (ly - 470) ** 2) / (2 * 260.0 ** 2))
            rgb = mix(rgb, (0x1B, 0x3B, 0x52), 0.35 * bloom)
            c.over(i, rgb, body)

            if detail_stars:
                for sx, sy, sr, sa in STARS:
                    ddd = circle(lx, ly, sx, sy, sr)
                    if ddd < 12:
                        a = sa * (cover(ddd, aa) + 0.35 * math.exp(-max(ddd, 0) / 4.0))
                        c.over(i, (0xF4, 0xF2, 0xFF), body * clamp(a))

            # The lit windows' glow, spilling onto the night.
            g = 0.0
            for (wx, wy) in lit:
                dw = rounded_rect(lx, ly, wx, wy, CELL / 2, CELL / 2, CELL_RADIUS)
                if 0 < dw < glow_reach:
                    g += math.exp(-dw / 46.0)
            if g > 0:
                c.over(i, GLOW, body * clamp(0.38 * g))

            # The sleeping window: dark glass, a hairline rim, and a moon.
            dw = rounded_rect(lx, ly, sleep[0], sleep[1], CELL / 2, CELL / 2, CELL_RADIUS)
            if dw < 2:
                inside = cover(dw, aa)
                c.over(i, (0x05, 0x06, 0x14), body * inside * 0.55)
                rim = cover(abs(dw + 2.5) - 2.5, aa)
                c.over(i, (0xFF, 0xFF, 0xFF), body * rim * 0.16)
                if detail_moon and inside > 0:
                    mx, my = sleep[0] + 6, sleep[1] + 4
                    moon = max(circle(lx, ly, mx, my, 52), -circle(lx, ly, mx + 28, my - 22, 46))
                    mg = math.exp(-max(moon, 0) / 6.0) * 0.16
                    c.over(i, MOON, body * inside * clamp(cover(moon, aa) * 0.92 + mg))

            # The lit windows: phosphor, brighter at the top, with a soft
            # inner rim of light.
            for (wx, wy) in lit:
                dw = rounded_rect(lx, ly, wx, wy, CELL / 2, CELL / 2, CELL_RADIUS)
                if dw < 2:
                    inside = cover(dw, aa)
                    tt = (ly - (wy - CELL / 2)) / CELL
                    col = mix(PHOSPHOR_TOP, PHOSPHOR_BOTTOM, smooth(tt))
                    # A sheen across the upper third.
                    sheen = clamp(1 - tt / 0.45) * 0.22
                    col = mix(col, (255, 255, 255), sheen)
                    # Deeper toward the edges, so each window reads as glass.
                    edge = math.exp(-max(-dw, 0) / 22.0)
                    col = mix(col, PHOSPHOR_BOTTOM, 0.35 * edge * tt)
                    c.over(i, col, body * inside)
                    # The first window shows what's inside: a prompt.
                    if detail_prompt and (wx, wy) == lit[0]:
                        dp = prompt(lx, ly, wx, wy + 6)
                        c.over(i, (0x0B, 0x6B, 0x52), body * inside * cover(dp, aa) * 0.78)
                    break

            # Glass on the whole body: a rim of light along the top edge.
            rim = cover(abs(d + 3) - 3, aa) * clamp(1 - t * 1.6)
            c.over(i, (255, 255, 255), body * rim * 0.22)
    return c


# iconutil's names for each size.
ICONSET = [
    ("icon_16x16.png", 16), ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32), ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128), ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256), ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512), ("icon_512x512@2x.png", 1024),
]


def main(argv):
    if len(argv) >= 2 and argv[0] == "--preview":
        size = int(argv[2]) if len(argv) > 2 else 512
        with open(argv[1], "wb") as f:
            f.write(draw(size).png())
        return 0
    if len(argv) != 1:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    out = argv[0]
    os.makedirs(out, exist_ok=True)
    drawn = {}
    for name, size in ICONSET:
        if size not in drawn:
            drawn[size] = draw(size).png()
        with open(os.path.join(out, name), "wb") as f:
            f.write(drawn[size])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
