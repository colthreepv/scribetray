# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow>=10"]
# ///
"""Tray glyph candidates that avoid the Windows privacy microphone indicator.

Run: uv run design/ui-refresh/tools/icon_candidates.py
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

OUT = Path(__file__).resolve().parents[1] / "out"
SS = 16
CORAL = (240, 86, 74)
CORAL_TOP, CORAL_BOT = (255, 112, 94), (214, 45, 78)
REC = (240, 68, 56)


def canvas(n):
    img = Image.new("RGBA", (n * SS, n * SS), (0, 0, 0, 0))
    return img, ImageDraw.Draw(img), n * SS / 16


def bars(d, k, xs, hs, color, w=1.6, cy=8.0):
    for x, h in zip(xs, hs):
        d.rounded_rectangle([round((x - w / 2) * k), round((cy - h / 2) * k), round((x + w / 2) * k), round((cy + h / 2) * k)], radius=w / 2 * k, fill=color)


def tile(img, d, k):
    g = Image.new("RGBA", img.size)
    gd = ImageDraw.Draw(g)
    for y in range(img.size[1]):
        t = y / (img.size[1] - 1)
        gd.line([(0, y), (img.size[0], y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(CORAL_TOP, CORAL_BOT)) + (255,))
    m = Image.new("L", img.size, 0)
    ImageDraw.Draw(m).rounded_rectangle([round(0.5 * k), round(0.5 * k), round(15.5 * k), round(15.5 * k)], radius=round(3.5 * k), fill=255)
    img.paste(g, (0, 0), m)


def cand_tile(n, fg):
    img, d, k = canvas(n)
    tile(img, d, k)
    bars(d, k, [4, 6.5, 9.5, 12], [4, 8.5, 8.5, 4], (255, 255, 255), w=1.7)
    # small gap between bars 2 and 3 reads as "voice" not "equalizer"
    return img


def cand_bars(n, fg):
    img, d, k = canvas(n)
    bars(d, k, [2.5, 5.5, 8.0, 10.5, 13.5], [4.5, 9.5, 14, 9.5, 4.5], CORAL, w=2.0)
    return img


def cand_caret(n, fg):
    img, d, k = canvas(n)
    # I-beam text caret in the foreground colour + coral sound waves
    s = lambda *v: [round(x * k) for x in v]
    d.rectangle(s(3.25, 2, 4.75, 14), fill=fg)
    d.rectangle(s(1.5, 1.25, 6.5, 2.5), fill=fg)
    d.rectangle(s(1.5, 13.5, 6.5, 14.75), fill=fg)
    bars(d, k, [9.0, 12.0, 15.0], [5, 10, 5], CORAL, w=1.8)
    return img


def cand_bubble(n, fg):
    img, d, k = canvas(n)
    s = lambda *v: [round(x * k) for x in v]
    d.rounded_rectangle(s(0.75, 1.5, 15.25, 12.5), radius=3.5 * k, fill=CORAL)
    d.polygon(s(3.5, 12, 7.5, 12, 3.5, 15.25), fill=CORAL)
    bars(d, k, [5, 8, 11], [3.5, 6.5, 3.5], (255, 255, 255), w=1.7, cy=7.0)
    return img


def with_badge(img, n):
    """Recording badge with a transparent cut ring, same geometry as the mic set."""
    k = n * SS / 16
    cut = Image.new("L", img.size, 0)
    ImageDraw.Draw(cut).ellipse([round(7 * k), round(7 * k), round(17 * k), round(17 * k)], fill=255)
    a = img.getchannel("A")
    a = Image.composite(Image.new("L", img.size, 0), a, cut)
    img = img.copy()
    img.putalpha(a)
    ImageDraw.Draw(img).ellipse([round(8.25 * k), round(8.25 * k), round(15.75 * k), round(15.75 * k)], fill=REC)
    return img


CANDS = [("A  brand tile", cand_tile), ("B  waveform", cand_bars), ("C  caret + voice", cand_caret), ("D  speech bubble", cand_bubble)]


def main():
    sizes = [16, 24, 32]
    col_w, row_h, label_w = 190, 44, 0
    themes = [("dark taskbar", (32, 32, 32), (255, 255, 255)), ("light taskbar", (238, 238, 238), (28, 28, 30))]
    sheet = Image.new("RGBA", (40 + col_w * len(CANDS), 34 + row_h * 4), (250, 250, 250, 255))
    d = ImageDraw.Draw(sheet)
    try:
        font = ImageFont.truetype("C:/Windows/Fonts/segoeui.ttf", 13)
    except OSError:
        font = ImageFont.load_default()
    for ci, (name, fn) in enumerate(CANDS):
        d.text((40 + ci * col_w + 6, 8), name, fill=(40, 40, 40), font=font)
    for ti, (tname, bg, fg) in enumerate(themes):
        for bi, badge in enumerate((False, True)):
            y = 30 + (ti * 2 + bi) * row_h
            d.rectangle([0, y, sheet.width, y + row_h - 4], fill=bg + (255,))
            for ci, (name, fn) in enumerate(CANDS):
                x = 40 + ci * col_w + 6
                for s in sizes:
                    big = fn(s, fg)
                    if badge:
                        big = with_badge(big, s)
                    ic = big.resize((s, s), Image.Resampling.BOX)
                    sheet.alpha_composite(ic, (x, y + (row_h - 4 - s) // 2))
                    x += s + 14
                if s == 32 and ci == 0:
                    pass
            d.text((4, y + 12), "idle" if not badge else "rec", fill=fg, font=font)
    sheet.save(OUT / "tray-candidates.png")
    print("written", OUT / "tray-candidates.png")


if __name__ == "__main__":
    main()

