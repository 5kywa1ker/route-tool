"""生成 Bypass Tool 应用图标。

产出（全部落在仓库根 assets/）：
  app.png            256x256 主图（Slint 窗口图标 / 预览用）
  app.ico            多尺寸 ICO（exe 资源图标 + Inno Setup 安装图标）
  tray_<state>.rgba  32x32 原始 RGBA（托盘三态，运行时 include_bytes! 直接加载，无需解码依赖）

设计：圆角方形渐变蓝底 + 白色「一路进、两路出」的路由分叉符号
（左侧节点 = 本机，右侧上下两支 = 直连 / 旁路由），保证缩到 16x16 仍可辨识。
渐变自带明暗过渡，贴近 Win11 Fluent 图标的质感。

用法：python scripts/gen_icons.py
"""

import math
import os

from PIL import Image, ImageDraw

S = 256  # 设计画布
# 渐变底：左上亮蓝 -> 右下深蓝（Win11 Fluent 质感）
BG_TL = (0x3E, 0x7B, 0xFF, 255)
BG_BR = (0x0A, 0x47, 0xC8, 255)
FG = (255, 255, 255, 255)  # 白色符号

# 托盘三态底色（直连=灰 / 旁路由=绿 / 回退=红）
TRAY_STATES = {
    "direct": (0x9E, 0x9E, 0x9E, 255),
    "bypass": (0x2E, 0xE7, 0x4B, 255),
    "fallback": (0xE5, 0x39, 0x35, 255),
}

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "assets")


def _arrow(d, a, b, w, color, head=52, hw=56):
    """带圆头起点与三角箭头的粗线。"""
    d.line([a, b], fill=color, width=w)
    d.ellipse([a[0] - w / 2, a[1] - w / 2, a[0] + w / 2, a[1] + w / 2], fill=color)

    dx, dy = b[0] - a[0], b[1] - a[1]
    length = math.hypot(dx, dy)
    ux, uy = dx / length, dy / length
    px, py = -uy, ux  # 垂直方向

    tip = (b[0] + ux * head * 0.40, b[1] + uy * head * 0.40)
    base = (b[0] - ux * head * 0.60, b[1] - uy * head * 0.60)
    d.polygon(
        [
            tip,
            (base[0] + px * hw / 2, base[1] + py * hw / 2),
            (base[0] - px * hw / 2, base[1] - py * hw / 2),
        ],
        fill=color,
    )


def _gradient_rect(size, pad, radius, c_tl, c_br):
    """圆角方形 + 对角渐变（左上亮 -> 右下深），返回 RGBA Image。"""
    w = h = size
    img = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    px = img.load()
    denom = max(w + h, 1)
    dr = c_br[0] - c_tl[0]
    dg = c_br[1] - c_tl[1]
    db = c_br[2] - c_tl[2]
    for y in range(h):
        for x in range(w):
            t = (x + y) / denom
            px[x, y] = (
                int(c_tl[0] + dr * t),
                int(c_tl[1] + dg * t),
                int(c_tl[2] + db * t),
                255,
            )
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [pad, pad, w - pad - 1, h - pad - 1], radius=radius, fill=255
    )
    out = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    out.paste(img, (0, 0), mask)
    return out


def render(bg, size=S, gradient=False):
    """在 size x size 画布上绘制一枚图标（RGBA）。
    gradient=True 时用渐变底（主图标），否则用纯色 bg（托盘三态）。
    """
    pad, radius = 6, 60

    if gradient:
        img = _gradient_rect(S, pad, radius, BG_TL, BG_BR)
    else:
        img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
        ImageDraw.Draw(img).rounded_rectangle(
            [pad, pad, S - pad - 1, S - pad - 1], radius=radius, fill=bg
        )
    d = ImageDraw.Draw(img)

    # 左侧本机节点
    cx = cy = 128.0
    node_x, r = 70.0, 37.0
    d.ellipse([node_x - r, cy - r, node_x + r, cy + r], fill=FG)

    # 主干：节点 -> 分叉点
    fork_x = 146.0
    d.line([(node_x, cy), (fork_x, cy)], fill=FG, width=34)
    d.ellipse([fork_x - 17, cy - 17, fork_x + 17, cy + 17], fill=FG)

    # 两支：上=旁路由，下=直连
    _arrow(d, (fork_x, cy), (204, 70), 30, FG)
    _arrow(d, (fork_x, cy), (204, 186), 30, FG)

    if size != S:
        img = img.resize((size, size), Image.LANCZOS)
    return img


def main():
    os.makedirs(OUT, exist_ok=True)

    master = render(BG_TL, S, gradient=True)
    master.save(os.path.join(OUT, "app.png"))

    # ICO：Windows 资源与安装包常用尺寸
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    master.save(
        os.path.join(OUT, "app.ico"),
        format="ICO",
        sizes=[(s, s) for s in sizes],
    )

    # 托盘三态：32x32 原始 RGBA，运行时零解码
    for name, color in TRAY_STATES.items():
        img = render(color, 32).convert("RGBA")
        path = os.path.join(OUT, f"tray_{name}.rgba")
        with open(path, "wb") as f:
            f.write(img.tobytes())

    print(f"assets written to {OUT}")
    for f in sorted(os.listdir(OUT)):
        print(f"  {f:20} {os.path.getsize(os.path.join(OUT, f)):>8} bytes")


if __name__ == "__main__":
    main()
