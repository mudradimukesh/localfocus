#!/usr/bin/env python3
import sys
from pathlib import Path


VERSION = 4
SIZE = 17 + VERSION * 4
DATA_CODEWORDS = 80
EC_CODEWORDS = 20


def gf_tables():
    exp = [0] * 512
    log = [0] * 256
    x = 1
    for i in range(255):
        exp[i] = x
        log[x] = i
        x <<= 1
        if x & 0x100:
            x ^= 0x11D
    for i in range(255, 512):
        exp[i] = exp[i - 255]
    return exp, log


GF_EXP, GF_LOG = gf_tables()


def gf_mul(a, b):
    if a == 0 or b == 0:
        return 0
    return GF_EXP[GF_LOG[a] + GF_LOG[b]]


def rs_generator(degree):
    poly = [1]
    for i in range(degree):
        next_poly = [0] * (len(poly) + 1)
        for j, coef in enumerate(poly):
            next_poly[j] ^= coef
            next_poly[j + 1] ^= gf_mul(coef, GF_EXP[i])
        poly = next_poly
    return poly


def rs_remainder(data, degree):
    generator = rs_generator(degree)
    result = [0] * degree
    for value in data:
        factor = value ^ result[0]
        result = result[1:] + [0]
        for i in range(degree):
            result[i] ^= gf_mul(generator[i + 1], factor)
    return result


class BitBuffer:
    def __init__(self):
        self.bits = []

    def append(self, value, length):
        for i in reversed(range(length)):
            self.bits.append((value >> i) & 1)

    def bytes(self):
        return [
            sum(bit << (7 - i) for i, bit in enumerate(self.bits[j : j + 8]))
            for j in range(0, len(self.bits), 8)
        ]


def make_codewords(text):
    payload = text.encode("utf-8")
    if len(payload) > 76:
        raise ValueError("This fixed QR generator supports URLs up to 76 bytes")

    bits = BitBuffer()
    bits.append(0b0100, 4)
    bits.append(len(payload), 8)
    for byte in payload:
        bits.append(byte, 8)

    capacity_bits = DATA_CODEWORDS * 8
    bits.append(0, min(4, capacity_bits - len(bits.bits)))
    while len(bits.bits) % 8:
        bits.append(0, 1)

    data = bits.bytes()
    pad = 0xEC
    while len(data) < DATA_CODEWORDS:
        data.append(pad)
        pad = 0x11 if pad == 0xEC else 0xEC

    return data + rs_remainder(data, EC_CODEWORDS)


def blank_matrix():
    return [[False] * SIZE for _ in range(SIZE)], [[False] * SIZE for _ in range(SIZE)]


def set_module(matrix, reserved, row, col, value, reserve=True):
    if 0 <= row < SIZE and 0 <= col < SIZE:
        matrix[row][col] = bool(value)
        if reserve:
            reserved[row][col] = True


def finder(matrix, reserved, row, col):
    for r in range(-1, 8):
        for c in range(-1, 8):
            rr, cc = row + r, col + c
            if not (0 <= rr < SIZE and 0 <= cc < SIZE):
                continue
            value = (
                0 <= r <= 6
                and 0 <= c <= 6
                and (r in (0, 6) or c in (0, 6) or (2 <= r <= 4 and 2 <= c <= 4))
            )
            set_module(matrix, reserved, rr, cc, value)


def alignment(matrix, reserved, center_row, center_col):
    for r in range(-2, 3):
        for c in range(-2, 3):
            value = max(abs(r), abs(c)) != 1
            set_module(matrix, reserved, center_row + r, center_col + c, value)


def draw_function_patterns(matrix, reserved):
    finder(matrix, reserved, 0, 0)
    finder(matrix, reserved, 0, SIZE - 7)
    finder(matrix, reserved, SIZE - 7, 0)
    alignment(matrix, reserved, 26, 26)

    for i in range(SIZE):
        if not reserved[6][i]:
            set_module(matrix, reserved, 6, i, i % 2 == 0)
        if not reserved[i][6]:
            set_module(matrix, reserved, i, 6, i % 2 == 0)

    set_module(matrix, reserved, 4 * VERSION + 9, 8, True)

    for i in range(9):
        if i != 6:
            reserved[8][i] = True
            reserved[i][8] = True
    for i in range(8):
        reserved[8][SIZE - 1 - i] = True
        reserved[SIZE - 1 - i][8] = True


def mask_bit(mask, row, col):
    if mask == 0:
        return (row + col) % 2 == 0
    raise ValueError("Only mask 0 is implemented")


def draw_data(matrix, reserved, codewords, mask=0):
    bits = []
    for byte in codewords:
        for i in reversed(range(8)):
            bits.append((byte >> i) & 1)

    index = 0
    direction = -1
    col = SIZE - 1
    row = SIZE - 1
    while col > 0:
        if col == 6:
            col -= 1
        while 0 <= row < SIZE:
            for c in (col, col - 1):
                if not reserved[row][c]:
                    bit = bits[index] if index < len(bits) else 0
                    value = bool(bit) ^ mask_bit(mask, row, c)
                    set_module(matrix, reserved, row, c, value, reserve=False)
                    index += 1
            row += direction
        direction *= -1
        row += direction
        col -= 2


def format_bits(mask=0):
    data = (0b01 << 3) | mask
    value = data << 10
    generator = 0b10100110111
    for i in reversed(range(10, 15)):
        if (value >> i) & 1:
            value ^= generator << (i - 10)
    return ((data << 10) | value) ^ 0b101010000010010


def bit(value, index):
    return (value >> index) & 1


def draw_format(matrix, reserved, mask=0):
    bits = format_bits(mask)
    for i in range(6):
        set_module(matrix, reserved, 8, i, bit(bits, i))
    set_module(matrix, reserved, 8, 7, bit(bits, 6))
    set_module(matrix, reserved, 8, 8, bit(bits, 7))
    set_module(matrix, reserved, 7, 8, bit(bits, 8))
    for i in range(9, 15):
        set_module(matrix, reserved, 14 - i, 8, bit(bits, i))

    for i in range(8):
        set_module(matrix, reserved, SIZE - 1 - i, 8, bit(bits, i))
    for i in range(8, 15):
        set_module(matrix, reserved, 8, SIZE - 15 + i, bit(bits, i))
    set_module(matrix, reserved, SIZE - 8, 8, True)


def qr_matrix(text):
    matrix, reserved = blank_matrix()
    draw_function_patterns(matrix, reserved)
    draw_data(matrix, reserved, make_codewords(text), mask=0)
    draw_format(matrix, reserved, mask=0)
    return matrix


def svg(matrix, label):
    border = 4
    scale = 10
    size = (SIZE + border * 2) * scale
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {SIZE + border * 2} {SIZE + border * 2}" role="img" aria-label="{label}">',
        '<rect width="100%" height="100%" fill="#fff"/>',
    ]
    for row in range(SIZE):
        for col in range(SIZE):
            if matrix[row][col]:
                parts.append(f'<rect x="{col + border}" y="{row + border}" width="1" height="1" fill="#000"/>')
    parts.append("</svg>")
    return "\n".join(parts)


def main():
    if len(sys.argv) != 4:
        print("usage: make_qr_svg.py TEXT OUTPUT.svg LABEL", file=sys.stderr)
        return 2
    text, output, label = sys.argv[1], sys.argv[2], sys.argv[3]
    Path(output).write_text(svg(qr_matrix(text), label), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
