"""macOS native named pasteboards + three production daemons over real TLS.

Run after cargo build --workspace: python3 tests/clipboard_session_smoke.py
Target visits in this test use primary handover (pointer stays local).
Pointer A->B->C and delayed dispatch are separately covered by Rust tests.
"""
import ctypes as C
import sys
import time

from sharing_session_smoke import Cluster


class Pasteboards:
    def __init__(self, cluster):
        C.CDLL("/System/Library/Frameworks/AppKit.framework/AppKit")
        self.objc = C.CDLL("/usr/lib/libobjc.A.dylib")
        self.objc.objc_getClass.argtypes = [C.c_char_p]
        self.objc.objc_getClass.restype = C.c_void_p
        self.objc.sel_registerName.argtypes = [C.c_char_p]
        self.objc.sel_registerName.restype = C.c_void_p
        self.address = C.cast(self.objc.objc_msgSend, C.c_void_p).value
        self.cluster = cluster
        self.pool = self.call(C.c_void_p, self.objc.objc_getClass(b"NSAutoreleasePool"), "new")

    def call(self, result, obj, method, types=(), args=()):
        fn = C.CFUNCTYPE(result, C.c_void_p, C.c_void_p, *types)(self.address)
        return fn(obj, self.objc.sel_registerName(method.encode()), *args)

    def string(self, text):
        data = text.encode()
        cls = self.objc.objc_getClass(b"NSString")
        obj = self.call(C.c_void_p, cls, "alloc")
        obj = self.call(C.c_void_p, obj, "initWithBytes:length:encoding:",
                        (C.c_char_p, C.c_size_t, C.c_size_t), (data, len(data), 4))
        return self.call(C.c_void_p, obj, "autorelease")

    def board(self, node):
        return self.call(C.c_void_p, self.objc.objc_getClass(b"NSPasteboard"),
                         "pasteboardWithName:", (C.c_void_p,),
                         (self.string(f"sharemouse-test-{self.cluster.directory.name}-{node['name']}"),))

    def write(self, node, text):
        board = self.board(node)
        self.call(C.c_size_t, board, "clearContents")
        assert self.call(C.c_bool, board, "setString:forType:", (C.c_void_p, C.c_void_p),
                         (self.string(text), self.string("public.utf8-plain-text")))

    def read(self, node):
        obj = self.call(C.c_void_p, self.board(node), "stringForType:",
                        (C.c_void_p,), (self.string("public.utf8-plain-text"),))
        if not obj:
            return None
        data = self.call(C.c_void_p, obj, "dataUsingEncoding:", (C.c_size_t,), (4,))
        size = self.call(C.c_size_t, data, "length")
        address = self.call(C.c_void_p, data, "bytes")
        return C.string_at(address, size).decode()

    def serial(self, node):
        return self.call(C.c_size_t, self.board(node), "changeCount")

    def close(self):
        for name in ("a", "b", "c"):
            self.call(None, self.board({"name": name}), "releaseGlobally")
        self.call(None, self.pool, "drain")


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
    text = "Clipboard fixture 中文 🦀\nline two"
    boards.write(b, text)
    cluster.wait(lambda: boards.read(a) == text, "native b to primary a")
    assert boards.read(c) is None, "body reached unrelated device"
    print("PASS: native follower copy reaches current primary; unrelated board untouched", flush=True)

    def handover(node):
        cluster.send(node, "StopSharing")
        cluster.wait(lambda: cluster.all_phase("Stopped"), "confirmed stop")
        cluster.send(node, "StartSharing")
        cluster.wait(lambda: cluster.all_phase("Running", node["events"]["PublicKeyFingerprint"]), "new primary")

    handover(c)
    cluster.wait(lambda: boards.read(c) == text, "latest copy survives handover")
    serials = [boards.serial(n) for n in cluster.nodes]
    deadline = time.monotonic() + 3.5  # includes a periodic retry
    while time.monotonic() < deadline:
        cluster.pump()
    assert [boards.serial(n) for n in cluster.nodes] == serials, "program writes caused an echo"
    print("PASS: handover preserves latest body; native serials stable through retry", flush=True)
    boards.write(a, text)  # same bytes, genuine new native copy
    cluster.wait(lambda: boards.serial(c) > serials[2], "same text copied again becomes a new version")
    cluster.send(a, "StopSharing")
    cluster.wait(lambda: cluster.all_phase("Stopped"), "pause")
    boards.write(b, "copy while sharing is stopped")
    deadline = time.monotonic() + 0.6
    while time.monotonic() < deadline:
        cluster.pump()
    assert boards.read(c) == text
    cluster.send(c, "StartSharing")
    cluster.wait(lambda: cluster.all_phase("Running", c["events"]["PublicKeyFingerprint"]), "resume")
    cluster.wait(lambda: boards.read(c) == "copy while sharing is stopped", "paused copy announced")
    cluster.wait(lambda: c["events"].get("ClipboardStatus") == "Ready", "ready status")
    print("PASS: repeated copy gets a new version; paused copies resume; ready visible over IPC", flush=True)
    if "--gtk" in sys.argv:
        cluster.gtk_smoke(c)


def main():
    if sys.platform != "darwin":
        raise SystemExit("This test requires macOS named NSPasteboards.")
    cluster = Cluster()
    boards = Pasteboards(cluster)
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
