#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Draws the supaS3 icon and writes the files the packages use.

    python3 packaging/icons/make-icons.py

Needs Pillow. The outputs are committed; rerun only when the drawing changes.
The icon shows the application's two panes with an arrow from one to the other.
"""
from pathlib import Path
from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
SCALE = 4  # draw large, then shrink, for smooth edges
SIZE = 1024

BACKGROUND = (24, 28, 38, 255)
PANE = (236, 240, 247, 255)
ACCENT = (64, 156, 255, 255)


def draw() -> Image.Image:
    side = SIZE * SCALE
    image = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    pen = ImageDraw.Draw(image)

    def box(x0, y0, x1, y1, radius, fill):
        pen.rounded_rectangle(
            [x0 * SCALE, y0 * SCALE, x1 * SCALE, y1 * SCALE], radius * SCALE, fill=fill
        )

    # macOS draws icons inside a margin; the same shape reads well elsewhere.
    box(64, 64, 960, 960, 200, BACKGROUND)
    box(184, 232, 464, 792, 44, PANE)
    box(560, 232, 840, 792, 44, PANE)
    # The arrow crosses from the left pane into the right one.
    box(332, 476, 640, 548, 36, ACCENT)
    pen.polygon(
        [(600 * SCALE, 392 * SCALE), (752 * SCALE, 512 * SCALE), (600 * SCALE, 632 * SCALE)],
        fill=ACCENT,
    )
    return image.resize((SIZE, SIZE), Image.LANCZOS)


def main() -> None:
    icon = draw()
    icon.save(HERE / "supas3-1024.png", optimize=True)
    icon.resize((256, 256), Image.LANCZOS).save(HERE / "supas3-256.png", optimize=True)
    icon.save(
        HERE / "supas3.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )


if __name__ == "__main__":
    main()
