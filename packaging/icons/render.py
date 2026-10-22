#!/usr/bin/env python3
"""Renders omp-deck-<size>.png from the same geometry as omp-deck.svg (needs Pillow)."""
import os
from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
SIZES = (16, 24, 32, 48, 64, 128, 256, 512)
S = 2048  # supersampled canvas; geometry below is in the SVG's 512 px space
K = S / 512


def master():
    # Diagonal violet (#8b5cf6) -> cyan (#22d3ee) gradient.
    a, b = (139, 92, 246), (34, 211, 238)
    grad = Image.new("RGB", (S, S))
    px = grad.load()
    for y in range(S):
        for x in range(S):
            t = (x + y) / (2 * (S - 1))
            px[x, y] = tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [32 * K, 32 * K, 480 * K - 1, 480 * K - 1], radius=112 * K, fill=255
    )
    img = grad.convert("RGBA")
    img.putalpha(mask)
    d = ImageDraw.Draw(img)
    w = round(40 * K)

    def stroke(points):
        pts = [(x * K, y * K) for x, y in points]
        d.line(pts, fill="white", width=w, joint="curve")
        r = w / 2
        for x, y in pts:
            d.ellipse([x - r, y - r, x + r, y + r], fill="white")

    stroke([(148, 176), (268, 256), (148, 336)])
    stroke([(300, 342), (368, 342)])
    return img


m = master()
for s in SIZES:
    m.resize((s, s), Image.LANCZOS).save(os.path.join(HERE, f"omp-deck-{s}.png"))
