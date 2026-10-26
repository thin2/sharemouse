"""Three production macOS daemons, real TLS and isolated image pasteboards.

Checks representations and original files, not actual application/Finder paste.
Run after cargo build --workspace:
python3 tests/image_clipboard_session_smoke.py --gtk
"""
import ctypes as C
import random
import struct
import sys
import time
import zlib
from pathlib import Path
from urllib.parse import unquote, urlparse

from clipboard_session_smoke import Pasteboards
from sharing_session_smoke import Cluster


def png(width, height):
    rgba = random.Random(12012).randbytes(width * height * 4)

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    scanlines = b"".join(b"\0" + rgba[y * width * 4:(y + 1) * width * 4] for y in range(height))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(scanlines)) + chunk(b"IEND", b""))


def animated_gif():
    header = b"GIF89a\x01\0\x01\0\x80\0\0\0\0\0\xff\xff\xff"
    loop = b"!\xff\x0bNETSCAPE2.0\x03\x01\0\0\0"
    frame = b"!\xf9\x04\0\x0a\0\0\0,\0\0\0\0\x01\0\x01\0\0\x02\x02"
    return header + loop + frame + b"D\x01\0" + frame + b"L\x01\0;"


class ImageBoards(Pasteboards):
    def data(self, data):
        return self.call(C.c_void_p, self.objc.objc_getClass(b"NSData"),
                         "dataWithBytes:length:", (C.c_char_p, C.c_size_t), (data, len(data)))

    def write_data(self, node, kind, data):
        board = self.board(node)
        self.call(C.c_size_t, board, "clearContents")
        assert self.call(C.c_bool, board, "setData:forType:", (C.c_void_p, C.c_void_p),
                         (self.data(data), self.string(kind)))

    def read_data(self, node, kind):
        data = self.call(C.c_void_p, self.board(node), "dataForType:",
                         (C.c_void_p,), (self.string(kind),))
        if not data:
            return None
        size = self.call(C.c_size_t, data, "length")
        address = self.call(C.c_void_p, data, "bytes")
        return C.string_at(address, size)

    def write_files(self, node, paths):
        urls = [self.call(C.c_void_p, self.objc.objc_getClass(b"NSURL"),
                          "fileURLWithPath:", (C.c_void_p,), (self.string(str(p.resolve())),))
                for p in paths]
        array = (C.c_void_p * len(urls))(*urls)
        objects = self.call(C.c_void_p, self.objc.objc_getClass(b"NSArray"),
                            "arrayWithObjects:count:", (C.POINTER(C.c_void_p), C.c_size_t),
                            (array, len(urls)))
        board = self.board(node)
        self.call(C.c_size_t, board, "clearContents")
        assert self.call(C.c_bool, board, "writeObjects:", (C.c_void_p,), (objects,))

    def image_file(self, node):
        raw = self.read_data(node, "public.file-url")
        if not raw:
            return None
        url = urlparse(raw.decode())
        assert url.scheme == "file"
        return Path(unquote(url.path))

    def original_matches(self, node, expected):
        path = self.image_file(node)
        return path is not None and path.is_file() and path.read_bytes() == expected


def run(cluster, boards):
    a, b, c = [cluster.start(name) for name in ("a", "b", "c")]
    for node in cluster.nodes:
        for peer in cluster.nodes:
            if node is not peer:
                cluster.send(node, {"AuthorizeKey": [peer["name"], peer["events"]["PublicKeyFingerprint"]]})
    cluster.wait(lambda: all(len(n["events"].get("AuthorizedUpdated", {})) == 2 for n in cluster.nodes),
                 "authorize complete group")
    cluster.connect_all()
    cluster.wait(lambda: cluster.all_phase("Stopped"), "initial stop")
    cluster.send(a, "StartSharing")
    cluster.wait(lambda: cluster.all_phase("Running", a["events"]["PublicKeyFingerprint"]), "primary a")
    large = png(640, 480)
    assert len(large) > 1024 * 1024
    boards.write_data(b, "public.png", large)
    cluster.wait(lambda: boards.original_matches(a, large), "large PNG original reaches current target")
    assert boards.read_data(a, "public.png").startswith(b"\x89PNG")
    assert boards.image_file(c) is None
    cluster.wait(lambda: a["events"].get("ClipboardStatus") == "Ready", "large image ready")
    print("PASS: >1 MiB PNG over chunks; original file and application PNG; unrelated board untouched", flush=True)

    def handover(node):
        cluster.send(node, "StopSharing")
        cluster.wait(lambda: cluster.all_phase("Stopped"), "confirmed stop")
        cluster.send(node, "StartSharing")
        cluster.wait(lambda: cluster.all_phase("Running", node["events"]["PublicKeyFingerprint"]), "new primary")

    handover(c)
    cluster.wait(lambda: boards.original_matches(c, large), "image survives handover")
    serials = [boards.serial(n) for n in cluster.nodes]
    deadline = time.monotonic() + 3.5
    while time.monotonic() < deadline:
        cluster.pump()
    assert [boards.serial(n) for n in cluster.nodes] == serials, "image writes caused an echo"

    gif = animated_gif()
    fixture = cluster.directory / "动画 original.gif"
    fixture.write_bytes(gif)
    boards.write_files(b, [fixture])
    cluster.wait(lambda: boards.original_matches(c, gif), "single GIF file preserved exactly")
    assert boards.image_file(c).name == fixture.name
    assert boards.read_data(c, "com.compuserve.gif") == gif
    assert boards.read_data(c, "public.png").startswith(b"\x89PNG")
    print("PASS: single GIF file keeps original bytes/name and adds application PNG; no echo after handover", flush=True)

    before = boards.serial(c)
    boards.write_files(b, [fixture, boards.image_file(a)])
    cluster.wait(lambda: c["events"].get("ClipboardStatus") == "Unsupported", "multiple files unsupported")
    assert boards.serial(c) == before, "multiple files replaced destination clipboard"

    # Screenshot-style TIFF input is normalized to PNG.
    rep = boards.call(C.c_void_p, boards.objc.objc_getClass(b"NSBitmapImageRep"),
                      "imageRepWithData:", (C.c_void_p,), (boards.data(png(3, 1)),))
    tiff = boards.call(C.c_void_p, rep, "TIFFRepresentation")
    size = boards.call(C.c_size_t, tiff, "length")
    address = boards.call(C.c_void_p, tiff, "bytes")
    boards.write_data(b, "public.tiff", C.string_at(address, size))
    cluster.wait(lambda: boards.serial(c) > before and boards.image_file(c).suffix == ".png", "TIFF normalized")
    cluster.wait(lambda: c["events"].get("ClipboardStatus") == "Ready", "TIFF ready")
    print("PASS: multi-file clipboard rejected; TIFF image content normalized to PNG", flush=True)
    if "--gtk" in sys.argv:
        cluster.gtk_smoke(c)


def main():
    if sys.platform != "darwin":
        raise SystemExit("This test requires macOS named NSPasteboards.")
    cluster = Cluster()
    boards = ImageBoards(cluster)
    try:
        run(cluster, boards)
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        boards.close()
        cluster.selector.close()
    for log in cluster.directory.glob("*/*.log"):
        assert "panicked at" not in log.read_text(errors="replace"), f"panic in {log}"
    print("PASS: clean shutdown; no daemon panic")


if __name__ == "__main__":
    main()
