"""Draws the HDEC icon: HDEC.icns and icon.png here, ../windows/hdec.ico.
Needs Pillow and macOS `iconutil`: python3 packaging/macos/make-icon.py"""
import os, subprocess, tempfile
from PIL import Image, ImageDraw, ImageFilter

S = 4  # supersampling
N = 1024 * S
here = os.path.dirname(os.path.abspath(__file__))


def icon():
    img = Image.new("RGBA", (N, N), (0, 0, 0, 0))
    # Apple's grid: an 824 px tile centred on the 1024 canvas, soft shadow below.
    tile = (100 * S, 100 * S, 924 * S, 924 * S)
    shadow = Image.new("RGBA", (N, N), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle(
        (tile[0], tile[1] + 12 * S, tile[2], tile[3] + 12 * S), 185 * S, fill=(0, 0, 0, 90)
    )
    img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(14 * S)))
    # Blue vertical gradient (the app's accent colours), clipped to the tile.
    top, bottom = (0x5B, 0x97, 0xFF), (0x23, 0x5F, 0xE0)
    grad = Image.new("RGBA", (N, N))
    gd = ImageDraw.Draw(grad)
    for y in range(N):
        t = min(max((y - tile[1]) / (tile[3] - tile[1]), 0), 1)
        gd.line([(0, y), (N, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)) + (255,))
    mask = Image.new("L", (N, N), 0)
    ImageDraw.Draw(mask).rounded_rectangle(tile, 185 * S, fill=255)
    img.paste(grad, (0, 0), mask)

    d = ImageDraw.Draw(img)
    white = (255, 255, 255, 255)
    blue = bottom + (255,)
    # Archive box: lid and body, together centred on the tile (y 297-727).
    d.rounded_rectangle((270 * S, 297 * S, 754 * S, 397 * S), 26 * S, fill=white)
    d.rounded_rectangle((300 * S, 417 * S, 724 * S, 727 * S), 30 * S, fill=white)
    # Zipper down the middle: teeth alternate left and right.
    cx = 512 * S
    for i, y in enumerate(range(419, 607, 28)):
        x0 = cx - (34 if i % 2 == 0 else 0) * S
        d.rounded_rectangle((x0, y * S, x0 + 34 * S, (y + 18) * S), 5 * S, fill=blue)
    # Pull tab.
    d.rounded_rectangle((cx - 30 * S, 607 * S, cx + 30 * S, 687 * S), 16 * S, fill=blue)
    d.rounded_rectangle((cx - 12 * S, 627 * S, cx + 12 * S, 667 * S), 8 * S, fill=white)
    return img.resize((1024, 1024), Image.LANCZOS)


big = icon()
with tempfile.TemporaryDirectory() as tmp:
    iconset = os.path.join(tmp, "HDEC.iconset")
    os.mkdir(iconset)
    for size in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            px = size * scale
            name = f"icon_{size}x{size}{'@2x' if scale == 2 else ''}.png"
            big.resize((px, px), Image.LANCZOS).save(os.path.join(iconset, name))
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(here, "HDEC.icns")], check=True)
big.save(os.path.join(here, "icon.png"))
# Windows icons fill their square: just the tile, without the Mac margin.
tile = big.crop((92, 92, 932, 932))
win = os.path.join(here, "..", "windows")
os.makedirs(win, exist_ok=True)
tile.save(os.path.join(win, "hdec.ico"), sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
