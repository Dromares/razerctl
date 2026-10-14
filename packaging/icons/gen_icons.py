#!/usr/bin/env python3
"""Writes the five symbolic tray icons (razerctl-*-symbolic.svg) next to this script.

Same 22-unit geometry as the DAN-1452 mockup (`tray-icons.png`) and as `pixmap()` in src/bin/razerctl-gui/tray.rs.
Everything is a filled shape (no strokes): GTK recolours symbolic icons by `fill` only, KDE by the
ColorScheme-Text class. The badge's knock-out gap is a clip path, the glyph cut into the badge is a hole.
"""
import math, pathlib

OUT = pathlib.Path(__file__).parent
STYLE = '<style type="text/css" id="current-color-scheme">.ColorScheme-Text{color:#232629}</style>'


def rrect(x0, y0, x1, y1, r):
    w, h = x1 - x0, y1 - y0
    return (f"M{x0 + r:g} {y0:g}h{w - 2 * r:g}a{r:g} {r:g} 0 0 1 {r:g} {r:g}v{h - 2 * r:g}a{r:g} {r:g} 0 0 1 {-r:g} {r:g}"
            f"h{-(w - 2 * r):g}a{r:g} {r:g} 0 0 1 {-r:g} {-r:g}v{-(h - 2 * r):g}a{r:g} {r:g} 0 0 1 {r:g} {-r:g}Z")


def circle(cx, cy, r):
    return f"M{cx - r:g} {cy:g}a{r:g} {r:g} 0 1 0 {2 * r:g} 0a{r:g} {r:g} 0 1 0 {-2 * r:g} 0Z"


RING = rrect(5.2, 1.2, 16.8, 20.8, 5.8) + rrect(6.8, 2.8, 15.2, 19.2, 4.2)  # evenodd: 1.6-unit outline of (6,2)-(16,20) r5
WHEEL = [
    "M6.8 8.7h8.4v1.6H6.8Z",      # chin line
    "M10.2 2.8h1.6v6.9h-1.6Z",    # seam
    "M10 4.2h2v3.2h-2Z",          # wheel
]
BC = (16.2, 16.2)


def dashed_ring(dash=2.6, gap=2.0, half=0.8, n=400):
    """The outline of the body as separate dashes, each a filled polygon (a stroke-dasharray would not recolour)."""
    x0, y0, x1, y1, r = 6.0, 2.0, 16.0, 20.0, 5.0
    pts = []
    def arc(cx, cy, a0):
        for i in range(n // 8 + 1):
            a = math.radians(a0 + 90 * i / (n // 8))
            pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    arc(x1 - r, y0 + r, -90); arc(x1 - r, y1 - r, 0); arc(x0 + r, y1 - r, 90); arc(x0 + r, y0 + r, 180)
    pts.append(pts[0])
    seg = [0.0]
    for a, b in zip(pts, pts[1:]):
        seg.append(seg[-1] + math.dist(a, b))
    total, out, t = seg[-1], [], 0.0
    def at(s):
        i = max(1, next((k for k, v in enumerate(seg) if v >= s), len(seg) - 1))
        f = (s - seg[i - 1]) / ((seg[i] - seg[i - 1]) or 1)
        return (pts[i - 1][0] + f * (pts[i][0] - pts[i - 1][0]), pts[i - 1][1] + f * (pts[i][1] - pts[i - 1][1]), pts[i - 1], pts[i])
    while t < total - 0.01:
        end = min(t + dash, total)
        left, right = [], []
        k = 8
        for j in range(k + 1):
            x, y, a, b = at(t + (end - t) * j / k)
            dx, dy = b[0] - a[0], b[1] - a[1]
            l = math.hypot(dx, dy) or 1
            nx, ny = -dy / l, dx / l
            left.append((x + nx * half, y + ny * half)); right.append((x - nx * half, y - ny * half))
        poly = left + right[::-1]
        out.append("M" + "L".join(f"{x:.2f} {y:.2f}" for x, y in poly) + "Z")
        t = end + gap
    return out


def svg(parts, clip=None):
    clip_def = ""
    body_attr = ""
    if clip:
        clip_def = f'<clipPath id="knock"><path clip-rule="evenodd" d="M0 0H22V22H0Z{clip}"/></clipPath>'
        body_attr = ' clip-path="url(#knock)"'
    body = "".join(f'<path d="{d}"{" fill-rule=\"evenodd\"" if ev else ""}/>' for d, ev in parts["body"])
    badge = "".join(f'<path d="{d}"{" fill-rule=\"evenodd\"" if ev else ""}/>' for d, ev in parts.get("badge", []))
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" width="22" height="22">{STYLE}{clip_def}'
            f'<g class="ColorScheme-Text" fill="currentColor"><g{body_attr}>{body}</g>{badge}</g></svg>\n')


mouse = [(RING, True)] + [(w, False) for w in WHEEL]
icons = {
    "razerctl-symbolic": svg({"body": mouse}),
    "razerctl-nodevice-symbolic": svg({"body": [(d, False) for d in dashed_ring()]}),
    "razerctl-remap-symbolic": svg(
        {"body": mouse, "badge": [(circle(*BC, 2.9), False)]}, clip=circle(*BC, 4.6)),
    "razerctl-low-symbolic": svg(
        {"body": mouse, "badge": [(circle(*BC, 4.4) + "M15.4 13.6h1.6v2.7h-1.6Z" + circle(16.2, 18.4, 0.9), True)]}, clip=circle(*BC, 5.8)),
    "razerctl-charging-symbolic": svg(
        {"body": mouse, "badge": [(circle(*BC, 4.4) + "M17 12.8L14.4 16.6L15.953 16.6L15.4 19.6L18 15.8L16.447 15.8Z", True)]}, clip=circle(*BC, 5.8)),
}
for name, text in icons.items():
    (OUT / f"{name}.svg").write_text(text)
    print(name, len(text))
