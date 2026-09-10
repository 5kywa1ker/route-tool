import ctypes
from ctypes import wintypes

u32 = ctypes.windll.user32
u32.SetProcessDPIAware()

EnumWindows = u32.EnumWindows
EnumWindowsProc = ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)

found = []

def cb(hwnd, lp):
    if not u32.IsWindowVisible(hwnd):
        return True
    n = u32.GetWindowTextLengthW(hwnd)
    if n == 0:
        return True
    buf = ctypes.create_unicode_buffer(n + 1)
    u32.GetWindowTextW(hwnd, buf, n + 1)
    if 'RouteTool' in buf.value:
        rc = wintypes.RECT()
        u32.GetWindowRect(hwnd, ctypes.byref(rc))
        cr = wintypes.RECT()
        u32.GetClientRect(hwnd, ctypes.byref(cr))
        try:
            dpi = u32.GetDpiForWindow(hwnd)
        except Exception:
            dpi = -1
        found.append((hk := hwnd, buf.value, (rc.left, rc.top, rc.right, rc.bottom),
                      (cr.right - cr.left, cr.bottom - cr.top), dpi))
    return True

EnumWindows(EnumWindowsProc(cb), 0)
for f in found:
    print('hwnd=%s title=%r rect=%s client=%s GetDpiForWindow=%s' % f)
print('GetDpiForSystem=%s' % u32.GetDpiForSystem())
if not found:
    print('NO WINDOW FOUND')
