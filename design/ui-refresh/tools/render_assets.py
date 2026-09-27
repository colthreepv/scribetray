# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow>=10"]
# ///
"""Render Scribetray UI refresh assets: tray-state icons, app icon, overlay previews.

Run: uv run design/ui-refresh/tools/render_assets.py
Outputs land in design/ui-refresh/out/.
"""
from __future__ import annotations

import math
import random
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "out"
SS = 16  # supersampling factor

TRAY_SIZES = [16, 20, 24, 32, 40, 48]
APP_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]

GLYPH = {"dark": (255, 255, 255), "light": (28, 28, 30)}  # dark = dark taskbar
REC = (240, 68, 56)
WORK = (245, 165, 36)
ERR = (240, 68, 56)
OK = (34, 197, 94)


def mask(size: int) -> tuple[Image.Image, ImageDraw.ImageDraw, float]:
    m = Image.new("L", (size * SS, size * SS), 0)
    return m, ImageDraw.Draw(m), size * SS / 16.0


def down(m: Image.Image, size: int) -> Image.Image:
    return m.resize((size, size), Image.Resampling.BOX)


def mic_mask(size: int, slash: bool = False) -> Image.Image:
    """Solid microphone glyph on a 16-unit grid (reads well at 16 px)."""
    m, d, k = mask(size)
    s = lambda *v: [round(x * k) for x in v]
    d.rounded_rectangle(s(5, 1, 11, 10), radius=3 * k, fill=255)  # capsule
    w = round(1.5 * k)
    d.arc(s(2.75, 2.5, 13.25, 12.5), start=0, end=180, fill=255, width=w)  # cradle
    d.rectangle(s(2.75, 6.6, 2.75 + 1.5, 7.6), fill=255)
    d.rectangle(s(13.25 - 1.5, 6.6, 13.25, 7.6), fill=255)
    d.rectangle(s(7.25, 12, 8.75, 14.25), fill=255)  # stem
    d.rounded_rectangle(s(4.75, 13.75, 11.25, 15.25), radius=0.75 * k, fill=255)  # base
    if slash:
        gap = Image.new("L", m.size, 0)
        g = ImageDraw.Draw(gap)
        g.line(s(1.5, 0.5, 15.5, 14.5), fill=255, width=round(3.5 * k))
        m = ImageChops.subtract(m, gap)
        d = ImageDraw.Draw(m)
        d.line(s(1.5, 0.5, 15.5, 14.5), fill=255, width=round(1.5 * k))
    return m


def badge(size: int, kind: str) -> tuple[Image.Image, Image.Image, Image.Image | None]:
    """Returns (cutout mask, badge mask, inner-mark mask)."""
    cx, cy, r, cut = 12.0, 12.0, 3.75, 5.0
    cm, cd, k = mask(size)
    cd.ellipse([round((cx - cut) * k), round((cy - cut) * k), round((cx + cut) * k), round((cy + cut) * k)], fill=255)
    bm, bd, _ = mask(size)
    bd.ellipse([round((cx - r) * k), round((cy - r) * k), round((cx + r) * k), round((cy + r) * k)], fill=255)
    mark = None
    if kind == "error":
        mark, md, _ = mask(size)
        a = 1.6
        md.line([round((cx - a) * k), round((cy - a) * k), round((cx + a) * k), round((cy + a) * k)], fill=255, width=round(1.1 * k))
        md.line([round((cx - a) * k), round((cy + a) * k), round((cx + a) * k), round((cy - a) * k)], fill=255, width=round(1.1 * k))
    return cm, bm, mark


def compose(layers: list[tuple[Image.Image, tuple[int, int, int]]], size: int) -> Image.Image:
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    for m, color in layers:
        solid = Image.new("RGBA", (size, size), color + (255,))
        solid.putalpha(down(m, size))
        img = Image.alpha_composite(img, solid)
    return img


def tray_icon(size: int, state: str, theme: str) -> Image.Image:
    glyph = mic_mask(size, slash=(state == "off"))
    fg = GLYPH[theme]
    if state in ("idle", "off"):
        return compose([(glyph, fg)], size)
    cut, bm, mark = badge(size, state)
    glyph = ImageChops.subtract(glyph, cut)
    color = {"recording": REC, "working": WORK, "error": ERR}[state]
    layers = [(glyph, fg), (bm, color)]
    if mark is not None:
        layers.append((mark, (255, 255, 255)))
    return compose(layers, size)


def app_icon(size: int) -> Image.Image:
    """Colored brand tile: warm gradient, white mic, waveform bars at >= 48 px."""
    big = 256 * 4
    k = big / 256
    grad = Image.new("RGBA", (big, big))
    top, bot = (255, 112, 94), (214, 45, 78)
    gd = ImageDraw.Draw(grad)
    for y in range(big):
        t = y / (big - 1)
        gd.line([(0, y), (big, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bot)) + (255,))
    tile = Image.new("L", (big, big), 0)
    ImageDraw.Draw(tile).rounded_rectangle([round(12 * k), round(12 * k), round(244 * k), round(244 * k)], radius=round(56 * k), fill=255)
    base = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    base.paste(grad, (0, 0), tile)
    fg = Image.new("L", (big, big), 0)
    d = ImageDraw.Draw(fg)
    s = lambda *v: [round(x * k) for x in v]
    d.rounded_rectangle(s(102, 46, 154, 146), radius=26 * k, fill=255)
    d.arc(s(74, 64, 182, 176), start=0, end=180, fill=255, width=round(13 * k))
    d.rectangle(s(74, 112, 87, 122), fill=255)
    d.rectangle(s(169, 112, 182, 122), fill=255)
    d.rectangle(s(121.5, 174, 134.5, 200), fill=255)
    d.rounded_rectangle(s(98, 196, 158, 209), radius=6.5 * k, fill=255)
    if size >= 48:
        for x, h in ((34, 34), (52, 64), (204 - 10 + 2, 64), (222 - 10 + 2, 34)):
            d.rounded_rectangle(s(x, 110 - h / 2, x + 10, 110 + h / 2), radius=5 * k, fill=255)
    white = Image.new("RGBA", (big, big), (255, 255, 255, 255))
    white.putalpha(fg)
    img = Image.alpha_composite(base, white)
    return img.resize((size, size), Image.Resampling.LANCZOS)


# ---------------------------------------------------------------- overlay previews
PILL_BG = (22, 24, 29)
BAR_COUNT, BAR_W, BAR_PITCH = 7, 2.0, 4.0


def rr(d, box, r, fill=None, outline=None, width=1):
    d.rounded_rectangle([round(v) for v in box], radius=r, fill=fill, outline=outline, width=width)


def draw_overlay(scale: int, state: str, t: float, bars: list[float], bg=(38, 38, 40)) -> Image.Image:
    """Mirror of the spec in PLAN.md; units are DIPs times 'scale'."""
    W, H = 110, 40
    backdrop = Image.new("RGBA", (W * scale * 4, H * scale * 4), bg + (255,))
    img = Image.new("RGBA", backdrop.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    q = scale * 4
    ox, oy = 26, 9
    if state in ("done", "error"):
        pw = 22
    else:
        pw = 58
    rr(d, (ox * q, oy * q, (ox + pw) * q, (oy + 22) * q), 11 * q, fill=PILL_BG + (240,), outline=(255, 255, 255, 34), width=q)
    cy = oy + 11
    if state == "recording":
        a = 0.55 + 0.45 * (0.5 + 0.5 * math.cos(2 * math.pi * t / 1.6))
        col = tuple(round(c * a + p * (1 - a)) for c, p in zip(REC, PILL_BG))
        d.ellipse([(ox + 8) * q, (cy - 3) * q, (ox + 14) * q, (cy + 3) * q], fill=col)
        for i, lv in enumerate(bars):
            h = 2 + lv * 12
            x = ox + 21 + i * BAR_PITCH
            rr(d, (x * q, (cy - h / 2) * q, (x + BAR_W) * q, (cy + h / 2) * q), BAR_W / 2 * q, fill=(240, 242, 245))
    elif state == "working":
        d.ellipse([(ox + 8) * q, (cy - 3) * q, (ox + 14) * q, (cy + 3) * q], fill=WORK)
        for i in range(3):
            y = cy - 3 * max(0.0, math.sin(2 * math.pi * (t / 0.9 - i / 6)))
            x = ox + 27 + i * 6
            d.ellipse([(x - 1.75) * q, (y - 1.75) * q, (x + 1.75) * q, (y + 1.75) * q], fill=(240, 242, 245))
    elif state == "done":
        c = ox + 11
        d.line([((c - 4.5) * q, cy * q), ((c - 1.5) * q, (cy + 3) * q), ((c + 4.5) * q, (cy - 3.5) * q)], fill=OK, width=round(2 * q), joint="curve")
    elif state == "error":
        c = ox + 11
        for a, b in (((-3.5, -3.5), (3.5, 3.5)), ((-3.5, 3.5), (3.5, -3.5))):
            d.line([((c + a[0]) * q, (cy + a[1]) * q), ((c + b[0]) * q, (cy + b[1]) * q)], fill=ERR, width=round(2 * q))
    return Image.alpha_composite(backdrop, img).resize((W * scale, H * scale), Image.Resampling.LANCZOS)


def speech_level(t: float, rng: random.Random) -> float:
    syll = max(0.0, math.sin(2 * math.pi * t * 3.1)) ** 0.6
    phrase = 1.0 if (t % 3.2) < 2.4 else 0.05
    return min(1.0, phrase * (0.25 + 0.75 * syll) * (0.75 + 0.5 * rng.random()))


def overlay_gifs(scale: int = 3) -> None:
    rng = random.Random(7)
    frames, hist, shown = [], [0.0] * BAR_COUNT, [0.0] * BAR_COUNT
    fps, sample_every = 30, 0.05
    next_sample = 0.0
    for n in range(int(6.4 * fps)):
        t = n / fps
        if t >= next_sample:
            hist = hist[1:] + [speech_level(t, rng)]
            next_sample += sample_every
        shown = [s + (h - s) * 0.45 for s, h in zip(shown, hist)]
        frames.append(draw_overlay(scale, "recording", t, shown))
    save_gif(frames, OUT / "overlay-recording.gif", fps)
    frames = [draw_overlay(scale, "working", n / fps, []) for n in range(int(1.8 * fps))]
    save_gif(frames, OUT / "overlay-working.gif", fps)
    strip = Image.new("RGBA", (110 * scale, 40 * scale * 4), (38, 38, 40, 255))
    for i, st in enumerate(["recording", "working", "done", "error"]):
        strip.paste(draw_overlay(scale, st, 0.0, [0.2, 0.5, 0.9, 0.6, 1.0, 0.4, 0.15]), (0, i * 40 * scale))
    strip.save(OUT / "overlay-states.png")


def save_gif(frames: list[Image.Image], path: Path, fps: int) -> None:
    pal = [f.convert("RGB").quantize(colors=128, method=Image.Quantize.MEDIANCUT) for f in frames]
    pal[0].save(path, save_all=True, append_images=pal[1:], duration=round(1000 / fps), loop=0, optimize=False, disposal=2)


def preview_sheet(tray: dict) -> None:
    states = ["idle", "recording", "working", "error", "off"]
    cell = 64
    sheet = Image.new("RGBA", (cell * len(states) * 2 + 40, cell * 2 + 20), (0, 0, 0, 0))
    for row, (theme, bg) in enumerate((("dark", (32, 32, 32)), ("light", (238, 238, 238)))):
        for col, st in enumerate(states):
            x = col * cell * 2 + 20
            y = row * cell + 10
            ImageDraw.Draw(sheet).rectangle([x - 20 if col == 0 else x - 20, y, x + cell * 2 - 20, y + cell], fill=bg + (255,))
            ic32 = tray[(theme, st)][32].resize((32, 32))
            ic16 = tray[(theme, st)][16].resize((32, 32), Image.Resampling.NEAREST)
            sheet.alpha_composite(ic16, (x, y + 16))
            sheet.alpha_composite(ic32, (x + 48, y + 16))
    sheet.save(OUT / "tray-states-preview.png")


def main() -> None:
    (OUT / "tray").mkdir(parents=True, exist_ok=True)
    tray: dict = {}
    for theme in ("dark", "light"):
        for state in ("idle", "recording", "working", "error", "off"):
            imgs = {s: tray_icon(s, state, theme) for s in TRAY_SIZES}
            tray[(theme, state)] = imgs
            name = f"tray-{state}-{theme}"
            imgs[48].save(OUT / "tray" / f"{name}.ico", sizes=[(s, s) for s in TRAY_SIZES], append_images=[imgs[s] for s in TRAY_SIZES if s != 48])
            imgs[32].save(OUT / "tray" / f"{name}-32.png")
    apps = {s: app_icon(s) for s in APP_SIZES}
    apps[256].save(OUT / "scribetray.ico", sizes=[(s, s) for s in APP_SIZES], append_images=[apps[s] for s in APP_SIZES if s != 256])
    apps[256].save(OUT / "app-icon-256.png")
    preview_sheet(tray)
    overlay_gifs()
    print("assets written to", OUT)


if __name__ == "__main__":
    main()
