"""Regenerate the committed desktop icons (requires Pillow).

Run from the repository root: python scripts/generate-app-icons.py
Use --source PATH to import a new square PNG. Packaging uses the generated
files directly and does not require Pillow.
"""
import argparse
import io
import struct
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
MASTER = ROOT / "assets/icons/app-icon.png"


def generate(source):
    with Image.open(source) as image:
        if image.width != image.height or image.width < 1024:
            raise ValueError("Use a square source image of at least 1024 pixels.")
        master = image.convert("RGBA").resize((1024, 1024), Image.Resampling.LANCZOS)
    MASTER.parent.mkdir(parents=True, exist_ok=True)
    master.save(MASTER, optimize=True)
    master.save(
        ROOT / "app-icon.ico",
        format="ICO",
        sizes=[(n, n) for n in (16, 20, 24, 32, 40, 48, 64, 128, 256)],
    )

    # Match the visual size of macOS rounded-square icons in the Dock.
    canvas = Image.new("RGBA", (1024, 1024))
    artwork = master.resize((824, 824), Image.Resampling.LANCZOS)
    canvas.alpha_composite(artwork, (100, 100))
    chunks = []
    for tag, size in [
        ("icp4", 16), ("ic11", 32), ("icp5", 32), ("ic12", 64),
        ("ic07", 128), ("ic13", 256), ("ic08", 256), ("ic14", 512),
        ("ic09", 512), ("ic10", 1024),
    ]:
        data = io.BytesIO()
        canvas.resize((size, size), Image.Resampling.LANCZOS).save(data, format="PNG")
        png = data.getvalue()
        chunks.append(tag.encode("ascii") + struct.pack(">I", len(png) + 8) + png)
    body = b"".join(chunks)
    (ROOT / "app-icon.icns").write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)
    print("Generated assets/icons/app-icon.png, app-icon.ico and app-icon.icns")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=MASTER)
    generate(parser.parse_args().source)
