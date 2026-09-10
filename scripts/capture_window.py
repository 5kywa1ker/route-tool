"""Capture the RouteTool window rectangle via Win32 + Pillow-free BMP write.

Uses ctypes only (no Add-Type, no compilation) so it works under the sandbox.
"""
import ctypes
from ctypes import wintypes
import struct
import sys
import time

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)

# 声明 DPI 感知：否则在 125% 缩放下 GetWindowRect/PrintWindow 都返回
# 被虚拟化（除以 1.25）的坐标，截出来的图会被缩小且与真实像素不符。
try:
    ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)   # PROCESS_PER_MONITOR_DPI_AWARE
except Exception:
    user32.SetProcessDPIAware()


class RECT(ctypes.Structure):
    _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                ("right", ctypes.c_long), ("bottom", ctypes.c_long)]


def find_window(title_part: str):
    found = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, lparam):
        if not user32.IsWindowVisible(hwnd):
            return True
        n = user32.GetWindowTextLengthW(hwnd)
        if n == 0:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        if title_part in buf.value:
            found.append((hwnd, buf.value))
        return True

    user32.EnumWindows(cb, 0)
    return found


def capture(hwnd, out_path):
    user32.ShowWindow(hwnd, 5)          # SW_SHOW
    user32.SetForegroundWindow(hwnd)
    time.sleep(1.2)
    for _ in range(40):
        if not user32.IsIconic(hwnd):
            break
        time.sleep(0.1)

    r = RECT()
    if not user32.GetWindowRect(hwnd, ctypes.byref(r)):
        raise OSError("GetWindowRect failed")
    w = r.right - r.left
    h = r.bottom - r.top
    print(f"rect={w}x{h} at ({r.left},{r.top})")

    hdc = user32.GetWindowDC(hwnd)
    memdc = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    gdi32.SelectObject(memdc, bmp)

    # PW_RENDERFULLCONTENT = 2 -> captures DWM-composited content reliably
    ok = user32.PrintWindow(hwnd, memdc, 2)
    if not ok:
        gdi32.BitBlt(memdc, 0, 0, w, h, hdc, 0, 0, 0x00CC0020)

    # BITMAPINFOHEADER
    class BMIH(ctypes.Structure):
        _fields_ = [("biSize", wintypes.DWORD), ("biWidth", ctypes.c_long),
                    ("biHeight", ctypes.c_long), ("biPlanes", wintypes.WORD),
                    ("biBitCount", wintypes.WORD), ("biCompression", wintypes.DWORD),
                    ("biSizeImage", wintypes.DWORD), ("biXPelsPerMeter", ctypes.c_long),
                    ("biYPelsPerMeter", ctypes.c_long), ("biClrUsed", wintypes.DWORD),
                    ("biClrImportant", wintypes.DWORD)]

    hdr = BMIH()
    hdr.biSize = ctypes.sizeof(BMIH)
    hdr.biWidth = w
    hdr.biHeight = -h          # negative => top-down
    hdr.biPlanes = 1
    hdr.biBitCount = 32
    hdr.biCompression = 0      # BI_RGB

    stride = w * 4
    buf = ctypes.create_string_buffer(stride * h)
    got = gdi32.GetDIBits(memdc, bmp, 0, h, buf, ctypes.byref(hdr), 0)
    if got == 0:
        raise OSError("GetDIBits failed")

    # write BMP (BGRA, top-down input -> flip for positive-height BMP)
    pixel_offset = 14 + 40
    filesize = pixel_offset + stride * h
    with open(out_path, "wb") as f:
        f.write(b"BM")
        f.write(struct.pack("<IHHI", filesize, 0, 0, pixel_offset))
        f.write(struct.pack("<IiiHHIIiiII", 40, w, h, 1, 32, 0, stride * h, 2835, 2835, 0, 0))
        raw = buf.raw
        for row in range(h - 1, -1, -1):       # BMP wants bottom-up
            f.write(raw[row * stride:(row + 1) * stride])

    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(memdc)
    user32.ReleaseDC(hwnd, hdc)
    print(f"saved={out_path}")


if __name__ == "__main__":
    title = sys.argv[1] if len(sys.argv) > 1 else "RouteTool"
    out = sys.argv[2] if len(sys.argv) > 2 else r"C:\Users\hfbco\AppData\Local\Temp\rt-shot.bmp"
    wins = find_window(title)
    print("matched:", [(hex(h), t) for h, t in wins])
    if not wins:
        print("no-window")
        sys.exit(1)
    capture(wins[0][0], out)
