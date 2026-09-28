# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow>=10"]
# ///
"""README hero banner and GitHub social preview (1280x640).

Run: uv run design/public-release/tools/render_banner.py
"""
import math
import random
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

HERE = Path(__file__).resolve().parents[1]
REPO = HERE.parents[1]
OUT = HERE / "media"
ICON = REPO / "design/ui-refresh/out/app-icon-256.png"
F = "C:/Windows/Fonts/"
S = 2  # supersample

BG_TOP, BG_BOT = (24, 26, 32), (15, 16, 20)
CORAL = (240, 86, 74)
TEXT = (240, 242, 245)
MUTED = (150, 156, 168)


def font(name, px):
    return ImageFont.truetype(F + name, px * S)


def rr(d, box, r, **kw):
    d.rounded_rectangle([round(v * S) for v in box], radius=r * S, **kw)


def main():
    W, H = 1280, 640
    img = Image.new("RGB", (W * S, H * S))
    d = ImageDraw.Draw(img)
    for y in range(H * S):
        t = y / (H * S - 1)
        d.line([(0, y), (W * S, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOT)))
    # soft coral glow behind the field
    glow = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse([760 * S, 150 * S, 1240 * S, 520 * S], fill=CORAL + (60,))
    glow = glow.filter(ImageFilter.GaussianBlur(90 * S))
    img = Image.alpha_composite(img.convert("RGBA"), glow)
    d = ImageDraw.Draw(img)

    icon = Image.open(ICON).convert("RGBA").resize((132 * S, 132 * S), Image.Resampling.LANCZOS)
    img.alpha_composite(icon, (84 * S, 150 * S))
    d.text((84 * S, 310 * S), "Scribetray", font=font("segoeuib.ttf", 76), fill=TEXT)
    d.text((88 * S, 412 * S), "Speak into any text field on Windows.", font=font("segoeui.ttf", 30), fill=TEXT)
    d.text((88 * S, 458 * S), "Press Win+Alt+V, talk, and the words land at your cursor.", font=font("segoeui.ttf", 22), fill=MUTED)
    d.text((88 * S, 492 * S), "Powered by ElevenLabs Scribe.", font=font("segoeui.ttf", 22), fill=MUTED)

    # mock composer
    fx, fy, fw, fh = 690, 250, 500, 130
    rr(d, (fx, fy, fx + fw, fy + fh), 22, fill=(43, 43, 46), outline=(62, 62, 66), width=S)
    line = "Can you review the overlay before we ship"
    d.text(((fx + 28) * S, (fy + 52) * S), line, font=font("segoeui.ttf", 22), fill=TEXT)
    cx = fx + 28 + d.textlength(line, font=font("segoeui.ttf", 22)) / S + 3
    d.rectangle([round(cx * S), (fy + 56) * S, round((cx + 2) * S), (fy + 82) * S], fill=TEXT)

    # waveform pill above the caret (2x the in-app size)
    pw, ph = 116, 44
    px, py = cx - 8, fy - ph / 2 - 4
    shadow = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle([round(px * S), round((py + 4) * S), round((px + pw) * S), round((py + ph + 4) * S)], radius=22 * S, fill=(0, 0, 0, 140))
    img = Image.alpha_composite(img, shadow.filter(ImageFilter.GaussianBlur(8 * S)))
    d = ImageDraw.Draw(img)
    rr(d, (px, py, px + pw, py + ph), 22, fill=(22, 24, 29), outline=(70, 72, 80), width=S)
    cy = py + ph / 2
    d.ellipse([round((px + 16) * S), round((cy - 6) * S), round((px + 28) * S), round((cy + 6) * S)], fill=(240, 68, 56))
    for i, lv in enumerate([0.25, 0.55, 0.95, 0.7, 1.0, 0.5, 0.2]):
        h = 4 + lv * 24
        x = px + 42 + i * 8
        rr(d, (x, cy - h / 2, x + 4, cy + h / 2), 2, fill=TEXT)

    # tiny taskbar strip with the tray glyph
    tray = REPO / "design/ui-refresh/out/tray/tray-recording-dark-32.png"
    if tray.exists():
        rr(d, (1060, 560, 1216, 604), 10, fill=(32, 32, 34))
        t = Image.open(tray).convert("RGBA").resize((28 * S, 28 * S), Image.Resampling.LANCZOS)
        img.alpha_composite(t, (1076 * S, 568 * S))
        d.text((1114 * S, 568 * S), "14:32", font=font("segoeui.ttf", 18), fill=TEXT)

    OUT.mkdir(parents=True, exist_ok=True)
    img = img.convert("RGB").resize((W, H), Image.Resampling.LANCZOS)
    img.save(OUT / "banner.png", optimize=True)
    img.resize((W // 2, H // 2), Image.Resampling.LANCZOS)
    print("written", OUT / "banner.png")


if __name__ == "__main__":
    main()
