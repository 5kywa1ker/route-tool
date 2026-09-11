"""调整 RouteTool 窗口尺寸（物理像素），用于把长页面整页铺开截图。

合成滚轮/滚动条拖拽推不动 Slint 的 ScrollView，所以要看长页面下半部分时
只能把窗口拉高，再用 PrintWindow 抓取（PrintWindow 会渲染窗口自身缓冲，
超出屏幕的部分也能截到）。

用法: python setsize.py [width] [height]
不传参数 = 还原为 1118x797（本机 1100x750 逻辑、DPI 125% 的默认外框尺寸）。
"""
import ctypes
import sys
import time

u = ctypes.WinDLL("user32")
try:
    ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)
except Exception:
    u.SetProcessDPIAware()


@ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)
def _cb(h, l):
    if not u.IsWindowVisible(h):
        return True
    n = u.GetWindowTextLengthW(h)
    if n == 0:
        return True
    b = ctypes.create_unicode_buffer(n + 1)
    u.GetWindowTextW(h, b, n + 1)
    if "RouteTool" in b.value:
        found.append(h)
    return True


found = []
u.EnumWindows(_cb, 0)
if not found:
    print("no-window")
    sys.exit(1)

hwnd = found[0]
w = int(sys.argv[1]) if len(sys.argv) > 1 else 1118
h = int(sys.argv[2]) if len(sys.argv) > 2 else 797
u.ShowWindow(hwnd, 9)                       # SW_RESTORE
u.SetWindowPos(hwnd, 0, 32, 0, w, h, 0x0004)  # SWP_NOZORDER
u.SetForegroundWindow(hwnd)
time.sleep(1.2)
print("set %dx%d" % (w, h))
