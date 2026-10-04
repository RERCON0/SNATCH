"""Export the approved SNATCH marks as Windows icons (requires Pillow).

Run from any directory: python icons/export_icons.py
The source marks keep their original shape; this only fits them onto the
shared white app tile and exports PNG/ICO sizes for Windows.
"""

from pathlib import Path

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parent
MASTER_SIZE = 1024
ICON_SIZES = (16, 20, 24, 32, 40, 48, 64, 96, 128, 256)


def export_icon(edition: str) -> Image.Image:
    with Image.open(ROOT / "source" / f"mark-{edition}.png") as source:
        mark = source.convert("RGBA")
    bounds = mark.getchannel("A").getbbox()
    if bounds is None:
        raise ValueError(f"The {edition} mark is empty")
    mark = mark.crop(bounds)
    # Scale both editions by their longest side, preserving the selected shapes.
    mark.thumbnail((720, 720), Image.Resampling.LANCZOS)

    tile = Image.new("RGBA", (MASTER_SIZE, MASTER_SIZE))
    ImageDraw.Draw(tile).rounded_rectangle(
        (24, 24, 999, 999), radius=166, fill=(255, 255, 255, 255)
    )
    tile.alpha_composite(
        mark, ((MASTER_SIZE - mark.width) // 2, (MASTER_SIZE - mark.height) // 2)
    )
    tile.save(ROOT / f"icon-{edition}.png", optimize=True)
    tile.save(
        ROOT / f"icon-{edition}.ico",
        sizes=[(size, size) for size in ICON_SIZES],
    )
    return tile


def main() -> None:
    app = export_icon("app")
    export_icon("cli")
    app.resize((256, 256), Image.Resampling.LANCZOS).save(
        ROOT / "icon-256.png", optimize=True
    )
    for edition in ("app", "cli"):
        with Image.open(ROOT / f"icon-{edition}.ico") as icon:
            expected = {(size, size) for size in ICON_SIZES}
            if icon.ico.sizes() != expected:
                raise ValueError(f"Missing ICO sizes in {edition}")
            for size in expected:
                frame = icon.ico.getimage(size).convert("RGBA")
                # Lanczos can leave alpha 1-2 at small outer corners.
                if frame.getpixel((0, 0))[3] > 3:
                    raise ValueError(f"Opaque outer corner in {edition} at {size}")
    print("Exported GUI / 01 and CLI / 02: white tiles, PNG + 10 ICO sizes")


if __name__ == "__main__":
    main()
