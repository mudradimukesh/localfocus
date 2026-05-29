#!/usr/bin/env python3
import os
import struct
import sys
import zlib


def chunk(kind, data):
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    )


def write_png(path, size):
    bg = (36, 115, 77, 255)
    accent = (53, 92, 125, 255)
    white = (255, 255, 255, 255)
    dark = (24, 26, 22, 255)
    rows = []
    for y in range(size):
        row = bytearray([0])
        for x in range(size):
            nx = x / max(1, size - 1)
            ny = y / max(1, size - 1)
            r = int(accent[0] * (1 - nx) + bg[0] * nx)
            g = int(accent[1] * (1 - nx) + bg[1] * nx)
            b = int(accent[2] * (1 - nx) + bg[2] * nx)
            a = 255

            margin = size * 0.18
            bar_w = size * 0.12
            in_l = margin <= x <= margin + bar_w and margin <= y <= size - margin
            in_f_v = size * 0.44 <= x <= size * 0.56 and margin <= y <= size - margin
            in_f_top = size * 0.44 <= x <= size * 0.80 and margin <= y <= margin + bar_w
            in_f_mid = size * 0.44 <= x <= size * 0.72 and size * 0.44 <= y <= size * 0.56
            if in_l or in_f_v or in_f_top or in_f_mid:
                r, g, b, a = white

            radius = size * 0.18
            dx = min(x, size - 1 - x)
            dy = min(y, size - 1 - y)
            if dx < radius and dy < radius:
                cx = radius if x < size / 2 else size - 1 - radius
                cy = radius if y < size / 2 else size - 1 - radius
                if ((x - cx) ** 2 + (y - cy) ** 2) > radius**2:
                    r, g, b, a = dark[0], dark[1], dark[2], 0

            row.extend([r, g, b, a])
        rows.append(bytes(row))

    raw = b"".join(rows)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as file:
        file.write(png)


def main():
    out_dir = sys.argv[1]
    os.makedirs(out_dir, exist_ok=True)
    sizes = [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ]
    for size, name in sizes:
        write_png(os.path.join(out_dir, name), size)


if __name__ == "__main__":
    main()
