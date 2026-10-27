"""Regenerate the committed desktop icons from assets/icons/app-icon.svg.

Run from the repository root: python scripts/generate-app-icons.py
Requires rsvg-convert (librsvg). Packaging uses the generated files
directly and does not need this script.

Outputs:
  assets/icons/app-icon.png                       1024 px master
  lan-mouse-gtk/resources/de.feschber.LanMouse.svg window icon (copy of the SVG)
  app-icon.ico                                    Windows, PNG entries 16-256 px
  app-icon.icns                                   macOS, artwork inset to 824/1024
"""
import argparse
import struct
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "assets/icons/app-icon.svg"


def render(svg, size, workdir):
    out = Path(workdir) / f"{svg.stem}-{size}.png"
    subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size), str(svg), "-o", str(out)],
        check=True,
    )
    return out.read_bytes()


def generate(source):
    with tempfile.TemporaryDirectory() as workdir:
        (ROOT / "assets/icons/app-icon.png").write_bytes(render(source, 1024, workdir))
        (ROOT / "lan-mouse-gtk/resources/de.feschber.LanMouse.svg").write_text(
            source.read_text(encoding="utf-8"), encoding="utf-8"
        )

        sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
        images = [render(source, n, workdir) for n in sizes]
        offset = 6 + 16 * len(sizes)
        entries = b""
        for n, data in zip(sizes, images):
            # A width/height byte of 0 means 256.
            entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(data), offset)
            offset += len(data)
        (ROOT / "app-icon.ico").write_bytes(
            struct.pack("<HHH", 0, 1, len(sizes)) + entries + b"".join(images)
        )

        # Match the visual size of macOS rounded-square icons in the Dock.
        inner = source.read_text(encoding="utf-8").split(">", 1)[1].rsplit("</svg>", 1)[0]
        padded = Path(workdir) / "padded.svg"
        padded.write_text(
            '<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" '
            'viewBox="0 0 1024 1024">'
            f'<g transform="translate(100 100) scale({824 / 1024})">{inner}</g></svg>',
            encoding="utf-8",
        )
        chunks = b""
        for tag, size in [
            ("icp4", 16), ("ic11", 32), ("icp5", 32), ("ic12", 64),
            ("ic07", 128), ("ic13", 256), ("ic08", 256), ("ic14", 512),
            ("ic09", 512), ("ic10", 1024),
        ]:
            png = render(padded, size, workdir)
            chunks += tag.encode("ascii") + struct.pack(">I", len(png) + 8) + png
        (ROOT / "app-icon.icns").write_bytes(
            b"icns" + struct.pack(">I", len(chunks) + 8) + chunks
        )
    print("Generated app-icon.png, de.feschber.LanMouse.svg, app-icon.ico and app-icon.icns")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=SOURCE)
    generate(parser.parse_args().source)
