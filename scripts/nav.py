"""点击 RouteTool 侧栏菜单切页，用于截图核对布局。

用法: python nav.py <home|config|logs|about>

坐标按「窗口外框 + 物理像素偏移」计算：
- 必须先声明 DPI 感知，否则 GetWindowRect 返回虚拟化坐标（除以缩放因子），
  而 SetCursorPos 需要物理坐标，两者混用会点错位置。
- 菜单项在客户区中的 y 偏移（逻辑 DIP）：侧栏顶部品牌区约 100，之后每项间隔 56，
  菜单项高 40 -> 中心分别约 132 / 188 / 244 / 300。
"""
import ctypes
import sys
import time
from ctypes import wintypes

u = ctypes.WinDLL("user32")

try:
    ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)   # PER_MONITOR_DPI_AWARE
except Exception:
    u.SetProcessDPIAware()


class RECT(ctypes.Structure):
    _fields_ = [("l", ctypes.c_long), ("t", ctypes.c_long),
                ("r", ctypes.c_long), ("b", ctypes.c_long)]


def find_window():
    found = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def cb(h, l):
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

    u.EnumWindows(cb, 0)
    return found


# 窗口外框内的物理像素坐标 -> 各菜单项中心（由 shot-r3.png 像素扫描实测）。
# 注意：这些是「物理像素」，与 DPI 缩放无关；不要再用逻辑 DIP 乘 scale，
# 否则 125% 缩放下会点到错误位置。
MENU_Y_PHYS = {"home": 143, "config": 198, "logs": 256, "about": 309}
MENU_X_PHYS = 100


def click(cx, cy, delay=0.8):
    u.SetCursorPos(cx, cy)
    time.sleep(0.25)
    u.mouse_event(0x0002, 0, 0, 0, 0)   # LEFTDOWN
    time.sleep(0.08)
    u.mouse_event(0x0004, 0, 0, 0, 0)   # LEFTUP
    time.sleep(delay)


if __name__ == "__main__":
    target = sys.argv[1] if len(sys.argv) > 1 else "config"
    wins = find_window()
    if not wins:
        print("no-window")
        sys.exit(1)
    h = wins[0]
    u.ShowWindow(h, 9)          # SW_RESTORE（SW_SHOW 对已最小化窗口无效）
    u.SetForegroundWindow(h)
    time.sleep(1.0)

    rc = RECT()
    u.GetWindowRect(h, ctypes.byref(rc))
    cx = rc.l + MENU_X_PHYS
    cy = rc.t + MENU_Y_PHYS[target]
    click(cx, cy)
    print(f"clicked {target} at ({cx},{cy}) window=({rc.l},{rc.t},{rc.r},{rc.b})")
