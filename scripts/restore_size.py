"""把 RouteTool 窗口恢复为设计尺寸（880x600 逻辑 = 1100x750 物理 @125%）。"""
import ctypes
import time
from ctypes import wintypes

u = ctypes.WinDLL("user32")
try:
    ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)
except Exception:
    u.SetProcessDPIAware()


class RECT(ctypes.Structure):
    _fields_ = [("l", ctypes.c_long), ("t", ctypes.c_long),
                ("r", ctypes.c_long), ("b", ctypes.c_long)]


def find():
    out = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def cb(h, l):
        if u.IsWindowVisible(h) and u.GetWindowTextLengthW(h):
            b = ctypes.create_unicode_buffer(u.GetWindowTextLengthW(h) + 1)
            u.GetWindowTextW(h, b, len(b))
            if "RouteTool" in b.value:
                out.append(h)
        return True

    u.EnumWindows(cb, 0)
    return out


h = find()[0]
u.ShowWindow(h, 9)
time.sleep(0.4)
# 期望客户区 1100x750；用 AdjustWindowRectEx 反推外框
dpi = u.GetDpiForWindow(h)
scale = dpi / 96.0


class RECTEX(ctypes.Structure):
    _fields_ = [("l", ctypes.c_long), ("t", ctypes.c_long),
                ("r", ctypes.c_long), ("b", ctypes.c_long)]


r = RECTEX(0, 0, int(880 * scale), int(600 * scale))
u.AdjustWindowRectEx(ctypes.byref(r), u.GetWindowLongW(h, -16), False, u.GetWindowLongW(h, -20))
w = r.r - r.l
ht = r.b - r.t
cur = RECT()
u.GetWindowRect(h, ctypes.byref(cur))
u.MoveWindow(h, cur.l, cur.t, w, ht, True)
time.sleep(0.5)
u.GetWindowRect(h, ctypes.byref(cur))
cr = RECT()
u.GetClientRect(h, ctypes.byref(cr))
print(f"window=({cur.l},{cur.t},{cur.r},{cur.b}) outer={cur.r-cur.l}x{cur.b-cur.t} "
      f"client={cr.r}x{cr.b} dpi={dpi} scale={scale}")
