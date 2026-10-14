"""Generates assets/app.ico: a cursor crossing from a monitor to a laptop.

Run: python assets/make_icon.py   (needs Pillow)
"""
from pathlib import Path
from PIL import Image, ImageDraw

S = 1024  # draw big, downsample for clean edges
OUT = Path(__file__).with_name("app.ico")


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def background(size):
    top, bottom = (59, 130, 246), (67, 56, 202)
    grad = Image.new("RGB", (size, size))
    d = ImageDraw.Draw(grad)
    for y in range(size):
        d.line([(0, y), (size, y)], fill=lerp(top, bottom, y / (size - 1)))
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, size - 1, size - 1], radius=size * 0.22, fill=255)
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    img.paste(grad, (0, 0), mask)
    return img


def detailed():
    img = background(S)
    d = ImageDraw.Draw(img)
    white, screen = (255, 255, 255, 255), (30, 41, 120, 255)
    u = S / 100
    # monitor (left)
    d.rounded_rectangle([10 * u, 22 * u, 56 * u, 58 * u], radius=4 * u, fill=white)
    d.rectangle([14 * u, 26 * u, 52 * u, 54 * u], fill=screen)
    d.rectangle([29 * u, 58 * u, 37 * u, 66 * u], fill=white)
    d.rounded_rectangle([21 * u, 65 * u, 45 * u, 70 * u], radius=2 * u, fill=white)
    # laptop (right)
    d.rounded_rectangle([58 * u, 42 * u, 88 * u, 66 * u], radius=3 * u, fill=white)
    d.rectangle([61 * u, 45 * u, 85 * u, 63 * u], fill=screen)
    d.rounded_rectangle([53 * u, 66 * u, 93 * u, 71 * u], radius=2 * u, fill=white)
    # cursor crossing the gap
    x, y, k = 50 * u, 33 * u, 1.05 * u
    arrow = [(0, 0), (0, 22), (5.5, 17), (9, 25), (12.5, 23.5), (9, 15.5), (16, 15.5)]
    pts = [(x + px * k, y + py * k) for px, py in arrow]
    outline = [(x + (px - 0.0) * k, y + py * k) for px, py in arrow]
    d.polygon(outline, fill=(15, 23, 42, 255))
    inner = [(x + 1.6 * k + px * k * 0.82, y + 3.2 * k + py * k * 0.82) for px, py in arrow]
    d.polygon(inner, fill=white)
    return img


def simple(size):
    """16-24 px: details would turn to mush, so just two screens."""
    img = background(S)
    d = ImageDraw.Draw(img)
    u = S / 100
    white = (255, 255, 255, 255)
    d.rounded_rectangle([12 * u, 24 * u, 58 * u, 62 * u], radius=6 * u, fill=white)
    d.rounded_rectangle([20 * u, 64 * u, 50 * u, 74 * u], radius=3 * u, fill=white)
    d.rounded_rectangle([60 * u, 44 * u, 90 * u, 70 * u], radius=5 * u, fill=white)
    return img.resize((size, size), Image.LANCZOS)


big = detailed()
sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
frames = [simple(s) if s <= 24 else big.resize((s, s), Image.LANCZOS) for s in sizes]
frames[-1].save(OUT, format="ICO", sizes=[(s, s) for s in sizes], append_images=frames[:-1])
big.resize((256, 256), Image.LANCZOS).save(OUT.with_name("icon-256.png"))
print("wrote", OUT)
