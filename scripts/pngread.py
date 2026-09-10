"""PNG 解码 + 简单像素分析（纯标准库，无 Pillow 依赖）。

用法:
  python pngread.py <file.png> info
  python pngread.py <file.png> col <x> <y0> <y1> [step]
  python pngread.py <file.png> row <y> <x0> <x1> [step]
"""
import struct
import sys
import zlib


def load(path):
    d = open(path, "rb").read()
    pos = 8
    w = h = None
    idat = b""
    while pos < len(d):
        ln = struct.unpack(">I", d[pos:pos + 4])[0]
        typ = d[pos + 4:pos + 8]
        data = d[pos + 8:pos + 8 + ln]
        if typ == b"IHDR":
            w, h, bd, ct = struct.unpack(">IIBB", data[:10])
            if bd != 8 or ct not in (2, 6):
                raise ValueError(f"unsupported png: bitdepth={bd} colortype={ct}")
            bpp = 3 if ct == 2 else 4
        elif typ == b"IDAT":
            idat += data
        elif typ == b"IEND":
            break
        pos += 12 + ln

    raw = zlib.decompress(idat)
    stride = w * bpp
    prev = bytearray(stride)
    rows = []
    p = 0
    for _ in range(h):
        f = raw[p]
        p += 1
        line = bytearray(raw[p:p + stride])
        p += stride
        if f == 1:
            for i in range(bpp, stride):
                line[i] = (line[i] + line[i - bpp]) & 255
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 255
        elif f == 3:
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 255
        elif f == 4:
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                pp = a + b - c
                pa, pb, pc = abs(pp - a), abs(pp - b), abs(pp - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        rows.append(bytes(line))
        prev = line
    return w, h, bpp, rows


class Img:
    def __init__(self, path):
        self.w, self.h, self.bpp, self.rows = load(path)

    def px(self, x, y):
        r = self.rows[y]
        o = x * self.bpp
        return r[o], r[o + 1], r[o + 2]


if __name__ == "__main__":
    path = sys.argv[1]
    cmd = sys.argv[2] if len(sys.argv) > 2 else "info"
    im = Img(path)
    if cmd == "info":
        print(f"size={im.w}x{im.h} bpp={im.bpp}")
    elif cmd == "col":
        x, y0, y1 = int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
        step = int(sys.argv[6]) if len(sys.argv) > 6 else 1
        for y in range(y0, y1, step):
            print(y, im.px(x, y))
    elif cmd == "row":
        y, x0, x1 = int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
        step = int(sys.argv[6]) if len(sys.argv) > 6 else 1
        for x in range(x0, x1, step):
            print(x, im.px(x, y))
    else:
        print("unknown cmd")
