"""在绝对屏幕坐标点击（物理像素），用于 UI 核对脚本。

用法: python clickat.py <x> <y> [delay]
坐标是屏幕物理像素（已声明 DPI 感知），窗口外框坐标可由 wininfo 或
capture_window.py 打印的 rect 推算。
"""
import ctypes
import sys
import time

u = ctypes.WinDLL("user32")
try:
    ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)
except Exception:
    u.SetProcessDPIAware()


def click(x, y, delay=0.8):
    u.SetCursorPos(int(x), int(y))
    time.sleep(0.25)
    u.mouse_event(0x0002, 0, 0, 0, 0)   # LEFTDOWN
    time.sleep(0.08)
    u.mouse_event(0x0004, 0, 0, 0, 0)   # LEFTUP
    time.sleep(delay)


if __name__ == "__main__":
    x = int(sys.argv[1])
    y = int(sys.argv[2])
    d = float(sys.argv[3]) if len(sys.argv) > 3 else 0.8
    click(x, y, d)
    print("clicked (%d,%d)" % (x, y))
