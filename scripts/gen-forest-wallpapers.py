#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Draws the artwork of the four Forest themes (Clearing, Canopy, Wafer, Die Shot).

Original to this project: no photograph, no third-party image, no font. Everything is computed
(numpy, Pillow, pycairo), so the artwork is reproducible from source. The random numbers are
seeded: the same program gives the same picture.

Run from the repository root:

    scripts/gen-forest-wallpapers.py [--width 1920] [--out DIR] [--png] [name ...]

names: clearing canopy wafer die-shot (default: all). The pictures are composed on a 2560x1600
canvas and resampled, still in floating point, to `--width` (16:10), then dithered with
triangular noise of one 8-bit step and quantised, so the long gradients do not band. The files
embedded in the shell binary are written with the defaults:

    crates/chonk-shell/assets/wallpapers/forest-<name>.jpg   (1920x1200, JPEG quality 92, 4:4:4)

The rule all four follow: the busy part of the picture is at the rim or the bottom; the middle
of the screen, where windows sit, is quiet.
"""
import argparse
import math
import os

import cairo
import numpy as np
from PIL import Image

BW, BH = 2560, 1600


def hexrgb(h):
    h = h.lstrip('#')
    return np.array([int(h[i:i + 2], 16) / 255.0 for i in (0, 2, 4)], dtype=np.float32)


def lerp(a, b, t):
    return a + (b - a) * t


def smooth(t):
    t = np.clip(t, 0.0, 1.0)
    return t * t * (3.0 - 2.0 * t)


class Canvas:
    def __init__(self, scale, seed):
        self.s = scale
        self.w, self.h = BW * scale, BH * scale
        self.rng = np.random.default_rng(seed)
        y, x = np.mgrid[0:self.h, 0:self.w].astype(np.float32)
        # base-pixel coordinates (0..2560, 0..1600) of every pixel's centre
        self.x = (x + 0.5) / scale
        self.y = (y + 0.5) / scale
        self.img = np.zeros((self.h, self.w, 3), dtype=np.float32)

    def noise(self, cells_x, octaves=4, gain=0.5, seed=None):
        """Fractal value noise in 0..1, `cells_x` cells across at the first octave."""
        rng = self.rng if seed is None else np.random.default_rng(seed)
        total = np.zeros((self.h, self.w), dtype=np.float32)
        amp, norm = 1.0, 0.0
        for o in range(octaves):
            gx = max(2, int(round(cells_x * (2 ** o))))
            gy = max(2, int(round(gx * self.h / self.w)))
            grid = rng.random((gy + 3, gx + 3), dtype=np.float32)
            im = Image.fromarray(grid, mode='F').resize((self.w + 3 * (self.w // gx + 1), self.h + 3 * (self.h // gy + 1)), Image.BICUBIC)
            layer = np.asarray(im, dtype=np.float32)[self.h // gy + 1:self.h // gy + 1 + self.h, self.w // gx + 1:self.w // gx + 1 + self.w]
            total += layer * amp
            norm += amp
            amp *= gain
        return np.clip(total / norm, 0.0, 1.0)

    def mask(self, draw):
        """An antialiased coverage mask (0..1): `draw(ctx)` paints in base-pixel coordinates."""
        surface = cairo.ImageSurface(cairo.FORMAT_A8, self.w, self.h)
        ctx = cairo.Context(surface)
        ctx.scale(self.s, self.s)
        ctx.set_source_rgba(0, 0, 0, 1)
        draw(ctx)
        surface.flush()
        data = np.frombuffer(surface.get_data(), dtype=np.uint8).reshape(self.h, surface.get_stride())[:, :self.w]
        return data.astype(np.float32) / 255.0

    def over(self, color, alpha):
        """Source-over of a flat colour through a coverage map."""
        a = alpha[..., None]
        self.img = self.img * (1.0 - a) + np.asarray(color, dtype=np.float32) * a

    def add(self, color, amount):
        self.img += np.asarray(color, dtype=np.float32) * amount[..., None]

    def blur(self, m, radius):
        """A box blur run three times (close to a Gaussian) of a single-channel map; radius in base pixels."""
        r = max(1, int(round(radius * self.s)))
        out = m
        for _ in range(3):
            for axis in (0, 1):
                c = np.cumsum(np.pad(out, [(r + 1, r) if a == axis else (0, 0) for a in (0, 1)], mode='edge'), axis=axis, dtype=np.float64)
                n = out.shape[axis]
                hi = np.take(c, np.arange(2 * r + 1, 2 * r + 1 + n), axis=axis)
                lo = np.take(c, np.arange(0, n), axis=axis)
                out = ((hi - lo) / (2 * r + 1)).astype(np.float32)
        return out

    def save(self, path, png=False, width=None):
        if width and width != self.w:
            height = round(self.h * width / self.w)
            planes = [np.asarray(Image.fromarray(self.img[..., i], 'F').resize((width, height), Image.LANCZOS)) for i in range(3)]
            self.img = np.stack(planes, axis=-1).astype(np.float32)
        rng = np.random.default_rng(12345)
        # triangular dither of one step, the same for the three channels' sum but independent each
        d = (rng.random(self.img.shape, dtype=np.float32) - rng.random(self.img.shape, dtype=np.float32)) / 255.0
        out = np.clip(self.img + d, 0.0, 1.0)
        im = Image.fromarray((out * 255.0 + 0.5).astype(np.uint8), 'RGB')
        if png:
            im.save(path, optimize=True)
        else:
            im.save(path, quality=92, subsampling=0, optimize=True)


# --- the mark -------------------------------------------------------------------------------------
# A conifer of traces, vias and pads, in a 100-unit box.
MARK_TIERS = [((33, 36), (50, 17), (67, 36)), ((25, 55), (50, 33), (75, 55)), ((17, 74), (50, 50), (83, 74))]
MARK_VIAS = [(33, 36), (67, 36), (25, 55), (75, 55), (17, 74), (83, 74)]
MARK_PADS = [31, 47, 63]


def mark_traces(ctx, ox, oy, size, width):
    """The mark's strokes (trunk, tiers, chip edge) as one stroked path."""
    k = size / 100.0
    ctx.set_line_width(width)
    ctx.set_line_cap(cairo.LINE_CAP_ROUND)
    ctx.set_line_join(cairo.LINE_JOIN_ROUND)
    ctx.move_to(ox + 50 * k, oy + 17 * k)
    ctx.line_to(ox + 50 * k, oy + 80 * k)
    for a, b, c in MARK_TIERS:
        ctx.move_to(ox + a[0] * k, oy + a[1] * k)
        ctx.line_to(ox + b[0] * k, oy + b[1] * k)
        ctx.line_to(ox + c[0] * k, oy + c[1] * k)
    ctx.move_to(ox + 30 * k, oy + 84 * k)
    ctx.line_to(ox + 70 * k, oy + 84 * k)
    ctx.stroke()


def mark_vias(ctx, ox, oy, size, radius):
    k = size / 100.0
    for x, y in MARK_VIAS:
        ctx.arc(ox + x * k, oy + y * k, radius, 0, 2 * math.pi)
        ctx.fill()


def mark_pads(ctx, ox, oy, size):
    k = size / 100.0
    for x in MARK_PADS:
        ctx.rectangle(ox + x * k, oy + 86.5 * k, 6 * k, 5 * k)
    ctx.fill()


def conifer(ctx, x, base, height, rng, lean=0.0, tiers=None, spread=1.0):
    """A conifer silhouette: a stack of drooping tiers on a thin trunk, tip at (x + lean, base - height)."""
    tiers = tiers or max(4, int(height / 26) + 3)
    half = height * (0.17 + 0.05 * rng.random()) * spread
    tip_x, tip_y = x + lean, base - height
    ctx.move_to(tip_x, tip_y)
    left, right = [], []
    for i in range(1, tiers + 1):
        t = i / tiers
        y = tip_y + height * 0.94 * t
        cx = x + lean * (1 - t)
        w = half * (t ** 0.85) * (0.86 + 0.28 * rng.random())
        droop = height * 0.035 * (0.6 + 0.8 * rng.random()) * min(1.0, 8.0 / tiers) * 1.4
        left.append((cx - w, y + droop, cx - w * 0.48, y - droop * 0.6))
        right.append((cx + w, y + droop, cx + w * 0.48, y - droop * 0.6))
    for (ox, oy, ix, iy) in right:
        ctx.line_to(ox, oy)
        ctx.line_to(ix, iy)
    ctx.line_to(x + height * 0.012, base)
    ctx.line_to(x - height * 0.012, base)
    for (ox, oy, ix, iy) in reversed(left):
        ctx.line_to(ix, iy)
        ctx.line_to(ox, oy)
    ctx.close_path()
    ctx.fill()


# --- Clearing -------------------------------------------------------------------------------------
def clearing(c):
    X, Y = c.x, c.y
    top, mid, low = hexrgb('#dfe8e4'), hexrgb('#f2efe4'), hexrgb('#f7ecd9')
    t = Y / BH
    sky = np.where((t < 0.62)[..., None], lerp(top, mid, smooth(t / 0.62)[..., None]), lerp(mid, low, smooth((t - 0.62) / 0.38)[..., None]))
    c.img = sky.astype(np.float32)
    # high thin cloud: two soft bands, barely there
    cloud = c.noise(3, 4, 0.55, seed=11)
    band = np.exp(-((t - 0.30) / 0.16) ** 2)
    c.add(np.array([1.0, 1.0, 1.0]), (smooth((cloud - 0.45) / 0.4) * band * 0.035).astype(np.float32))
    # the sun: a low, soft disc on the right, behind the far ridges
    sx, sy = BW * 0.735, BH * 0.612
    d = np.sqrt((X - sx) ** 2 + (Y - sy) ** 2)
    c.add(hexrgb('#fff3d6'), (np.exp(-(d / 560.0) ** 2) * 0.085).astype(np.float32))
    c.add(hexrgb('#fff8e8'), (np.exp(-(d / 130.0) ** 2) * 0.10).astype(np.float32))
    c.over(hexrgb('#fffdf6'), smooth((40.0 - d) / 2.5) * 0.92)

    rng = np.random.default_rng(7)
    # ridges, far to near: (base height of the ridge line, relief, tree height, colour, mist under it)
    ridges = [
        (0.660, 34, 13, '#c9d6cf', 0.60),
        (0.705, 46, 22, '#b3c5bb', 0.55),
        (0.752, 60, 36, '#93ad9f', 0.50),
        (0.806, 72, 60, '#6b8c7b', 0.42),
        (0.868, 90, 104, '#44695a', 0.30),
        (0.945, 110, 190, '#274536', 0.0),
    ]
    for index, (level, relief, tree, colour, mist) in enumerate(ridges):
        n = 2600
        xs = np.linspace(-40, BW + 40, n)
        phase = rng.random(4) * 6.283
        ridge = (np.sin(xs / 610.0 + phase[0]) * 0.50 + np.sin(xs / 263.0 + phase[1]) * 0.27 + np.sin(xs / 117.0 + phase[2]) * 0.13 + np.sin(xs / 51.0 + phase[3]) * 0.05)
        # the valley of the picture is under the sun; ridges rise to the left
        ridge_y = BH * level - relief * ridge - relief * 0.9 * smooth((BW * 0.55 - xs) / (BW * 0.7))

        def draw(ctx, xs=xs, ridge_y=ridge_y, tree=tree, index=index):
            ctx.move_to(xs[0], BH + 10)
            for x, y in zip(xs, ridge_y):
                ctx.line_to(x, y)
            ctx.line_to(xs[-1], BH + 10)
            ctx.close_path()
            ctx.fill()
            step = tree * (0.36 if index < 5 else 0.42)
            x = -30.0
            r = np.random.default_rng(100 + index)
            while x < BW + 30:
                base = float(np.interp(x, xs, ridge_y)) + tree * 0.10
                h = tree * (0.62 + 0.75 * r.random() ** 1.6)
                conifer(ctx, x, base, h, r, lean=h * 0.03 * (r.random() - 0.5))
                x += step * (0.55 + 0.9 * r.random())

        m = c.mask(draw)
        c.over(hexrgb(colour), m)
        if mist > 0:
            # mist pooled at the foot of this ridge, in front of it and behind the next
            foot = BH * level + relief * 0.9
            band = np.exp(-((Y - foot) / (48.0 + index * 12.0)) ** 2)
            drift = 0.72 + 0.28 * c.noise(5, 3, 0.5, seed=40 + index)
            c.over(hexrgb('#f3f1e6'), (band * mist * drift).astype(np.float32))
    c.img *= (1.0 - 0.22 * smooth((Y - BH * 0.90) / (BH * 0.10)))[..., None]
    # the mark as traces in the nearest ridge, lower left: two shades lighter than the trees, no more
    def trace(ctx):
        mark_traces(ctx, 196, BH - 250, 190, 5.0)
        mark_pads(ctx, 196, BH - 250, 190)
    c.over(hexrgb('#3d6350'), c.mask(trace) * 0.75)
    def vias(ctx):
        mark_vias(ctx, 196, BH - 250, 190, 7.5)
    c.over(hexrgb('#4f7a64'), c.mask(vias) * 0.85)
    def holes(ctx):
        mark_vias(ctx, 196, BH - 250, 190, 3.2)
    c.over(hexrgb('#274536'), c.mask(holes))
    # the whole picture a touch darker at the corners
    v = ((X - BW / 2) / (BW / 2)) ** 2 + ((Y - BH / 2) / (BH / 2)) ** 2
    c.img *= (1.0 - 0.045 * smooth((v - 0.7) / 1.3))[..., None]


# --- Canopy ---------------------------------------------------------------------------------------
def canopy(c):
    X, Y = c.x, c.y
    t = Y / BH
    # the night sky between the trees: blue-green, deepest overhead, lighter toward the horizon
    top, mid, low = hexrgb('#06141a'), hexrgb('#0e3038'), hexrgb('#1c4a4a')
    sky = np.where((t < 0.6)[..., None], lerp(top, mid, smooth(t / 0.6)[..., None]), lerp(mid, low, smooth((t - 0.6) / 0.3)[..., None]))
    c.img = sky.astype(np.float32)
    cloud = c.noise(2.5, 5, 0.52, seed=3)
    c.add(hexrgb('#2a6a66'), (smooth((cloud - 0.42) / 0.5) * np.exp(-((t - 0.45) / 0.35) ** 2) * 0.07).astype(np.float32))
    c.add(hexrgb('#1c3f5a'), (smooth((c.noise(4, 4, 0.5, seed=5) - 0.5) / 0.4) * np.exp(-((t - 0.3) / 0.3) ** 2) * 0.06).astype(np.float32))
    # copper light at the horizon: the day that is coming, or the town behind the hill
    hx = BW * 0.5
    c.over(hexrgb('#b5633a'), (np.exp(-((Y - BH * 0.87) / (BH * 0.12)) ** 2 - ((X - hx) / (BW * 0.42)) ** 2) * 0.62).astype(np.float32))
    c.over(hexrgb('#f0a868'), (np.exp(-((Y - BH * 0.878) / (BH * 0.04)) ** 2 - ((X - hx) / (BW * 0.28)) ** 2) * 0.55).astype(np.float32))
    # stars, fewer toward the glow
    rng = np.random.default_rng(21)
    stars = [(rng.random() * BW, rng.random() ** 1.5 * BH * 0.78, 0.45 + 1.15 * rng.random() ** 3) for _ in range(620)]

    def small(ctx):
        for x, y, rad in stars:
            ctx.arc(x, y, rad, 0, 2 * math.pi)
            ctx.fill()
    c.add(hexrgb('#cfe8dc'), c.mask(small) * (0.6 * (1.0 - smooth((t - 0.5) / 0.35))).astype(np.float32))
    # the constellation: the mark, in the gap, its vias the bright stars, its traces barely lit
    size, ox, oy = 400.0, BW * 0.5 - 200.0, BH * 0.30 - 200.0
    c.add(hexrgb('#7fd6b4'), c.mask(lambda ctx: mark_traces(ctx, ox, oy, size, 1.6)) * 0.22)
    c.add(hexrgb('#8fe8c4'), c.blur(c.mask(lambda ctx: mark_vias(ctx, ox, oy, size, 9.0)), 7.0) * 0.55)

    def via(ctx):
        mark_vias(ctx, ox, oy, size, 3.4)
        k = size / 100.0
        for px in MARK_PADS:
            ctx.arc(ox + (px + 3) * k, oy + 89 * k, 2.2, 0, 2 * math.pi)
            ctx.fill()
    c.add(hexrgb('#e9fff4'), c.mask(via) * 0.9)

    # ridges against the glow, far to near
    rng = np.random.default_rng(9)
    for index, (level, relief, tree, colour) in enumerate([(0.865, 30, 20, '#12303a'), (0.900, 44, 40, '#0c2229'), (0.945, 60, 80, '#07151a')]):
        xs = np.linspace(-40, BW + 40, 2600)
        phase = rng.random(4) * 6.283
        ridge = np.sin(xs / 610.0 + phase[0]) * 0.5 + np.sin(xs / 263.0 + phase[1]) * 0.27 + np.sin(xs / 117.0 + phase[2]) * 0.13 + np.sin(xs / 51.0 + phase[3]) * 0.05
        # the ground falls to the middle of the picture
        ridge_y = BH * level - relief * ridge - relief * 1.4 * smooth(np.abs(xs - BW * 0.5) / (BW * 0.5))

        def draw(ctx, xs=xs, ridge_y=ridge_y, tree=tree, index=index):
            ctx.move_to(xs[0], BH + 10)
            for x, y in zip(xs, ridge_y):
                ctx.line_to(x, y)
            ctx.line_to(xs[-1], BH + 10)
            ctx.close_path()
            ctx.fill()
            r = np.random.default_rng(300 + index)
            x = -30.0
            while x < BW + 30:
                base = float(np.interp(x, xs, ridge_y)) + tree * 0.10
                h = tree * (0.62 + 0.75 * r.random() ** 1.6)
                conifer(ctx, x, base, h, r, lean=h * 0.03 * (r.random() - 0.5))
                x += tree * 0.38 * (0.55 + 0.9 * r.random())
        c.over(hexrgb(colour), c.mask(draw))
    # the canopy: tall trees at both sides, nearest and tallest at the edges, opening to the sky
    def stand(ctx, seed, count, lo, hi, reach):
        r = np.random.default_rng(seed)
        for side in (-1, 1):
            for i in range(count):
                u = (i + r.random() * 0.8) / count          # 0 at the edge, 1 at the inner end of the stand
                x = (BW * 0.5) + side * (BW * 0.5 + 60 - u * reach)
                h = lerp(hi, lo, u ** 0.8) * (0.86 + 0.28 * r.random())
                conifer(ctx, x, BH + 30, h, r, lean=side * -h * 0.02 * r.random(), tiers=int(h / 34) + 6, spread=0.80)

    back = c.mask(lambda ctx: stand(ctx, 31, 9, 560, 1180, 760))
    c.over(hexrgb('#0a1f24'), back)
    mid = c.mask(lambda ctx: stand(ctx, 32, 7, 700, 1420, 560))
    # the copper light catches the inner edges of the middle trees, low down
    rim = np.clip(c.blur(mid, 2.5) - mid, 0, 1) * smooth((t - 0.55) / 0.3)
    c.add(hexrgb('#c97a45'), rim * 0.45)
    c.over(hexrgb('#061318'), mid)
    front = c.mask(lambda ctx: stand(ctx, 33, 4, 1100, 1750, 330))
    c.over(hexrgb('#03090b'), front)
    # the ground in front
    c.img *= (1.0 - 0.5 * smooth((t - 0.93) / 0.07))[..., None]
    v = ((X - BW / 2) / (BW / 2)) ** 2 + ((Y - BH / 2) / (BH / 2)) ** 2
    c.img *= (1.0 - 0.18 * smooth((v - 0.8) / 1.2))[..., None]


# --- Wafer ----------------------------------------------------------------------------------------
def wafer(c):
    X, Y = c.x, c.y
    # the bench: near white, light from the upper left
    d = np.sqrt(((X - BW * 0.16) / BW) ** 2 + ((Y - BH * 0.10) / BH) ** 2)
    c.img = lerp(hexrgb('#f7f8fb'), hexrgb('#e2e6ee'), smooth(d / 1.1)[..., None]).astype(np.float32)
    # the wafer: a disc far larger than the screen, entering from the lower right. Its edge crosses the top
    # edge right of the middle and the bottom edge near the left: the right-hand column, the bottom strip and
    # the gap beside the windows are wafer; the launchers' corner is bench.
    wx, wy, wr = BW * 1.05, BH * 1.25, BH * 1.45
    rr = np.sqrt((X - wx) ** 2 + (Y - wy) ** 2)
    ang = np.arctan2(Y - wy, X - wx)
    inside = smooth((wr - rr) / 1.2 + 0.5)
    # its shadow on the bench
    sh = np.exp(-np.clip(rr - wr + 4, 0, None) / 30.0) * (1 - inside)
    c.img *= (1.0 - 0.13 * sh)[..., None]
    # polished silicon: a cool grey with one broad reflection lying across it
    streak = np.exp(-(((X + Y * 0.9) - BW * 1.02) / 620.0) ** 2)
    base = lerp(hexrgb('#c6cdd8'), hexrgb('#eceff5'), (0.18 + 0.82 * streak)[..., None])
    grain = (c.noise(260, 2, 0.5, seed=9) - 0.5) * 0.010
    surface = base + grain[..., None]
    # the die grid: only whole dies, as on a wafer map
    pitch_x, pitch_y, street = 92.0, 78.0, 3.0
    gx0, gy0 = wx % pitch_x, wy % pitch_y
    ix = np.floor((X - gx0) / pitch_x)
    iy = np.floor((Y - gy0) / pitch_y)
    edge = wr - 52.0
    far = np.zeros_like(X)
    for a in (gx0 + ix * pitch_x, gx0 + (ix + 1) * pitch_x):
        for b in (gy0 + iy * pitch_y, gy0 + (iy + 1) * pitch_y):
            far = np.maximum(far, np.sqrt((a - wx) ** 2 + (b - wy) ** 2))
    whole = (far < edge).astype(np.float32)
    # a drop-in site of two by two dies carries the mark instead of circuits: beside the instrument column
    six, siy = math.floor((BW * 0.862 - gx0) / pitch_x), math.floor((BH * 0.345 - gy0) / pitch_y)
    site = ((ix >= six) & (ix <= six + 1) & (iy >= siy) & (iy <= siy + 1))
    whole = whole * (1.0 - site.astype(np.float32))
    fx = (X - gx0) - ix * pitch_x
    fy = (Y - gy0) - iy * pitch_y
    aa = 1.0 / c.s
    in_die = smooth((np.minimum(fx, pitch_x - fx) - street) / aa + 0.5) * smooth((np.minimum(fy, pitch_y - fy) - street) / aa + 0.5)
    die = whole * in_die
    # every die the same layout, as tone: a core, a memory, a row of pads, a seal line
    core = ((fx > 14) & (fx < 52) & (fy > 12) & (fy < 46)).astype(np.float32)
    mem = ((fx > 58) & (fx < 80) & (fy > 12) & (fy < 66)).astype(np.float32) * (0.7 + 0.3 * (np.mod(fy, 6.0) < 3.0))
    pads = ((fy > 55) & (fy < 63) & (fx > 14) & (fx < 52) & (np.mod(fx - 14, 6) < 3.4)).astype(np.float32)
    seal = die * (1.0 - smooth((np.minimum(np.minimum(fx, pitch_x - fx), np.minimum(fy, pitch_y - fy)) - street - 1.5) / aa))
    tone = 1.0 - die * (0.070 + 0.040 * core + 0.030 * mem + 0.065 * pads) - 0.05 * seal
    # thin-film colour: the hue runs with the angle and the radius; strongest in a band inside the edge, and
    # every die takes a little of it (the film is on the dies, the streets are bare)
    film_band = np.exp(-((rr - (wr - 170.0)) / 230.0) ** 2) * 0.80 + 0.20
    ph = (ang * 3.1 + rr / 260.0)
    film = np.stack([0.5 + 0.5 * np.cos(ph + 2.2), 0.5 + 0.5 * np.cos(ph + 0.35), 0.5 + 0.5 * np.cos(ph - 1.5)], axis=-1)
    film = (film - film.mean(axis=-1, keepdims=True)) * 0.16
    surface = surface * tone[..., None] + film * (film_band * (0.22 + 0.78 * die))[..., None]
    c.img = c.img * (1 - inside[..., None]) + surface.astype(np.float32) * inside[..., None]
    # the edge: a bevel that catches the light, the dark rim, and one fine line of the film's colour inside it
    c.add(np.array([1.0, 1.0, 1.0]), (np.exp(-((rr - (wr - 6.0)) / 2.6) ** 2) * 0.50).astype(np.float32))
    c.img *= (1.0 - 0.26 * np.exp(-((rr - (wr - 0.5)) / 1.4) ** 2))[..., None]
    rim = np.exp(-((rr - (wr - 20.0)) / 3.0) ** 2) * inside
    c.img += (film * 2.2) * rim[..., None]
    # the mark, etched on its site like a wafer's identity
    ms = 128.0
    mx, my = gx0 + (six + 1) * pitch_x - ms / 2, gy0 + (siy + 1) * pitch_y - ms / 2 - 2

    def etched(ctx):
        mark_traces(ctx, mx, my, ms, 3.4)
        mark_vias(ctx, mx, my, ms, 5.0)
        mark_pads(ctx, mx, my, ms)
    c.over(hexrgb('#7d8899'), c.mask(etched) * 0.70)


# --- Die Shot -------------------------------------------------------------------------------------
def dieshot(c):
    X, Y = c.x, c.y
    rng = np.random.default_rng(1977)
    c.img[:] = hexrgb('#07090d')
    aa = 1.0 / c.s
    m = 46.0            # the pad ring's distance from the screen's edge
    core = m + 62.0     # where the blocks begin
    fine = c.noise(900, 2, 0.5, seed=4)

    def box(x0, y0, x1, y1, g):
        """Coverage of a rectangle inset by g, and the distance from its inset edge inward."""
        inb = smooth((X - x0 - g) / aa + 0.5) * smooth((x1 - g - X) / aa + 0.5) * smooth((Y - y0 - g) / aa + 0.5) * smooth((y1 - g - Y) / aa + 0.5)
        edge = np.minimum(np.minimum(X - x0, x1 - X), np.minimum(Y - y0, y1 - Y)) - g
        return inb, edge

    def cut(x0, y0, x1, y1, depth, small, out):
        w, h = x1 - x0, y1 - y0
        if depth == 0 or (w < small[0] and h < small[1]) or (depth < 3 and rng.random() < 0.12):
            out.append((x0, y0, x1, y1))
            return
        if w / h > 1.25 or (w / h > 0.8 and rng.random() < 0.5):
            s = round((x0 + w * (0.34 + 0.32 * rng.random())) / 8) * 8
            cut(x0, y0, s, y1, depth - 1, small, out)
            cut(s, y0, x1, y1, depth - 1, small, out)
        else:
            s = round((y0 + h * (0.34 + 0.32 * rng.random())) / 8) * 8
            cut(x0, y0, x1, s, depth - 1, small, out)
            cut(x0, s, x1, y1, depth - 1, small, out)

    blocks = []
    cut(core, core, BW - core, BH - core, 5, (330, 250), blocks)
    size = 500.0
    ox, oy = (BW - size) / 2, (BH - size) / 2 - 8
    sig_block = (ox - 60, oy - 44, ox + size + 60, oy + size + 52)
    # the metals' colours under a thin film: blue-black, teal, violet, bronze
    tints = [hexrgb('#0f2030'), hexrgb('#141c36'), hexrgb('#0e2428'), hexrgb('#1c1a30'), hexrgb('#231f14'), hexrgb('#121a24')]

    def texture(kind, x0, y0):
        if kind == 0:      # memory: a fine regular array with word-line bands and a sense-amplifier strip
            px, py = 6.0 + 2 * rng.integers(0, 3), 4.0 + 2 * rng.integers(0, 2)
            cell = (np.mod(X - x0, px) < px * 0.5).astype(np.float32) * (np.mod(Y - y0, py) < py * 0.6).astype(np.float32)
            band = (np.mod(Y - y0, py * 16) < py * 1.2).astype(np.float32)
            return 0.50 + 0.26 * cell - 0.34 * band
        if kind == 1:      # standard cells: rows of random run lengths between power rails
            row = 9.0
            ry = np.floor((Y - y0) / row)
            run = np.floor((X - x0) / (5.0 + 3.0 * np.mod(ry * 7.0, 3.0)))
            h = np.mod(np.sin(ry * 127.1 + run * 311.7) * 43758.5453, 1.0)
            rail = (np.mod(Y - y0, row) < 1.6).astype(np.float32)
            return 0.36 + 0.40 * h - 0.24 * rail
        if kind == 2:      # data path: bit slices
            h = np.mod(np.sin(np.floor((X - x0) / 12.0) * 91.7) * 15731.7, 1.0)
            return 0.42 + 0.24 * h + 0.14 * (np.mod(Y - y0, 30.0) < 2.0)
        gx, gy = np.floor((X - x0) / 74.0), np.floor((Y - y0) / 58.0)   # analog: plates
        h = np.mod(np.sin(gx * 12.9898 + gy * 78.233) * 43758.5453, 1.0)
        plate = smooth((np.minimum(np.mod(X - x0, 74.0), 74.0 - np.mod(X - x0, 74.0)) - 5.0) / aa) * smooth((np.minimum(np.mod(Y - y0, 58.0), 58.0 - np.mod(Y - y0, 58.0)) - 5.0) / aa)
        return 0.38 + 0.36 * h * plate

    for (x0, y0, x1, y1) in blocks:
        inb, edge = box(x0, y0, x1, y1, 7.0)
        if inb.max() <= 0:
            continue
        tint = tints[rng.integers(0, len(tints))]
        # a block is a few macros of its own: each a texture and a shade of the block's metal
        subs = []
        cut(x0 + 10, y0 + 10, x1 - 10, y1 - 10, 2, (200, 150), subs)
        colour = np.zeros_like(c.img) + (tint * 0.55)[None, None, :]
        for (a0, b0, a1, b1) in subs:
            sub, sedge = box(a0, b0, a1, b1, 3.0)
            tex = texture(rng.integers(0, 4), a0, b0) + (fine - 0.5) * 0.10
            shade = 0.86 + 0.34 * rng.random()
            # the detail is at a macro's rim; its middle is a quiet tone
            quiet = 1.0 - 0.45 * smooth((sedge - 10.0) / 46.0)
            body = tint[None, None, :] * shade * (0.90 + 0.95 * (tex[..., None] - 0.45) * quiet[..., None] + 0.45)
            # lit from the upper left: a light line on a macro's top and left, a dark one below and right
            lit = np.exp(-np.clip(np.minimum(X - a0, Y - b0) - 3.0, 0, None) / 1.2) * 0.22 - np.exp(-np.clip(np.minimum(a1 - X, b1 - Y) - 3.0, 0, None) / 1.2) * 0.20
            body = body * (1.0 + lit[..., None])
            colour = colour * (1 - sub[..., None]) + body * sub[..., None]
        ring = np.exp(-np.clip(edge, 0, None) / 10.0)
        colour = colour * (1.0 + 0.50 * ring[..., None])
        c.img = c.img * (1 - inb[..., None]) + colour.astype(np.float32) * inb[..., None]

    # routing between the blocks: thin gold
    def routes(ctx):
        ctx.set_line_width(1.2)
        for (x0, y0, x1, y1) in blocks + [sig_block]:
            ctx.rectangle(x0 + 3.5, y0 + 3.5, x1 - x0 - 7, y1 - y0 - 7)
        ctx.stroke()
    c.add(hexrgb('#d2a94e'), c.mask(routes) * 0.22)
    # the signature's own block: bare field with a ring like the others
    (x0, y0, x1, y1) = sig_block
    outb, _ = box(x0, y0, x1, y1, -7.0)
    c.over(hexrgb('#07090d'), outb)
    inb, edge = box(x0, y0, x1, y1, 7.0)
    ring = np.exp(-np.clip(edge, 0, None) / 12.0)
    field = hexrgb('#0d131b')[None, None, :] * (1.0 + 0.55 * ring[..., None] + (fine[..., None] - 0.5) * 0.12)
    c.img = c.img * (1 - inb[..., None]) + field.astype(np.float32) * inb[..., None]
    blocks.append(sig_block)

    # the top metal: power straps over everything inside the seal ring, two layers a little apart
    inside_ring, _ = box(core - 8, core - 8, BW - core + 8, BH - core + 8, 0.0)
    keep = inside_ring * (1.0 - inb)
    strap_v = (np.mod(X - core, 64.0) < 2.0).astype(np.float32) * keep
    strap_h = (np.mod(Y - core, 96.0) < 2.0).astype(np.float32) * keep
    c.img *= (1.0 - 0.30 * np.clip(np.roll(strap_v, int(2 * c.s), axis=1) + np.roll(strap_h, int(2 * c.s), axis=0), 0, 1))[..., None]
    c.add(hexrgb('#7f95b0'), strap_v * 0.085)
    c.add(hexrgb('#b89a52'), strap_h * 0.075)

    # the thin film: one reflection lying across the die, its colour turning along it
    along = (X * 0.8 + Y * 0.6) / (BW * 0.8 + BH * 0.6)
    across = (Y * 0.8 - X * 0.6) / BH
    sheen = np.exp(-((across + 0.18) / 0.34) ** 2)
    film = np.stack([0.5 + 0.5 * np.cos(along * 7.0 + 2.4), 0.5 + 0.5 * np.cos(along * 7.0 + 0.4), 0.5 + 0.5 * np.cos(along * 7.0 - 1.6)], axis=-1)
    c.img += (film - 0.35) * (sheen * 0.050)[..., None] * inside_ring[..., None]
    c.img *= (1.0 + 0.16 * sheen * inside_ring)[..., None]

    # the seal ring and the bond pads round it: fine, exact, not loud
    def seal(ctx):
        ctx.set_line_width(1.5)
        ctx.rectangle(core - 14, core - 14, BW - 2 * core + 28, BH - 2 * core + 28)
        ctx.stroke()
        ctx.set_line_width(1.0)
        ctx.rectangle(core - 9, core - 9, BW - 2 * core + 18, BH - 2 * core + 18)
        ctx.stroke()
    c.add(hexrgb('#d8b45a'), c.mask(seal) * 0.34)
    pad, pitch = 18.0, 40.0

    def pads(ctx, inset, stroke):
        def one(x, y):
            if stroke:
                ctx.rectangle(x + 0.5, y + 0.5, pad - 1, pad - 1)
            else:
                ctx.rectangle(x + inset, y + inset, pad - 2 * inset, pad - 2 * inset)
        n = int((BW - 2 * m - 80) // pitch)
        x0 = (BW - (n - 1) * pitch) / 2
        for i in range(n):
            for y in (m, BH - m - pad):
                one(x0 + i * pitch - pad / 2, y)
        n = int((BH - 2 * m - 150) // pitch)
        y0 = (BH - (n - 1) * pitch) / 2
        for i in range(n):
            for x in (m, BW - m - pad):
                one(x, y0 + i * pitch - pad / 2)
        if stroke:
            ctx.set_line_width(1.0)
            ctx.stroke()
        else:
            ctx.fill()
    c.over(hexrgb('#2c2718'), c.mask(lambda ctx: pads(ctx, 0.0, False)))
    c.over(hexrgb('#8d7a41'), c.mask(lambda ctx: pads(ctx, 5.0, False)) * 0.75)
    c.add(hexrgb('#d8b45a'), c.mask(lambda ctx: pads(ctx, 0.0, True)) * 0.30)

    # the signature in the top metal: the mark, a dark body with a bright core line
    c.over(hexrgb('#3a3626'), c.mask(lambda ctx: (mark_traces(ctx, ox, oy, size, 10.0), mark_pads(ctx, ox, oy, size))))
    c.add(hexrgb('#f0cd6a'), c.mask(lambda ctx: mark_traces(ctx, ox, oy, size, 1.6)) * 0.55)
    c.over(hexrgb('#a48a42'), c.mask(lambda ctx: mark_vias(ctx, ox, oy, size, 13.0)))
    c.over(hexrgb('#f0cd6a'), c.mask(lambda ctx: mark_vias(ctx, ox, oy, size, 13.0)) * np.clip(1.0 - c.mask(lambda ctx: mark_vias(ctx, ox, oy, size, 11.4)), 0, 1) * 0.6)
    c.over(hexrgb('#0a0e13'), c.mask(lambda ctx: mark_vias(ctx, ox, oy, size, 5.5)))
    # the microscope's field: darker toward the corners
    v = ((X - BW / 2) / (BW / 2)) ** 2 + ((Y - BH / 2) / (BH / 2)) ** 2
    c.img *= (1.0 - 0.26 * smooth((v - 0.6) / 1.3))[..., None]
    # a wallpaper, not a photograph: a little darker and less coloured than the die would be
    grey = c.img.mean(axis=-1, keepdims=True)
    c.img = np.clip((grey + (c.img - grey) * 0.86) * 0.88, 0.0, 1.0)


WALLPAPERS = {'clearing': (clearing, 101), 'canopy': (canopy, 202), 'wafer': (wafer, 303), 'die-shot': (dieshot, 404)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--width', type=int, default=1920)
    ap.add_argument('--out', default='crates/chonk-shell/assets/wallpapers')
    ap.add_argument('--png', action='store_true')
    ap.add_argument('names', nargs='*')
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    for name in a.names or WALLPAPERS:
        fn, seed = WALLPAPERS[name]
        canvas = Canvas(1, seed)
        fn(canvas)
        path = os.path.join(a.out, f"forest-{name}.{'png' if a.png else 'jpg'}")
        canvas.save(path, a.png, a.width)
        print(path, os.path.getsize(path))


if __name__ == '__main__':
    main()
