#!/usr/bin/env python3
"""Draws the app icon, a cat's head, into ui/app.ico. Needs Pillow."""
from PIL import Image, ImageDraw

SIZES = [16, 24, 32, 48, 64, 128, 256]
BIG = 1024
GROUND = (232, 163, 61, 255)
INK = (24, 20, 18, 255)


def draw():
    img = Image.new("RGBA", (BIG, BIG), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((0, 0, BIG - 1, BIG - 1), radius=210, fill=GROUND)
    # Ears, then the head over their base.
    d.polygon([(190, 520), (215, 140), (480, 330)], fill=INK)
    d.polygon([(834, 520), (809, 140), (544, 330)], fill=INK)
    d.ellipse((170, 280, 854, 880), fill=INK)
    # Eyes and nose, big enough to survive at small sizes.
    d.ellipse((310, 480, 450, 640), fill=GROUND)
    d.ellipse((574, 480, 714, 640), fill=GROUND)
    d.polygon([(470, 690), (554, 690), (512, 745)], fill=GROUND)
    return img


if __name__ == "__main__":
    draw().save("ui/app.ico", format="ICO", sizes=[(n, n) for n in SIZES])
