#!/usr/bin/env python3
"""Regenerate every derived icon in this directory from ``sekio_logo.png``.

Run from the repository root::

    python3 assets/generate.py

The outputs are committed on purpose: the Rust build embeds one of them with
``include_bytes!`` and the .deb/.rpm ship the rest, so nothing in CI may depend
on Pillow (or any other image tool) being installed on the runner.

Two things about the pipeline are deliberate, and both are there for the 16 px
output, which is the only size where this logo is genuinely at risk:

* **One LANCZOS step, always from the 1254x1254 source.** Resampling once from
  the largest available pixels keeps more of the eye and the document edges
  than chaining 1254 -> 256 -> 16 would.
* **The source is square-cropped to its own content first.** ``sekio_logo.png``
  carries roughly 6% transparent margin on each side, which costs about two
  pixels of the eye at 16x16 -- and at 16x16 two pixels is the difference
  between an eye and a smudge (checked by eye, magnified; see assets/README.md).
  Cropping to the alpha bounding box, squared and re-centred, hands every size
  the full tile. It is applied at every size rather than only the small ones so
  the whole set stays one consistent piece of framing.

The alpha channel is preserved throughout: these icons sit on a desktop
background, a title bar and a Start Menu tile, none of which are white.
"""

from __future__ import annotations

import pathlib

from PIL import Image

# Sizes the freedesktop hicolor theme is indexed by, plus the two the Windows
# .ico and the embedded window icon are taken from. 16/24 are menus and tabs,
# 32/48 are file managers, 64 is the embedded window icon, 128/256 are HiDPI
# and the Windows Start Menu / Add-Remove Programs entry.
SIZES = [16, 24, 32, 48, 64, 128, 256]

HERE = pathlib.Path(__file__).resolve().parent
SOURCE = HERE / "sekio_logo.png"
ICON_DIR = HERE / "icons"


def squared_to_content(image: Image.Image) -> Image.Image:
    """Crop `image` to a square centred on its non-transparent content.

    The side is the *longer* of the content's two dimensions, so nothing is
    ever cut off: the narrower axis simply keeps some of the original margin.
    """
    box = image.getchannel("A").getbbox()
    if box is None:  # fully transparent: nothing to centre on
        return image
    left, top, right, bottom = box
    side = max(right - left, bottom - top)
    cx, cy = (left + right) // 2, (top + bottom) // 2
    half = side // 2
    return image.crop((cx - half, cy - half, cx - half + side, cy - half + side))


def main() -> None:
    source = squared_to_content(Image.open(SOURCE).convert("RGBA"))
    ICON_DIR.mkdir(exist_ok=True)
    print(f"source cropped to {source.width}x{source.height}")

    for size in SIZES:
        # Always from `source`, never from the previous (smaller) output.
        out = source.resize((size, size), Image.LANCZOS)
        path = ICON_DIR / f"sekio-{size}.png"
        out.save(path, format="PNG", optimize=True)
        print(f"{path.relative_to(HERE.parent)}  {size}x{size}  {path.stat().st_size} B")

    # One multi-resolution .ico for the Windows executable resource and the
    # installer. Pillow resizes each frame from `source` itself, so the frames
    # match the standalone PNGs above rather than being re-derived from one of
    # them.
    ico = HERE / "sekio.ico"
    source.save(ico, format="ICO", sizes=[(s, s) for s in SIZES])
    print(f"{ico.relative_to(HERE.parent)}  {'/'.join(str(s) for s in SIZES)}  {ico.stat().st_size} B")


if __name__ == "__main__":
    main()
