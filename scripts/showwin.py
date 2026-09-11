"""把 RouteTool 窗口显示出来并置前（含「窗口存在但 IsWindowVisible=False」的情况）。

背景：从自动化会话里拉起的 UI 进程，窗口有时会被这层会话隐藏
（EnumWindows 能找到句柄、标题、尺寸，但 IsWindowVisible 为 False），
capture_window.py 的 find_window 只匹配可见窗口，于是表现为「没有窗口」。
这里不依赖可见性来找句柄，找到后强制 ShowWindow + SetForegroundWindow。

用法: python showwin.py
"""
import ctypes
import time
from ctypes import wintypes

u = ctypes.WinDLL("user32")
try:
    ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)
except Exception:
    u.SetProcessDPIAware()

SW_RESTORE = 9
SW_SHOW = 5
found = []


@ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
def cb(h, l):
    n = u.GetWindowTextLengthW(h)
    if n == 0:
        return True
    b = ctypes.create_unicode_buffer(n + 1)
    u.GetWindowTextW(h, b, n + 1)
    if "RouteTool" in b.value:
        r = wintypes.RECT()
        u.GetWindowRect(h, ctypes.byref(r))
        # 排除 0 尺寸的幽灵窗口
        if r.right - r.left > 200:
            found.append((h, b.value, bool(u.IsWindowVisible(h)),
                          (r.left, r.top, r.right, r.bottom)))
    return True


u.EnumWindows(cb, 0)
if not found:
    print("no-window")
    raise SystemExit(1)
for h, title, vis, rect in found:
    if not vis:
        u.ShowWindow(h, SW_RESTORE)
        time.sleep(0.4)
        u.ShowWindow(h, SW_SHOW)
    u.SetForegroundWindow(h)
    time.sleep(0.6)
    print("hwnd=%s visible_before=%s visible_now=%s rect=%s title=%r"
          % (hex(h), vis, bool(u.IsWindowVisible(h)), rect, title))
