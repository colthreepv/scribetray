# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow>=10"]
# ///
"""Square cover (1200x1200) for LinkedIn and other social posts.

Run: uv run tools/design/render_square.py
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "docs/media"
ICON = REPO / "assets/scribetray.ico"
F = "C:/Windows/Fonts/"
S = 2
BG_TOP, BG_BOT = (24, 26, 32), (13, 14, 18)
CORAL = (240, 86, 74)
TEXT = (240, 242, 245)
MUTED = (150, 156, 168)


def font(name, px):
    return ImageFont.truetype(F + name, px * S)


def rr(d, box, r, **kw):
    d.rounded_rectangle([round(v * S) for v in box], radius=round(r * S), **kw)


def centered(d, y, text, f, fill, W):
    w = d.textlength(text, font=f) / S
    d.text((round((W - w) / 2 * S), y * S), text, font=f, fill=fill)


def main():
    W = H = 1200
    img = Image.new("RGB", (W * S, H * S))
    d = ImageDraw.Draw(img)
    for y in range(H * S):
        t = y / (H * S - 1)
        d.line([(0, y), (W * S, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOT)))
    glow = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse([250 * S, 520 * S, 950 * S, 900 * S], fill=CORAL + (70,))
    img = Image.alpha_composite(img.convert("RGBA"), glow.filter(ImageFilter.GaussianBlur(110 * S)))
    d = ImageDraw.Draw(img)

    ico = Image.open(ICON)
    ico.size = max(ico.info.get("sizes", {ico.size}))
    icon = ico.convert("RGBA").resize((150 * S, 150 * S), Image.Resampling.LANCZOS)
    img.alpha_composite(icon, (round((W - 150) / 2 * S), 120 * S))
    centered(d, 290, "Scribetray", font("segoeuib.ttf", 96), TEXT, W)
    centered(d, 418, "Speak into any text field on Windows.", font("segoeui.ttf", 40), TEXT, W)

    # composer field with the waveform pill above the caret
    fx, fy, fw, fh = 150, 640, 900, 170
    rr(d, (fx, fy, fx + fw, fy + fh), 30, fill=(43, 43, 46), outline=(66, 66, 70), width=S)
    line = "Can you review the overlay before we ship"
    tf = font("segoeui.ttf", 36)
    d.text(((fx + 44) * S, (fy + 62) * S), line, font=tf, fill=TEXT)
    cx = fx + 44 + d.textlength(line, font=tf) / S + 5
    d.rectangle([round(cx * S), (fy + 66) * S, round((cx + 3) * S), (fy + 110) * S], fill=TEXT)

    pw, ph = 190, 66
    px, py = cx - 150, fy - ph / 2 - 6
    shadow = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle([round(px * S), round((py + 6) * S), round((px + pw) * S), round((py + ph + 6) * S)], radius=33 * S, fill=(0, 0, 0, 150))
    img = Image.alpha_composite(img, shadow.filter(ImageFilter.GaussianBlur(10 * S)))
    d = ImageDraw.Draw(img)
    rr(d, (px, py, px + pw, py + ph), 33, fill=(22, 24, 29), outline=(72, 74, 82), width=S)
    cy = py + ph / 2
    d.ellipse([round((px + 24) * S), round((cy - 9) * S), round((px + 42) * S), round((cy + 9) * S)], fill=(240, 68, 56))
    for i, lv in enumerate([0.25, 0.55, 0.95, 0.7, 1.0, 0.5, 0.2, 0.4]):
        h = 6 + lv * 36
        x = px + 64 + i * 14
        rr(d, (x, cy - h / 2, x + 6, cy + h / 2), 3, fill=TEXT)

    # keys row
    kf = font("segoeuisb.ttf" if (Path(F) / "segoeuisb.ttf").exists() else "seguisb.ttf", 30)
    items = [("Win+Alt+V", "talk"), ("Enter", "send"), ("Esc", "cancel")]
    lf = font("segoeui.ttf", 30)
    widths = [d.textlength(k, font=kf) / S + 36 + 14 + d.textlength(l, font=lf) / S for k, l in items]
    gap = 56
    x = (W - sum(widths) - gap * (len(items) - 1)) / 2
    y = 900
    for (k, l), w in zip(items, widths):
        kw = d.textlength(k, font=kf) / S + 36
        rr(d, (x, y, x + kw, y + 56), 12, fill=(36, 38, 44), outline=(80, 82, 90), width=S)
        d.text(((x + 18) * S, (y + 9) * S), k, font=kf, fill=TEXT)
        d.text(((x + kw + 14) * S, (y + 9) * S), l, font=lf, fill=MUTED)
        x += w + gap

    centered(d, 1060, "Open source · Rust · powered by ElevenLabs Scribe", font("segoeui.ttf", 28), MUTED, W)

    OUT.mkdir(parents=True, exist_ok=True)
    img.convert("RGB").resize((W, H), Image.Resampling.LANCZOS).save(OUT / "cover-square.png", optimize=True)
    print("written", OUT / "cover-square.png")


if __name__ == "__main__":
    main()
