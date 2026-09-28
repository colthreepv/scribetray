# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow>=10"]
# ///
"""Mockup of the owner-drawn ElevenLabs usage header in the tray menu.

Run: uv run design/ui-refresh/tools/usage_header_mock.py
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

OUT = Path(__file__).resolve().parents[1] / "out"
FONT = "C:/Windows/Fonts/segoeui.ttf"
FONT_B = "C:/Windows/Fonts/seguisb.ttf"
MENU_BG = (249, 249, 249)
TEXT = (26, 26, 26)
GRAY = (109, 109, 109)
TRACK = (224, 224, 224)
FILL = {"ok": (240, 86, 74), "warn": (245, 165, 36), "low": (220, 38, 38)}
SEP = (215, 215, 215)


def header(scale: int, used: int, limit: int, hours: str, reset: str, level: str) -> Image.Image:
    W, H = 300, 64  # DIPs; header item only
    img = Image.new("RGB", (W * scale, (H + 3 * 22) * scale), MENU_BG)
    d = ImageDraw.Draw(img)
    f = lambda px, bold=False: ImageFont.truetype(FONT_B if bold else FONT, px * scale)
    x0, x1 = 12 * scale, (W - 12) * scale
    d.text((x0, 8 * scale), "ElevenLabs", font=f(12, True), fill=TEXT)
    d.text((x1, 8 * scale), hours, font=f(12, True), fill=TEXT, anchor="ra")
    by = 29 * scale
    d.rounded_rectangle([x0, by, x1, by + 4 * scale], radius=2 * scale, fill=TRACK)
    frac = min(1.0, used / limit)
    if frac > 0:
        d.rounded_rectangle([x0, by, max(x0 + 4 * scale, x0 + round((x1 - x0) * frac)), by + 4 * scale], radius=2 * scale, fill=FILL[level])
    d.text((x0, 40 * scale), f"{used:,} / {limit:,} credits · resets {reset}", font=f(11), fill=GRAY)
    d.line([0, H * scale, W * scale, H * scale], fill=SEP, width=scale)
    y = (H + 4) * scale
    for label, key in (("Start recording", "Win+Alt+V"), ("Microphone", "›"), ("Language", "›")):
        d.text((28 * scale, y + 3 * scale), label, font=f(12, label.startswith("Start")), fill=TEXT)
        d.text((x1, y + 3 * scale), key, font=f(12), fill=TEXT, anchor="ra")
        y += 22 * scale
    return img


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    rows = [
        header(2, 1091, 23130, "≈ 37 h left", "Oct 6", "ok"),
        header(2, 19200, 23130, "≈ 6 h 40 m left", "Oct 6", "warn"),
        header(2, 22800, 23130, "≈ 34 m left", "Oct 6", "low"),
    ]
    sheet = Image.new("RGB", (rows[0].width * 3 + 40, rows[0].height + 20), (40, 40, 40))
    for i, r in enumerate(rows):
        sheet.paste(r, (10 + i * (r.width + 10), 10))
    sheet.save(OUT / "usage-header-mock.png")
    print("written", OUT / "usage-header-mock.png")


if __name__ == "__main__":
    main()

